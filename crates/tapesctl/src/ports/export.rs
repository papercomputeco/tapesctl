//! `tapesctl export <session-id>` — ported from `tapes export`.
//!
//! The Go command buffers the whole export with `io.ReadAll` before writing it.
//! This port streams instead: an export is one line per trace with every span's
//! payloads inlined, which for a long session is far larger than anything else
//! tapesctl holds, and there is no reason for it to pass through memory on its
//! way to a file.
//!
//! Otherwise it is deliberately the same command: same `--detail` grain, same
//! `--output`, same "write the body verbatim" contract. The export bundle is a
//! server-defined document — JSONL whose exact shape the console and the
//! importer both depend on — so a client that reformatted it would break both.

use tokio::io::AsyncWriteExt;

use snafu::{OptionExt, ResultExt};
use tapes_client::Call;

use crate::api::resolve_client;
use crate::cli::ExportArgs;
use crate::error::{Result, error};

/// The export cassette's per-session route, called directly. Export is a
/// cassette a deployment serves, not an operation of the sealed core
/// contract, so the route and the accepted `--detail` grains belong to this
/// command rather than to the shared client.
const EXPORT_SESSION_ROUTE: &str = "/v1/cassettes/export/sessions/{id}";

/// The export grains the server accepts, in the spelling the wire takes.
///
/// [`crate::error::Error::InvalidExportDetail`] spells these inline; a test
/// below holds the two lists together.
pub const DETAIL_VALUES: [&str; 2] = ["spans", "traces"];

/// Resolve a user-typed `--detail` onto the accepted set, case-folded and
/// trimmed the way this CLI has always accepted a closed set.
fn parse_detail(raw: &str) -> Option<&'static str> {
    let folded = raw.trim().to_ascii_lowercase();
    DETAIL_VALUES
        .iter()
        .find(|value| **value == folded)
        .copied()
}

/// Run one export.
pub async fn run(args: ExportArgs) -> Result<()> {
    let client = resolve_client(&args.api)?;
    let detail = match args.detail.as_deref() {
        Some(raw) => Some(parse_detail(raw).context(error::InvalidExportDetailSnafu {
            detail: raw.to_owned(),
        })?),
        None => None,
    };
    let mut call = Call {
        method: "GET",
        path: EXPORT_SESSION_ROUTE,
        path_params: vec![("id".to_owned(), args.session_id.clone())],
        ..Call::default()
    };
    if let Some(detail) = detail {
        call.query.push(("detail".to_owned(), detail.to_owned()));
    }
    let response = client.transport().execute_stream(&call).await?;

    match args.output.as_deref() {
        Some(path) => {
            let file = tokio::fs::File::create(path)
                .await
                .context(error::ExportFileSnafu {
                    path: path.to_owned(),
                })?;
            let written = stream_to(response, file).await?;
            // The progress note goes to stderr so `tapesctl export -o -` style
            // piping and shell redirection of stdout stay clean.
            eprintln!("tapesctl: wrote {written} bytes to {}", path.display());
            Ok(())
        }
        None => {
            stream_to(response, tokio::io::stdout()).await?;
            Ok(())
        }
    }
}

/// Copy the response body to `sink`, returning the byte count.
async fn stream_to<W>(mut response: reqwest::Response, mut sink: W) -> Result<u64>
where
    W: AsyncWriteExt + Unpin,
{
    let mut written = 0u64;
    while let Some(chunk) = response.chunk().await.context(error::ExportStreamSnafu)? {
        sink.write_all(&chunk)
            .await
            .context(error::ExportWriteSnafu)?;
        written = written.saturating_add(chunk.len() as u64);
    }
    // Without this an early process exit can truncate the last buffered write —
    // the failure mode is a file that looks complete and is not.
    sink.flush().await.context(error::ExportWriteSnafu)?;
    Ok(written)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::cli::ApiArgs;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn args(server: &MockServer, output: Option<std::path::PathBuf>) -> ExportArgs {
        ExportArgs {
            api: ApiArgs {
                api_url: Some(server.uri()),
            },
            session_id: "s-1".to_owned(),
            detail: None,
            output,
        }
    }

    const BUNDLE: &str = "{\"trace\":{\"trace_id\":\"t-1\"},\"spans\":[]}\n{\"trace\":{\"trace_id\":\"t-2\"},\"spans\":[]}\n";

    async fn export_server(body: &str) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/cassettes/export/sessions/s-1"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn the_bundle_is_written_to_the_output_file_verbatim() {
        // The importer and the console both parse this document; reformatting
        // it — even reserializing the JSON — would break them.
        let server = export_server(BUNDLE).await;
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("bundle.jsonl");

        run(args(&server, Some(out.clone()))).await.unwrap();

        assert_eq!(std::fs::read_to_string(&out).unwrap(), BUNDLE);
    }

    #[tokio::test]
    async fn the_detail_grain_reaches_the_server() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/cassettes/export/sessions/s-1"))
            .and(query_param("detail", "traces"))
            .respond_with(ResponseTemplate::new(200).set_body_string(BUNDLE))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();

        let mut args = args(&server, Some(dir.path().join("out.jsonl")));
        args.detail = Some("traces".to_owned());
        assert!(run(args).await.is_ok());
    }

    #[test]
    fn the_refusal_message_names_exactly_the_grains_the_server_accepts() {
        // The message spells its alternatives inline, because a user reading
        // it wants the answer and not a cross-reference. This is what keeps
        // that spelling honest: a server that grows a grain fails here rather
        // than teaching the user a stale set.
        let rendered = crate::error::error::InvalidExportDetailSnafu { detail: "x" }
            .build()
            .to_string();
        for value in DETAIL_VALUES {
            assert!(
                rendered.contains(value),
                "{value:?} missing from: {rendered}"
            );
        }
    }

    #[tokio::test]
    async fn an_unknown_detail_is_rejected_before_any_request() {
        let server = MockServer::start().await;
        let mut args = args(&server, None);
        args.detail = Some("everything".to_owned());

        assert!(run(args).await.is_err());
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_missing_session_surfaces_the_status_and_writes_no_file() {
        // A 404 body written into the output file would be a bundle-shaped lie.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/cassettes/export/sessions/s-1"))
            .respond_with(ResponseTemplate::new(404).set_body_string(r#"{"error":"not found"}"#))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("bundle.jsonl");

        let err = run(args(&server, Some(out.clone()))).await.unwrap_err();

        assert!(format!("{err}").contains("404"), "got: {err}");
        assert!(
            !out.exists(),
            "no file should be created for a failed export"
        );
    }

    #[tokio::test]
    async fn an_unwritable_output_path_is_an_error_rather_than_a_silent_drop() {
        let server = export_server(BUNDLE).await;
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("no-such-dir").join("bundle.jsonl");

        assert!(run(args(&server, Some(out))).await.is_err());
    }
}

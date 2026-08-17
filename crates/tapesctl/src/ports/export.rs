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
use tapes_client::core::models::ExportSessionParams;

use crate::api::client::parse_grain;
use crate::api::resolve_client;
use crate::cli::ExportArgs;
use crate::error::{Result, error};

/// Run one export.
pub async fn run(args: ExportArgs) -> Result<()> {
    let client = resolve_client(&args.api)?;
    let detail = match args.detail.as_deref() {
        Some(raw) => parse_grain(raw)
            .map(Some)
            .context(error::InvalidExportDetailSnafu {
                detail: raw.to_owned(),
            })?,
        None => None,
    };
    let response = client
        .export_session(&args.session_id, &ExportSessionParams { detail })
        .await?;

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
                tapes_url: Some(server.uri()),
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

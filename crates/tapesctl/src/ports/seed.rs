//! `tapesctl seed` — ported from `tapes seed`.
//!
//! Populates a server with demo sessions so a fresh console has something to
//! render. One `POST /v1/admin/seed/demo` with an empty object; the server owns
//! the fixtures.
//!
//! Two flags of the Go command are deliberately not carried over. `--demo` was
//! parsed and then ignored — the command always seeded demo data — and
//! `--overwrite` is refused by the server, so a client-side flag for it could
//! only ever produce a remote error. Porting either would be reproducing a bug
//! for symmetry's sake.
//!
//! Seeding writes to the server's single-tenant org. It is an admin route, not
//! something to point at a populated deployment.

use crate::api::resolve_client;
use crate::cli::SeedArgs;
use crate::error::Result;

/// Run one seed.
pub async fn run(args: SeedArgs) -> Result<()> {
    let client = resolve_client(&args.api)?;
    let result = client.seed_demo().await?;
    println!("{}", render(&result, client.base().as_str()));
    Ok(())
}

/// One human line from the server's seed result.
///
/// Every count is read defensively: the server owns this shape, and a summary
/// line is not worth failing a successful seed over.
#[must_use]
pub fn render(result: &serde_json::Value, target: &str) -> String {
    let count = |key: &str| result.get(key).and_then(serde_json::Value::as_i64);
    let sessions = count("sessions").unwrap_or_default();
    let raw_turns = count("raw_turns").unwrap_or_default();
    let inserted = count("raw_turns_inserted").unwrap_or_default();
    let deduped = count("raw_turns_deduped").unwrap_or_default();
    format!(
        "tapesctl: seeded {sessions} session(s) ({raw_turns} raw turns: {inserted} inserted, \
         {deduped} deduped) into {target}",
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::cli::ApiArgs;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn the_summary_reports_every_count_the_server_returned() {
        let rendered = render(
            &json!({
                "sessions": 3,
                "raw_turns": 12,
                "raw_turns_inserted": 10,
                "raw_turns_deduped": 2,
            }),
            "http://127.0.0.1:8081/",
        );
        assert!(rendered.contains("3 session(s)"), "got: {rendered}");
        assert!(rendered.contains("10 inserted"), "got: {rendered}");
        assert!(rendered.contains("2 deduped"), "got: {rendered}");
    }

    #[test]
    fn a_response_missing_a_count_still_renders() {
        // The summary is a courtesy; a server that trims a field must not turn a
        // successful seed into a failure.
        let rendered = render(&json!({"sessions": 1}), "http://x/");
        assert!(rendered.contains("1 session(s)"), "got: {rendered}");
        assert!(rendered.contains("0 inserted"), "got: {rendered}");
    }

    #[tokio::test]
    async fn seeding_posts_to_the_admin_route() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/admin/seed/demo"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"sessions":2,"raw_turns":6,"raw_turns_inserted":6,"raw_turns_deduped":0}"#,
            ))
            .mount(&server)
            .await;

        let result = run(SeedArgs {
            api: ApiArgs {
                tapes_url: Some(server.uri()),
            },
        })
        .await;

        assert!(result.is_ok(), "got: {result:?}");
    }

    #[tokio::test]
    async fn a_server_without_the_raw_layer_surfaces_its_refusal() {
        // The route answers 501 when the driver does not host raw turns; a
        // silent success would leave the user staring at an empty console.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/admin/seed/demo"))
            .respond_with(
                ResponseTemplate::new(501)
                    .set_body_string(r#"{"error":"seeding requires the raw-turn layer"}"#),
            )
            .mount(&server)
            .await;

        let err = run(SeedArgs {
            api: ApiArgs {
                tapes_url: Some(server.uri()),
            },
        })
        .await
        .unwrap_err();

        assert!(format!("{err}").contains("501"), "got: {err}");
    }
}

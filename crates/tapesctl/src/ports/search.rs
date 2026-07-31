//! `tapesctl search <query>` — ported from `tapes search`.
//!
//! Span search is the only mode. The Go command still carried a `--spans` flag
//! that had been reduced to a no-op and marked deprecated; a flag that is
//! parsed and ignored is an artifact, not a contract, so it is dropped here
//! rather than reproduced.
//!
//! What *is* reproduced is the layout, because `--quiet` is a pipe format:
//! `tapesctl skill generate $(tapesctl search "charm CLI" -q -k 1)` depends on
//! one bare session id per line, deduplicated in score order.
//!
//! Two other drops. The Go renderer coloured each field through the CLI's
//! lipgloss styles; tapesctl has no style layer, so the same columns are
//! printed plain. And the command reported a result count to product
//! telemetry, which tapesctl does not have.

use serde_json::Value;
use time::OffsetDateTime;
use time::UtcOffset;
use time::format_description::well_known::Rfc3339;

use crate::api::resolve_client;
use crate::cli::SearchArgs;
use crate::error::Result;

/// Longest turn preview before it is elided.
const PROMPT_WIDTH: usize = 80;

/// Longest snippet before it is elided.
const SNIPPET_WIDTH: usize = 100;

/// Run one search.
pub async fn run(args: SearchArgs) -> Result<()> {
    let client = resolve_client(&args.api)?;
    let output = client.search_spans(&args.query, args.top).await?;

    let results = output
        .get("results")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    if results.is_empty() {
        if !args.quiet {
            println!("No results found.");
        }
        return Ok(());
    }

    if args.quiet {
        for session_id in session_ids(&results) {
            println!("{session_id}");
        }
        return Ok(());
    }

    let echoed = output
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or(&args.query);
    println!("\nSpan Search Results for: {echoed:?}\n");
    for (index, hit) in results.iter().enumerate() {
        print_hit(index + 1, hit);
    }
    Ok(())
}

/// Session ids of the hits, deduplicated, in score order.
///
/// Order is the server's — the response is already ranked — so the first
/// occurrence of each session wins and the highest-scoring session is first.
#[must_use]
pub fn session_ids(results: &[Value]) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for hit in results {
        let Some(id) = hit.get("session_id").and_then(Value::as_str) else {
            continue;
        };
        if id.is_empty() || seen.iter().any(|known| known == id) {
            continue;
        }
        seen.push(id.to_owned());
    }
    seen
}

/// Render one hit.
fn print_hit(rank: usize, hit: &Value) {
    let text = |key: &str| hit.get(key).and_then(Value::as_str).unwrap_or_default();
    let score = hit.get("score").and_then(Value::as_f64).unwrap_or_default();

    println!(
        "  #{rank}  score: {score:.4}  {}/{}",
        text("trace_id"),
        text("span_id"),
    );

    // An empty prompt is a synthetic turn, not a missing field — the server
    // sends `user_prompt` even when it is blank, precisely so this case is
    // distinguishable.
    let prompt = text("user_prompt").replace('\n', " ");
    let prompt = if prompt.is_empty() {
        "(synthetic turn)".to_owned()
    } else {
        elide(&prompt, PROMPT_WIDTH)
    };
    println!("  turn: {prompt}");

    let snippet = text("snippet").replace('\n', " ");
    if !snippet.is_empty() {
        println!("   ├─ {}", elide(&snippet, SNIPPET_WIDTH));
    }

    let mut meta = format_started_at(text("started_at"));
    let session_id = text("session_id");
    if !session_id.is_empty() {
        meta.push_str(&format!("  session {session_id}"));
    }
    println!("  {meta}\n");
}

/// Truncate to `width`, marking the cut with an ellipsis.
///
/// Counted in characters rather than bytes. The Go original sliced bytes, which
/// splits a multi-byte rune in half — harmless there because Go tolerates
/// invalid UTF-8 in a string, but not something Rust can reproduce without
/// panicking on the same input. A prompt with an accented character is now cut
/// cleanly instead of mangled.
#[must_use]
fn elide(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        return value.to_owned();
    }
    let kept: String = value.chars().take(width.saturating_sub(3)).collect();
    format!("{kept}...")
}

/// Render a hit's timestamp at second precision, as the Go layout did.
///
/// A value that will not parse is printed as it arrived: it is the server's
/// field, and showing it beats showing nothing.
#[must_use]
fn format_started_at(raw: &str) -> String {
    let Ok(parsed) = OffsetDateTime::parse(raw, &Rfc3339) else {
        return raw.to_owned();
    };
    let utc = parsed.to_offset(UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        utc.year(),
        u8::from(utc.month()),
        utc.day(),
        utc.hour(),
        utc.minute(),
        utc.second(),
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::cli::ApiArgs;
    use serde_json::json;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn args(server: &MockServer, quiet: bool) -> SearchArgs {
        SearchArgs {
            api: ApiArgs {
                tapes_url: Some(server.uri()),
            },
            query: "charm CLI".to_owned(),
            top: 5,
            quiet,
        }
    }

    #[test]
    fn quiet_output_is_deduplicated_in_score_order() {
        // This is the pipe contract `skill generate` consumes.
        let results = vec![
            json!({"session_id": "s-a"}),
            json!({"session_id": "s-b"}),
            json!({"session_id": "s-a"}),
            json!({"session_id": ""}),
            json!({"trace_id": "no session at all"}),
            json!({"session_id": "s-c"}),
        ];
        assert_eq!(session_ids(&results), vec!["s-a", "s-b", "s-c"]);
    }

    #[test]
    fn a_long_value_is_elided_at_the_documented_width() {
        let long = "x".repeat(200);
        let elided = elide(&long, PROMPT_WIDTH);
        assert_eq!(elided.chars().count(), PROMPT_WIDTH);
        assert!(elided.ends_with("..."));
    }

    #[test]
    fn a_value_at_the_width_is_left_alone() {
        let exact = "y".repeat(PROMPT_WIDTH);
        assert_eq!(elide(&exact, PROMPT_WIDTH), exact);
    }

    #[test]
    fn eliding_never_splits_a_multibyte_character() {
        // The Go original sliced bytes here; the same input would have cut a
        // rune in half.
        let accented = "é".repeat(200);
        let elided = elide(&accented, PROMPT_WIDTH);
        assert_eq!(elided.chars().count(), PROMPT_WIDTH);
        assert!(elided.starts_with('é'));
    }

    #[test]
    fn timestamps_print_at_second_precision() {
        assert_eq!(
            format_started_at("2026-07-31T12:34:56.123456789Z"),
            "2026-07-31T12:34:56Z",
        );
        assert_eq!(
            format_started_at("2026-07-31T05:34:56-07:00"),
            "2026-07-31T12:34:56Z",
        );
    }

    #[test]
    fn an_unparseable_timestamp_is_shown_rather_than_swallowed() {
        assert_eq!(format_started_at("not a time"), "not a time");
    }

    async fn search_server(body: Value) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/search/spans"))
            .and(query_param("query", "charm CLI"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn a_search_renders_its_hits() {
        let server = search_server(json!({
            "query": "charm CLI",
            "count": 1,
            "results": [{
                "trace_id": "t-1",
                "span_id": "sp-1",
                "session_id": "s-1",
                "score": 0.9312,
                "user_prompt": "how do I use gum",
                "snippet": "gum glow",
                "started_at": "2026-07-31T12:00:00Z",
            }],
        }))
        .await;

        assert!(run(args(&server, false)).await.is_ok());
    }

    #[tokio::test]
    async fn an_empty_result_set_is_not_an_error() {
        let server = search_server(json!({"query": "charm CLI", "count": 0, "results": []})).await;
        assert!(run(args(&server, true)).await.is_ok());
        assert!(run(args(&server, false)).await.is_ok());
    }

    #[tokio::test]
    async fn the_result_count_reaches_the_server_as_top_k() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/search/spans"))
            .and(query_param("top_k", "3"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"results": []})))
            .mount(&server)
            .await;

        let mut args = args(&server, true);
        args.top = 3;
        assert!(run(args).await.is_ok());
    }

    #[tokio::test]
    async fn a_search_without_a_server_fails_on_the_missing_url() {
        let result = run(SearchArgs {
            api: ApiArgs { tapes_url: None },
            query: "x".to_owned(),
            top: 5,
            quiet: false,
        })
        .await;
        assert!(matches!(result, Err(crate::Error::MissingTapesUrl)));
    }
}

//! `tapesctl search <query>` — ported from `tapes search`.
//!
//! Span search is the only mode. The Go command still carried a `--spans` flag
//! that had been reduced to a no-op and marked deprecated; a flag that is
//! parsed and ignored is an artifact, not a contract, so it is dropped here
//! rather than reproduced.
//!
//! What *is* reproduced is the pipe format: `--quiet` prints one bare session
//! id per line, deduplicated in score order.

use serde::Deserialize;
use tapes_client::Call;
use time::OffsetDateTime;

use crate::api::client::narrow;
use crate::api::{print_json, resolve_client};
use crate::cli::SearchArgs;
use crate::error::Result;
use crate::render::text::{elide, one_line, relative, sanitize};
use crate::render::{Theme, Tone};

/// The search cassette's span route, called directly. Search is a cassette a
/// deployment serves, not an operation of the sealed core contract, so the
/// route and the response shape below belong to this command rather than to
/// the shared client.
const SEARCH_SPANS_ROUTE: &str = "/v1/cassettes/search/spans";

/// The span search response, decoded here because the shape is the search
/// cassette's own. Only the fields this renderer reads are named; anything
/// else the server says is ignored rather than fatal, so an additive change
/// cannot blank a page of results.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct SpanSearchOutput {
    /// The query the server ran, echoed back.
    pub query: String,
    /// The ranked hits. An explicit `null` decodes as empty, like an absent
    /// key.
    #[serde(deserialize_with = "null_default")]
    pub results: Vec<SpanSearchResult>,
}

/// One span hit with its trace/turn context.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct SpanSearchResult {
    /// The hit's relevance score.
    pub score: f32,
    /// The session the span belongs to.
    pub session_id: String,
    /// Preview of the matched span's delta-only text.
    pub snippet: String,
    /// The span's id.
    pub span_id: String,
    /// The span's start, an RFC 3339 timestamp.
    pub started_at: String,
    /// The trace the span belongs to.
    pub trace_id: String,
    /// The prompt of the turn the span belongs to. The server sends it even
    /// when blank, so a synthetic turn's empty prompt is distinguishable from
    /// a missing field.
    pub user_prompt: String,
}

/// Decode an explicit `null` as the type's default, like an absent key.
fn null_default<'de, D, T>(deserializer: D) -> std::result::Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// Run one search.
pub async fn run(args: SearchArgs) -> Result<()> {
    let client = resolve_client(&args.api)?;
    let value = client
        .transport()
        .execute(&Call {
            method: "GET",
            path: SEARCH_SPANS_ROUTE,
            query: vec![
                ("query".to_owned(), args.query.clone()),
                // Always sent, unlike a listing's omit-when-unset rule: the
                // flag carries the default, so this client always has a value,
                // and one request spelling is better than two.
                ("top_k".to_owned(), narrow(args.top).to_string()),
            ],
            ..Call::default()
        })
        .await?;
    if args.json {
        return print_json(&value);
    }
    let output: SpanSearchOutput = tapes_client::decode::typed(value)?;

    if args.quiet {
        for session_id in session_ids(&output.results) {
            println!("{session_id}");
        }
        return Ok(());
    }

    // The server echoes the query it ran; a response without one falls back to
    // what was asked rather than printing an empty pair of quotes.
    let echoed = if output.query.is_empty() {
        &args.query
    } else {
        &output.query
    };
    print!(
        "{}",
        render(
            echoed,
            &output.results,
            &Theme::detect(),
            OffsetDateTime::now_utc()
        )
    );
    Ok(())
}

#[must_use]
pub fn render(
    query: &str,
    results: &[SpanSearchResult],
    theme: &Theme,
    now: OffsetDateTime,
) -> String {
    let mut out = String::new();
    if results.is_empty() {
        out.push_str("No results found.\n");
        return out;
    }
    let sessions = session_ids(results).len();
    let hits = if results.len() == 1 { "hit" } else { "hits" };
    let across = if sessions == 1 { "session" } else { "sessions" };
    out.push_str(&theme.paint(Tone::Command, &format!("{:?}", sanitize(query))));
    out.push_str(&theme.paint(
        Tone::Secondary,
        &format!("  ·  {} {hits} across {sessions} {across}", results.len()),
    ));
    out.push_str("\n\n");

    // score(4) + 2 + prompt + 2 + when(8)
    let fixed = 4 + 2 + 2 + 8;
    let prompt_width = theme.width.saturating_sub(fixed).clamp(16, 80);
    for hit in results {
        let prompt = one_line(&hit.user_prompt);
        let prompt = if prompt.is_empty() && !hit.session_id.is_empty() {
            "(synthetic turn)".to_owned()
        } else if prompt.is_empty() {
            theme.absent().to_owned()
        } else {
            prompt
        };
        let when = relative(&hit.started_at, now);
        out.push_str(&theme.paint(Tone::Number, &format!("{:.2}", hit.score)));
        out.push_str("  ");
        out.push_str(&theme.paint(
            Tone::Primary,
            &format!("{:<prompt_width$}", elide(&prompt, prompt_width)),
        ));
        out.push_str("  ");
        out.push_str(&theme.paint(Tone::Secondary, &when));
        out.push('\n');

        let snippet = one_line(&hit.snippet);
        if !snippet.is_empty() {
            let width = theme.width.saturating_sub(8).max(20);
            out.push_str("      ");
            out.push_str(&theme.paint(Tone::Secondary, &format!("» {}", elide(&snippet, width))));
            out.push('\n');
        }

        let ids: Vec<String> = [
            ("session", &hit.session_id),
            ("trace", &hit.trace_id),
            ("span", &hit.span_id),
        ]
        .iter()
        .filter(|(_, id)| !id.is_empty())
        .map(|(name, id)| format!("{name} {}", sanitize(id)))
        .collect();
        // Ids are never elided; they wrap between whole `name id` pieces.
        let indent = "      ";
        let mut line = String::new();
        for piece in ids {
            let joined = if line.is_empty() {
                piece.clone()
            } else {
                format!("{line} · {piece}")
            };
            if !line.is_empty() && indent.len() + joined.chars().count() > theme.width {
                out.push_str(indent);
                out.push_str(&theme.paint(Tone::Secondary, &line));
                out.push('\n');
                line = piece;
            } else {
                line = joined;
            }
        }
        if !line.is_empty() {
            out.push_str(indent);
            out.push_str(&theme.paint(Tone::Secondary, &line));
            out.push('\n');
        }
    }
    out
}

/// Session ids of the hits, deduplicated, in score order.
///
/// Order is the server's — the response is already ranked — so the first
/// occurrence of each session wins and the highest-scoring session is first.
#[must_use]
pub fn session_ids(results: &[SpanSearchResult]) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for hit in results {
        let id = &hit.session_id;
        if id.is_empty() || seen.iter().any(|known| known == id) {
            continue;
        }
        seen.push(id.clone());
    }
    seen
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::cli::ApiArgs;
    use serde_json::{Value, json};
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn args(server: &MockServer, quiet: bool) -> SearchArgs {
        SearchArgs {
            api: ApiArgs {
                api_url: Some(server.uri()),
            },
            query: "charm CLI".to_owned(),
            top: 5,
            quiet,
            json: false,
        }
    }

    #[test]
    fn quiet_output_is_deduplicated_in_score_order() {
        // This is the pipe contract shell substitutions consume.
        // Decoded rather than constructed: a response model is
        // `#[non_exhaustive]`, which is the shipped shape saying the server
        // owns it.
        let results: Vec<SpanSearchResult> = serde_json::from_value(json!([
            {"session_id": "s-a"},
            {"session_id": "s-b"},
            {"session_id": "s-a"},
            {"session_id": ""},
            {"trace_id": "no session at all"},
            {"session_id": "s-c"},
        ]))
        .unwrap();
        assert_eq!(session_ids(&results), vec!["s-a", "s-b", "s-c"]);
    }

    #[test]
    fn the_human_view_is_a_ranked_list_with_snippets() {
        use time::macros::datetime;
        let results: Vec<SpanSearchResult> = serde_json::from_value(json!([
            {
                "score": 0.8231,
                "session_id": "01a0d365-1a2b-77a1-8473-bd2e295244a4",
                "trace_id": "t-1",
                "span_id": "sp-1",
                "user_prompt": "Fix WorkOS redirect\non staging",
                "snippet": "the redirect URI in the WorkOS dashboard is per-environment",
                "started_at": "2026-09-17T10:00:00Z"
            },
            {
                "score": 0.77,
                "session_id": "01a0d365-9c3d-77a1-8473-bd2e295244a4",
                "user_prompt": "",
                "snippet": "",
                "started_at": "2026-09-12T10:00:00Z"
            }
        ]))
        .unwrap();
        let now = datetime!(2026-09-26 12:00 UTC);
        let rendered = render(
            "how I fixed auth",
            &results,
            &crate::render::Theme::plain(100),
            now,
        );
        assert_eq!(
            rendered,
            "\"how I fixed auth\"  ·  2 hits across 2 sessions\n\
             \n\
             0.82  Fix WorkOS redirect on staging                                                    Sep 17\n\
             \x20     » the redirect URI in the WorkOS dashboard is per-environment\n\
             \x20     session 01a0d365-1a2b-77a1-8473-bd2e295244a4 · trace t-1 · span sp-1\n\
             0.77  (synthetic turn)                                                                  Sep 12\n\
             \x20     session 01a0d365-9c3d-77a1-8473-bd2e295244a4\n",
            "got:\n{rendered}"
        );
        assert_eq!(
            render("q", &[], &crate::render::Theme::plain(100), now),
            "No results found.\n"
        );
    }

    async fn search_server(body: Value) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/cassettes/search/spans"))
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
                // A field this build has never heard of must be ignored
                // rather than fatal.
                "a_field_from_the_future": 7,
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
            .and(path("/v1/cassettes/search/spans"))
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
            api: ApiArgs { api_url: None },
            query: "x".to_owned(),
            top: 5,
            quiet: false,
            json: false,
        })
        .await;
        assert!(matches!(result, Err(crate::Error::MissingTapesUrl)));
    }
}

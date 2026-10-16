//! `tapesctl search <query>` — ported from `tapes search`.
//!
//! Span search is the only mode. The Go command still carried a `--spans` flag
//! that had been reduced to a no-op and marked deprecated; a flag that is
//! parsed and ignored is an artifact, not a contract, so it is dropped here
//! rather than reproduced.
//!
//! What *is* reproduced is the pipe format: `--quiet` prints one bare session
//! id per line, deduplicated in score order.
//!
//! The route, the query, and the hit model are the shared client's
//! ([`tapes_client::search_spans`]), so a field the search cassette adds
//! reaches `--json` without a release here. What is this command's own is the
//! flag grammar — `me`, relative time bounds — and the human view.

use snafu::ResultExt;
use tapes_client::{SearchQuery, SpanSearchHit, search_spans};
use time::OffsetDateTime;

use crate::api::client::narrow;
use crate::api::resolve_client;
use crate::cli::SearchArgs;
use crate::error::{Result, error};
use crate::recall::{own_subject, resolve_subject, resolve_time};
use crate::render::text::{elide, one_line, relative, sanitize, tilde};
use crate::render::{Theme, Tone};

/// Build the search the flags describe, resolving `me` with `own` and
/// relative time bounds against `now`.
pub fn query_for(
    args: &SearchArgs,
    own: impl FnOnce() -> String,
    now: OffsetDateTime,
) -> Result<SearchQuery> {
    let blank_is_none = |value: &Option<String>| {
        value
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    Ok(SearchQuery {
        query: args.query.clone(),
        // Always sent, unlike a listing's omit-when-unset rule: the flag
        // carries the default, so this client always has a value, and one
        // request spelling is better than two.
        top_k: Some(narrow(args.top)),
        subject: resolve_subject(args.subject.clone(), own),
        cwd: blank_is_none(&args.cwd),
        harness: blank_is_none(&args.harness),
        since: resolve_time("--since", args.since.clone(), now)?,
        until: resolve_time("--until", args.until.clone(), now)?,
        // A switch, so only ever `true` on the wire: "not live" is not a
        // filter anybody asked for by leaving the flag off.
        live: args.live.then_some(true),
        ..SearchQuery::default()
    })
}

/// Run one search. `verbose` is the global `-v` count; any `-v` prints each
/// hit's whole prompt.
pub async fn run(args: SearchArgs, verbose: u8) -> Result<()> {
    let client = resolve_client(&args.api)?;
    let query = query_for(&args, own_subject, OffsetDateTime::now_utc())?;
    let response = search_spans(client.transport(), &query).await?;

    if args.json {
        return response
            .into_envelope(&query)
            .print_json()
            .context(error::RenderSnafu);
    }

    if args.quiet {
        for session_id in response.session_ids() {
            println!("{session_id}");
        }
        return Ok(());
    }

    // The server echoes the query it ran; a response without one falls back to
    // what was asked rather than printing an empty pair of quotes.
    let echoed = if response.query.is_empty() {
        args.query.clone()
    } else {
        response.query.clone()
    };
    print!(
        "{}",
        render(
            &echoed,
            &response.results,
            verbose > 0,
            &Theme::detect(),
            OffsetDateTime::now_utc()
        )
    );
    Ok(())
}

/// Who a hit belongs to, for a person reading it: the name, else the email,
/// else the raw subject.
#[must_use]
pub fn owner(hit: &SpanSearchHit) -> String {
    [
        &hit.auth_subject_name,
        &hit.auth_subject_email,
        &hit.auth_subject,
    ]
    .into_iter()
    .map(|value| one_line(value))
    .find(|value| !value.is_empty())
    .unwrap_or_default()
}

/// The ranked list. `full_prompt` prints each hit's prompt whole instead of
/// elided to one line.
#[must_use]
pub fn render(
    query: &str,
    results: &[SpanSearchHit],
    full_prompt: bool,
    theme: &Theme,
    now: OffsetDateTime,
) -> String {
    let mut out = String::new();
    if results.is_empty() {
        out.push_str("No results found.\n");
        return out;
    }
    let sessions = session_ids(results);
    let hits = if results.len() == 1 { "hit" } else { "hits" };
    let across = if sessions.len() == 1 {
        "session"
    } else {
        "sessions"
    };
    out.push_str(&theme.paint(Tone::Command, &format!("{:?}", sanitize(query))));
    out.push_str(&theme.paint(
        Tone::Secondary,
        &format!(
            "  ·  {} {hits} across {} {across}",
            results.len(),
            sessions.len()
        ),
    ));
    out.push_str("\n\n");

    let indent = "      ";
    let body_width = theme.width.saturating_sub(indent.len()).max(20);
    for hit in results {
        // score(4) + 2 + head + 2 + when(8)
        let when = relative(&hit.started_at, now);
        let head_width = theme.width.saturating_sub(4 + 2 + 2 + 8).clamp(16, 100);
        let mut head: Vec<String> = Vec::new();
        let who = owner(hit);
        head.push(if who.is_empty() {
            theme.absent().to_owned()
        } else {
            who
        });
        let title = one_line(&hit.display_title);
        if !title.is_empty() {
            head.push(title);
        }
        let head = elide(&head.join(" · "), head_width);
        out.push_str(&theme.paint(Tone::Number, &format!("{:.2}", hit.score)));
        out.push_str("  ");
        out.push_str(&theme.paint(Tone::Primary, &head));
        if hit.live {
            out.push_str("  ");
            out.push_str(&theme.paint(Tone::Command, "LIVE"));
        }
        out.push_str("  ");
        out.push_str(&theme.paint(Tone::Secondary, &when));
        out.push('\n');

        let prompt = if full_prompt {
            sanitize_block(&hit.user_prompt)
        } else {
            elide(&one_line(&hit.user_prompt), body_width.saturating_sub(2))
        };
        let prompt = if prompt.trim().is_empty() && !hit.session_id.is_empty() {
            "(synthetic turn)".to_owned()
        } else {
            prompt
        };
        for (n, line) in prompt.lines().enumerate() {
            out.push_str(indent);
            out.push_str(if n == 0 { "› " } else { "  " });
            out.push_str(&theme.paint(Tone::Primary, line));
            out.push('\n');
        }

        let snippet = one_line(&hit.snippet);
        if !snippet.is_empty() {
            out.push_str(indent);
            out.push_str(&theme.paint(
                Tone::Secondary,
                &format!("» {}", elide(&snippet, body_width.saturating_sub(2))),
            ));
            out.push('\n');
        }

        let cwd = tilde(&hit.cwd);
        let harness = sanitize(&hit.harness_id);
        let pieces: Vec<String> = [
            (None, cwd),
            (None, harness),
            (Some("session"), sanitize(&hit.session_id)),
            (Some("trace"), sanitize(&hit.trace_id)),
        ]
        .into_iter()
        .filter(|(_, value)| !value.is_empty())
        .map(|(name, value)| match name {
            Some(name) => format!("{name} {value}"),
            None => value,
        })
        .collect();
        // Ids are never elided; the line wraps between whole pieces.
        let mut line = String::new();
        for piece in pieces {
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
    if let Some(first) = sessions.first() {
        out.push('\n');
        out.push_str(&theme.paint(
            Tone::Secondary,
            &format!("read one  tapesctl sessions transcript {}", sanitize(first)),
        ));
        out.push('\n');
    }
    out
}

/// Sanitize a multi-line value line by line, keeping its line breaks.
fn sanitize_block(value: &str) -> String {
    value.lines().map(sanitize).collect::<Vec<_>>().join("\n")
}

/// Session ids of the hits, deduplicated, in score order.
///
/// Order is the server's — the response is already ranked — so the first
/// occurrence of each session wins and the highest-scoring session is first.
#[must_use]
pub fn session_ids(results: &[SpanSearchHit]) -> Vec<String> {
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
    use time::macros::datetime;
    use wiremock::matchers::{method, path, query_param, query_param_is_missing};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn bare(url: Option<String>) -> SearchArgs {
        SearchArgs {
            api: ApiArgs { api_url: url },
            query: "charm CLI".to_owned(),
            top: 5,
            subject: None,
            cwd: None,
            harness: None,
            since: None,
            until: None,
            live: false,
            quiet: false,
            json: false,
        }
    }

    fn args(server: &MockServer, quiet: bool) -> SearchArgs {
        SearchArgs {
            quiet,
            ..bare(Some(server.uri()))
        }
    }

    fn hits(value: Value) -> Vec<SpanSearchHit> {
        // Decoded rather than constructed: the hit model is
        // `#[non_exhaustive]`, which is the shipped shape saying the server
        // owns it.
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn quiet_output_is_deduplicated_in_score_order() {
        // This is the pipe contract shell substitutions consume.
        let results = hits(json!([
            {"session_id": "s-a"},
            {"session_id": "s-b"},
            {"session_id": "s-a"},
            {"session_id": ""},
            {"trace_id": "no session at all"},
            {"session_id": "s-c"},
        ]));
        assert_eq!(session_ids(&results), vec!["s-a", "s-b", "s-c"]);
    }

    #[test]
    fn every_flag_becomes_its_filter_with_me_and_durations_resolved() {
        let now = datetime!(2026-10-15 12:00 UTC);
        let args = SearchArgs {
            subject: Some("me".into()),
            cwd: Some("/src/app".into()),
            harness: Some("claude".into()),
            since: Some("7d".into()),
            until: Some("2026-10-15T00:00:00Z".into()),
            live: true,
            top: 3,
            ..bare(None)
        };
        let query = query_for(&args, || "local:ada".into(), now).unwrap();
        assert_eq!(
            query.pairs(),
            vec![
                ("query".to_owned(), "charm CLI".to_owned()),
                ("top_k".to_owned(), "3".to_owned()),
                ("subject".to_owned(), "local:ada".to_owned()),
                ("cwd".to_owned(), "/src/app".to_owned()),
                ("harness".to_owned(), "claude".to_owned()),
                ("since".to_owned(), "2026-10-08T12:00:00Z".to_owned()),
                ("until".to_owned(), "2026-10-15T00:00:00Z".to_owned()),
                ("live".to_owned(), "true".to_owned()),
            ]
        );
    }

    #[test]
    fn unset_flags_send_nothing_but_the_query_and_count() {
        let query = query_for(
            &bare(None),
            || panic!("me was not asked for"),
            datetime!(2026-10-15 12:00 UTC),
        )
        .unwrap();
        assert_eq!(
            query.pairs(),
            vec![
                ("query".to_owned(), "charm CLI".to_owned()),
                ("top_k".to_owned(), "5".to_owned()),
            ]
        );
    }

    fn rich_hits() -> Vec<SpanSearchHit> {
        hits(json!([
            {
                "score": 0.8231,
                "session_id": "01a0d365-1a2b-77a1-8473-bd2e295244a4",
                "trace_id": "t-1",
                "span_id": "sp-1",
                "user_prompt": "Fix OAuth redirect\non staging",
                "snippet": "the redirect URI is per-environment",
                "started_at": "2026-09-17T10:00:00Z",
                "auth_subject": "user_01",
                "auth_subject_name": "Ada Lovelace",
                "auth_subject_email": "ada@example.com",
                "display_title": "Auth fix",
                "cwd": "/src/app",
                "harness_id": "claude",
                "live": true
            },
            {
                "score": 0.77,
                "session_id": "01a0d365-9c3d-77a1-8473-bd2e295244a4",
                "user_prompt": "",
                "snippet": "",
                "started_at": "2026-09-12T10:00:00Z",
                "auth_subject": "local:bob"
            }
        ]))
    }

    #[test]
    fn the_human_view_names_the_owner_title_and_liveness() {
        let now = datetime!(2026-09-26 12:00 UTC);
        let rendered = render(
            "how I fixed auth",
            &rich_hits(),
            false,
            &crate::render::Theme::plain(100),
            now,
        );
        assert_eq!(
            rendered,
            "\"how I fixed auth\"  ·  2 hits across 2 sessions\n\
             \n\
             0.82  Ada Lovelace · Auth fix  LIVE  Sep 17\n\
             \x20     › Fix OAuth redirect on staging\n\
             \x20     » the redirect URI is per-environment\n\
             \x20     /src/app · claude · session 01a0d365-1a2b-77a1-8473-bd2e295244a4 · trace t-1\n\
             0.77  local:bob  Sep 12\n\
             \x20     › (synthetic turn)\n\
             \x20     session 01a0d365-9c3d-77a1-8473-bd2e295244a4\n\
             \n\
             read one  tapesctl sessions transcript 01a0d365-1a2b-77a1-8473-bd2e295244a4\n",
            "got:\n{rendered}"
        );
        assert_eq!(
            render("q", &[], false, &crate::render::Theme::plain(100), now),
            "No results found.\n"
        );
    }

    #[test]
    fn verbose_prints_the_whole_prompt_and_short_elides_it() {
        let now = datetime!(2026-09-26 12:00 UTC);
        let long = format!("first line\nsecond {}", "word ".repeat(40));
        let results = hits(json!([{"session_id": "s", "user_prompt": long}]));
        let theme = crate::render::Theme::plain(60);
        let short = render("q", &results, false, &theme, now);
        assert!(short.contains("› first line second word"), "{short}");
        assert!(short.contains('…'), "{short}");
        let full = render("q", &results, true, &theme, now);
        assert!(full.contains("› first line\n"), "{full}");
        assert!(full.contains("  second word"), "{full}");
        assert!(!full.contains('…'), "{full}");
    }

    #[test]
    fn the_owner_falls_back_from_name_to_email_to_subject() {
        let [named, mailed, bare, nobody]: [SpanSearchHit; 4] = hits(json!([
            {"auth_subject_name": "Ada", "auth_subject_email": "a@x", "auth_subject": "u"},
            {"auth_subject_email": "a@x", "auth_subject": "u"},
            {"auth_subject": "u"},
            {}
        ]))
        .try_into()
        .unwrap();
        assert_eq!(owner(&named), "Ada");
        assert_eq!(owner(&mailed), "a@x");
        assert_eq!(owner(&bare), "u");
        assert_eq!(owner(&nobody), "");
    }

    #[test]
    fn the_json_shape_is_the_shared_envelope() {
        let response: tapes_client::SpanSearchResponse = serde_json::from_value(json!({
            "query": "charm CLI", "count": 1,
            "results": [{"session_id": "s-1", "a_field_from_the_future": 7}]
        }))
        .unwrap();
        let query = query_for(&bare(None), String::new, datetime!(2026-10-15 12:00 UTC)).unwrap();
        let printed = serde_json::to_value(response.into_envelope(&query)).unwrap();
        assert_eq!(printed["query"], "charm CLI");
        assert_eq!(printed["top_k"], 5);
        assert_eq!(printed["items"][0]["session_id"], "s-1");
        assert_eq!(printed["items"][0]["a_field_from_the_future"], 7);
        assert!(printed.get("results").is_none());
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

        assert!(run(args(&server, false), 0).await.is_ok());
        let mut json = args(&server, false);
        json.json = true;
        assert!(run(json, 0).await.is_ok());
    }

    #[tokio::test]
    async fn an_empty_result_set_is_not_an_error() {
        let server = search_server(json!({"query": "charm CLI", "count": 0, "results": []})).await;
        assert!(run(args(&server, true), 0).await.is_ok());
        assert!(run(args(&server, false), 1).await.is_ok());
    }

    #[tokio::test]
    async fn the_filters_reach_the_server_under_their_wire_names() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/cassettes/search/spans"))
            .and(query_param("top_k", "3"))
            .and(query_param("subject", "matt"))
            .and(query_param("harness", "codex"))
            .and(query_param("live", "true"))
            .and(query_param_is_missing("cwd"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"results": []})))
            .expect(1)
            .mount(&server)
            .await;

        let mut args = args(&server, true);
        args.top = 3;
        args.subject = Some("matt".into());
        args.harness = Some("codex".into());
        args.live = true;
        assert!(run(args, 0).await.is_ok());
    }

    #[tokio::test]
    async fn a_malformed_time_bound_fails_before_any_request() {
        let server = MockServer::start().await;
        let mut args = args(&server, false);
        args.since = Some("last tuesday".into());
        let err = run(args, 0).await.unwrap_err();
        assert!(
            matches!(err, crate::Error::InvalidTimeBound { .. }),
            "{err}"
        );
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_search_without_a_server_fails_on_the_missing_url() {
        let result = run(bare(None), 0).await;
        assert!(matches!(result, Err(crate::Error::MissingTapesUrl)));
    }
}

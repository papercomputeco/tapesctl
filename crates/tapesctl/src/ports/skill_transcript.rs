//! Rendering a session as the transcript the extraction model reads.
//!
//! The shape is `[user]` / `[assistant]` / `[tools]` lines, built from the
//! derived trace surface: `GET /v1/traces?session_id=` for a session's
//! user-visible turns, then `GET /v1/traces/{id}` for each turn's spans.
//!
//! What is *excluded* is the point. Only main-thread, `main`-call-kind `llm`
//! spans become `[assistant]` lines, so the harness's shadow traffic —
//! permission checks, title generation, injected context, subagent threads —
//! never reaches the prompt. Thinking blocks are dropped for the same reason:
//! they are model-internal and inflate the prompt without adding workflow
//! signal. The model sees the conversation, not the machinery around it.
//!
//! The Go original also supported a single-turn filter and a lean mode with no
//! `[tools]` lines. Neither is ported: they existed for `deck` and the
//! structured export, and skill generation used neither.

use serde::Deserialize;
use serde_json::Value;
use snafu::ResultExt;
use time::OffsetDateTime;

use crate::api::client::ApiClient;
use crate::error::{Result, error};

/// One user-visible turn of a session.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TurnSummary {
    /// The trace this turn projects from.
    #[serde(default)]
    pub trace_id: String,
    /// The human's prompt. Empty for a synthetic turn.
    #[serde(default)]
    pub user_prompt: String,
    /// Derive-time preview, the stand-in when span text is unavailable.
    #[serde(default)]
    pub response_preview: String,
    /// Non-empty when the turn is a compaction seam or resume replay.
    #[serde(default)]
    pub synthetic: String,
    /// When the turn started, for the `--since`/`--until` window.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub started_at: Option<OffsetDateTime>,
}

/// One span of a turn.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Span {
    /// `llm`, `tool`, …
    #[serde(default)]
    pub kind: String,
    /// Tool name, for a `tool` span.
    #[serde(default)]
    pub name: String,
    /// `main` for the conversation spine; offshoots carry other values.
    #[serde(default)]
    pub call_kind: String,
    /// Non-empty on a subagent thread.
    #[serde(default)]
    pub thread_id: String,
    /// Content blocks. Left untyped — see [`blocks_text`].
    #[serde(default)]
    pub output: Value,
}

/// The `--since`/`--until` window, at turn grain.
#[derive(Debug, Clone, Copy, Default)]
pub struct TurnFilter {
    /// Drop turns starting before this.
    pub since: Option<OffsetDateTime>,
    /// Drop turns starting after this.
    pub until: Option<OffsetDateTime>,
}

impl TurnFilter {
    /// Whether either bound is set.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.since.is_none() && self.until.is_none()
    }
}

/// Keep the turns worth extracting from.
///
/// Synthetic turns go first — a compaction seam or resume replay carries no
/// user intent to learn from. A turn with no timestamp is treated as older
/// than any bound: `--since` drops it, `--until` keeps it, which is what the
/// Go zero-time comparison did.
#[must_use]
pub fn filter_turns(turns: Vec<TurnSummary>, filter: &TurnFilter) -> Vec<TurnSummary> {
    turns
        .into_iter()
        .filter(|turn| turn.synthetic.is_empty())
        .filter(|turn| match (filter.since, turn.started_at) {
            (Some(since), Some(at)) => at >= since,
            (Some(_), None) => false,
            (None, _) => true,
        })
        .filter(|turn| match (filter.until, turn.started_at) {
            (Some(until), Some(at)) => at <= until,
            (Some(_), None) => true,
            (None, _) => true,
        })
        .collect()
}

/// Fetch a session's turns and apply `filter`.
///
/// An empty result is an error rather than an empty transcript: generating a
/// skill from nothing would spend an LLM call to produce something invented.
pub async fn session_turns(
    client: &ApiClient,
    session_id: &str,
    filter: &TurnFilter,
) -> Result<Vec<TurnSummary>> {
    let document = client.list_traces(session_id).await?;
    // `/v1/traces?session_id=` is unpaginated by contract — the handler takes
    // no cursor or limit and returns the session's full turn list. This
    // tripwire turns a future, silently-truncating change of that contract
    // into a loud failure instead of an incomplete transcript.
    if document
        .get("next_cursor")
        .is_some_and(|cursor| !cursor.is_null())
    {
        return error::ApiContractSnafu {
            detail: "the traces listing began paginating; this client must learn cursors before \
                     it can build a complete transcript",
        }
        .fail();
    }
    let items = document
        .get("items")
        .cloned()
        .unwrap_or_else(|| Value::Array(Vec::new()));
    let turns: Vec<TurnSummary> = serde_json::from_value(items).context(error::ApiDecodeSnafu)?;

    let turns = filter_turns(turns, filter);
    snafu::ensure!(
        !turns.is_empty(),
        error::NoTurnsInSessionSnafu {
            session: session_id.to_owned(),
            filtered: !filter.is_empty(),
        }
    );
    Ok(turns)
}

/// Render one session as a transcript.
pub async fn build_session_transcript(
    client: &ApiClient,
    session_id: &str,
    filter: &TurnFilter,
) -> Result<String> {
    let turns = session_turns(client, session_id, filter).await?;
    let mut out = String::new();
    for turn in &turns {
        write_turn(client, &mut out, turn).await;
    }
    Ok(out)
}

/// Render one turn's prompt and its spine responses.
///
/// A failure to load the turn's spans is *not* propagated: the derive-time
/// preview stands in, so one unreachable trace degrades a single line rather
/// than failing a transcript the rest of which is fine.
async fn write_turn(client: &ApiClient, out: &mut String, turn: &TurnSummary) {
    if !turn.user_prompt.is_empty() {
        out.push_str(&format!("[user] {}\n", turn.user_prompt));
    }

    let spans = match client.get_trace(&turn.trace_id, None).await {
        Ok(document) => document
            .get("spans")
            .cloned()
            .and_then(|spans| serde_json::from_value::<Vec<Span>>(spans).ok())
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    };

    if !write_spine_responses(out, &spans) && !turn.response_preview.is_empty() {
        out.push_str(&format!("[assistant] {}\n", turn.response_preview));
    }
}

/// Walk a turn's spans in presentation order, emitting an `[assistant]` line
/// per conversation-spine span with text and a `[tools]` summary for the tool
/// calls between them. Reports whether any assistant text was written.
fn write_spine_responses(out: &mut String, spans: &[Span]) -> bool {
    let mut wrote = false;
    // Insertion-ordered counts: the summary reads in the order the tools were
    // actually called, which a hash map would scramble.
    let mut pending: Vec<(String, usize)> = Vec::new();

    for span in spans {
        match span.kind.as_str() {
            "tool" => {
                if !span.thread_id.is_empty() {
                    continue;
                }
                if let Some(entry) = pending.iter_mut().find(|(name, _)| *name == span.name) {
                    entry.1 += 1;
                } else {
                    pending.push((span.name.clone(), 1));
                }
            }
            "llm" => {
                if span.call_kind != "main" || !span.thread_id.is_empty() {
                    continue;
                }
                let text = blocks_text(&span.output);
                if text.is_empty() {
                    continue;
                }
                flush_tools(out, &mut pending);
                out.push_str(&format!("[assistant] {text}\n"));
                wrote = true;
            }
            _ => {}
        }
    }
    flush_tools(out, &mut pending);
    wrote
}

/// Emit the pending `[tools]` line, if any, and clear it.
fn flush_tools(out: &mut String, pending: &mut Vec<(String, usize)>) {
    if pending.is_empty() {
        return;
    }
    let rendered: Vec<String> = pending
        .iter()
        .map(|(name, count)| {
            if *count > 1 {
                format!("{name} ×{count}")
            } else {
                name.clone()
            }
        })
        .collect();
    out.push_str(&format!("[tools] {}\n", rendered.join(", ")));
    pending.clear();
}

/// Join the visible text blocks of a span's output.
///
/// Anything that is not an array of blocks yields no text — the same outcome
/// the Go implementation reached by ignoring its decode error. A span whose
/// payload shape this client does not recognize contributes nothing rather
/// than failing the turn.
#[must_use]
pub fn blocks_text(output: &Value) -> String {
    let Some(blocks) = output.as_array() else {
        return String::new();
    };
    blocks
        .iter()
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .filter(|text| !text.is_empty())
        .collect::<Vec<&str>>()
        .join("\n")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use serde_json::json;
    use time::format_description::well_known::Rfc3339;
    use url::Url;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn at(raw: &str) -> OffsetDateTime {
        OffsetDateTime::parse(raw, &Rfc3339).unwrap()
    }

    fn turn(trace_id: &str, started_at: Option<&str>) -> TurnSummary {
        TurnSummary {
            trace_id: trace_id.to_owned(),
            started_at: started_at.map(at),
            ..TurnSummary::default()
        }
    }

    fn span(kind: &str, name: &str, text: &str) -> Span {
        Span {
            kind: kind.to_owned(),
            name: name.to_owned(),
            call_kind: if kind == "llm" {
                "main".to_owned()
            } else {
                String::new()
            },
            thread_id: String::new(),
            output: if text.is_empty() {
                Value::Null
            } else {
                json!([{ "type": "text", "text": text }])
            },
        }
    }

    #[test]
    fn synthetic_turns_are_dropped_because_they_carry_no_user_intent() {
        let turns = vec![
            TurnSummary {
                trace_id: "keep".to_owned(),
                ..TurnSummary::default()
            },
            TurnSummary {
                trace_id: "drop".to_owned(),
                synthetic: "compaction".to_owned(),
                ..TurnSummary::default()
            },
        ];
        let kept = filter_turns(turns, &TurnFilter::default());
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].trace_id, "keep");
    }

    #[test]
    fn the_window_is_inclusive_at_both_bounds() {
        let turns = vec![
            turn("early", Some("2026-07-01T00:00:00Z")),
            turn("on-since", Some("2026-07-10T00:00:00Z")),
            turn("inside", Some("2026-07-15T00:00:00Z")),
            turn("on-until", Some("2026-07-20T00:00:00Z")),
            turn("late", Some("2026-07-25T00:00:00Z")),
        ];
        let kept = filter_turns(
            turns,
            &TurnFilter {
                since: Some(at("2026-07-10T00:00:00Z")),
                until: Some(at("2026-07-20T00:00:00Z")),
            },
        );
        let ids: Vec<&str> = kept.iter().map(|t| t.trace_id.as_str()).collect();
        assert_eq!(ids, vec!["on-since", "inside", "on-until"]);
    }

    #[test]
    fn an_undated_turn_is_treated_as_older_than_any_bound() {
        let undated = vec![turn("undated", None)];
        assert!(
            filter_turns(
                undated.clone(),
                &TurnFilter {
                    since: Some(at("2026-07-10T00:00:00Z")),
                    until: None,
                },
            )
            .is_empty(),
            "--since must drop it",
        );
        assert_eq!(
            filter_turns(
                undated,
                &TurnFilter {
                    since: None,
                    until: Some(at("2026-07-10T00:00:00Z")),
                },
            )
            .len(),
            1,
            "--until must keep it",
        );
    }

    #[test]
    fn only_the_main_thread_spine_becomes_assistant_text() {
        // The whole point of the transcript: harness shadow traffic and
        // subagent threads must not reach the extraction prompt.
        let spans = vec![
            span("llm", "", "on the spine"),
            Span {
                call_kind: "offshoot".to_owned(),
                ..span("llm", "", "a permission check")
            },
            Span {
                thread_id: "sub-1".to_owned(),
                ..span("llm", "", "a subagent")
            },
        ];
        let mut out = String::new();
        assert!(write_spine_responses(&mut out, &spans));
        assert_eq!(out, "[assistant] on the spine\n");
    }

    #[test]
    fn tool_calls_are_summarized_between_responses_with_repeat_counts() {
        let spans = vec![
            span("llm", "", "first"),
            span("tool", "Read", ""),
            span("tool", "Read", ""),
            span("tool", "Bash", ""),
            span("llm", "", "second"),
        ];
        let mut out = String::new();
        write_spine_responses(&mut out, &spans);
        assert_eq!(
            out,
            "[assistant] first\n[tools] Read ×2, Bash\n[assistant] second\n",
        );
    }

    #[test]
    fn trailing_tool_calls_are_still_reported() {
        let spans = vec![span("llm", "", "text"), span("tool", "Write", "")];
        let mut out = String::new();
        write_spine_responses(&mut out, &spans);
        assert_eq!(out, "[assistant] text\n[tools] Write\n");
    }

    #[test]
    fn subagent_tool_calls_are_not_counted() {
        let spans = vec![
            Span {
                thread_id: "sub-1".to_owned(),
                ..span("tool", "Read", "")
            },
            span("llm", "", "text"),
        ];
        let mut out = String::new();
        write_spine_responses(&mut out, &spans);
        assert_eq!(out, "[assistant] text\n", "no [tools] line expected");
    }

    #[test]
    fn thinking_blocks_are_dropped_but_text_blocks_are_joined() {
        let output = json!([
            {"type": "thinking", "thinking": "internal musing"},
            {"type": "text", "text": "one"},
            {"type": "text", "text": "two"},
        ]);
        assert_eq!(blocks_text(&output), "one\ntwo");
    }

    #[test]
    fn an_unrecognized_payload_yields_no_text_rather_than_failing() {
        assert_eq!(blocks_text(&Value::Null), "");
        assert_eq!(blocks_text(&json!({"not": "an array"})), "");
        assert_eq!(blocks_text(&json!(["a bare string"])), "");
    }

    async fn transcript_server(traces: Value, detail: Value) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/traces"))
            .and(query_param("session_id", "s-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(traces))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/traces/t-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(detail))
            .mount(&server)
            .await;
        server
    }

    fn client_for(server: &MockServer) -> ApiClient {
        ApiClient::new(Url::parse(&server.uri()).unwrap())
    }

    #[tokio::test]
    async fn a_transcript_interleaves_prompt_response_and_tools() {
        let server = transcript_server(
            json!({"items": [{"trace_id": "t-1", "user_prompt": "fix the test"}]}),
            json!({"spans": [
                {"kind": "llm", "call_kind": "main", "output": [{"type": "text", "text": "looking"}]},
                {"kind": "tool", "name": "Read"},
                {"kind": "llm", "call_kind": "main", "output": [{"type": "text", "text": "fixed"}]},
            ]}),
        )
        .await;

        let rendered =
            build_session_transcript(&client_for(&server), "s-1", &TurnFilter::default())
                .await
                .unwrap();

        assert_eq!(
            rendered,
            "[user] fix the test\n[assistant] looking\n[tools] Read\n[assistant] fixed\n",
        );
    }

    #[tokio::test]
    async fn the_preview_stands_in_when_a_turn_has_no_spine_text() {
        let server = transcript_server(
            json!({"items": [{
                "trace_id": "t-1",
                "user_prompt": "hello",
                "response_preview": "a preview",
            }]}),
            json!({"spans": []}),
        )
        .await;

        let rendered =
            build_session_transcript(&client_for(&server), "s-1", &TurnFilter::default())
                .await
                .unwrap();

        assert_eq!(rendered, "[user] hello\n[assistant] a preview\n");
    }

    #[tokio::test]
    async fn an_unreachable_trace_degrades_to_the_preview_rather_than_failing() {
        // One bad trace must not cost the user the rest of the transcript.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/traces"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items": [{
                "trace_id": "t-1",
                "user_prompt": "hello",
                "response_preview": "a preview",
            }]})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/traces/t-1"))
            .respond_with(ResponseTemplate::new(500).set_body_string(r#"{"error":"boom"}"#))
            .mount(&server)
            .await;

        let rendered =
            build_session_transcript(&client_for(&server), "s-1", &TurnFilter::default())
                .await
                .unwrap();

        assert_eq!(rendered, "[user] hello\n[assistant] a preview\n");
    }

    #[tokio::test]
    async fn a_session_with_no_usable_turns_is_an_error_not_an_empty_prompt() {
        let server = transcript_server(
            json!({"items": [{"trace_id": "t-1", "synthetic": "compaction"}]}),
            json!({"spans": []}),
        )
        .await;

        let err = build_session_transcript(&client_for(&server), "s-1", &TurnFilter::default())
            .await
            .unwrap_err();

        assert!(format!("{err}").contains("s-1"), "got: {err}");
    }
}

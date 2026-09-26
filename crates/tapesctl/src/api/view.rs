//! The human views for the read commands: `sessions list` and `get`,
//! `traces list` and `get`, `spans list` and `get`.
//!
//! Each view reads its columns off the undecoded [`serde_json::Value`] the
//! read client returns, so a field the server has not sent renders as absent
//! rather than failing the command, and a field the server grows simply has
//! no column yet. `--json` restores the document itself.

use serde_json::Value;
use time::OffsetDateTime;

use crate::render::style::status_tone;
use crate::render::text::{
    clock, count, duration_ns, elide, money, money_exact, one_line, relative, sanitize, short_id,
    stamp, tilde,
};
use crate::render::{Cell, Column, Record, Table, Theme, Tone};

/// Longest a title cell grows before eliding.
const TITLE_WIDTH: usize = 48;

/// Longest a prompt cell grows before eliding.
const PROMPT_WIDTH: usize = 60;

/// Terminal width at which the full session id replaces the short one.
const FULL_ID_FROM: usize = 140;

/// Terminal width at which the harness and model columns appear.
const WIDE_FROM: usize = 110;

// ---------------------------------------------------------------------------
// sessions list

/// Render `GET /v1/sessions` as a table with a one-line footer.
///
/// `sort` is the column the listing is ordered by, so the time column shows
/// the field that decided the order: `last_active` (the server default) shows
/// `last_seen_at`; `started_at` shows the start.
#[must_use]
pub fn sessions(value: &Value, sort: Option<&str>, theme: &Theme, now: OffsetDateTime) -> String {
    let Some(items) = value.get("items").and_then(Value::as_array) else {
        return "No sessions.\n".to_owned();
    };
    if items.is_empty() {
        return "No sessions.\n".to_owned();
    }

    let (time_header, time_path): (&'static str, &[&str]) = match sort {
        Some("started_at") => ("started", &["started_at"]),
        _ => ("last active", &["last_seen_at"]),
    };

    let mut table = Table::new(vec![
        Column::new("title").flex(16).max(TITLE_WIDTH),
        Column::new("status"),
        Column::new("harness").priority(5).from_width(WIDE_FROM),
        Column::new("model").priority(4).from_width(WIDE_FROM),
        Column::new("turns").right().priority(2),
        Column::new("cost").right().priority(3),
        Column::new(time_header),
        Column::new("id").priority(1).max(36),
    ]);

    for item in items {
        let status = status_word(item);
        let derived = status != "unknown";
        let id = sanitize(string_at(item, &["id"]));
        let id_shown = if theme.width >= FULL_ID_FROM || !theme.tty {
            id
        } else {
            short_id(&id)
        };
        table.row(vec![
            Cell::new(title(item), Tone::Primary),
            Cell::new(status.clone(), status_tone(&status)),
            Cell::or_absent(
                sanitize(string_at(item, &["harness_id"])),
                Tone::Secondary,
                theme,
            ),
            Cell::or_absent(
                sanitize(string_at(item, &["rollup", "model"])),
                Tone::Secondary,
                theme,
            ),
            if derived {
                Cell::or_absent(
                    number_at(item, &["rollup", "turn_count"])
                        .map(count)
                        .unwrap_or_default(),
                    Tone::Number,
                    theme,
                )
            } else {
                Cell::absent(theme)
            },
            Cell::or_absent(
                money(float_at(item, &["rollup", "usage", "cost_usd"])).unwrap_or_default(),
                Tone::Number,
                theme,
            ),
            Cell::new(relative(string_at(item, time_path), now), Tone::Secondary),
            Cell::new(id_shown, Tone::Secondary),
        ]);
    }

    let mut out = table.render(theme);
    out.push('\n');
    out.push_str(&theme.paint(Tone::Secondary, &sessions_footer(items.len(), value, theme)));
    out.push('\n');
    out
}

/// `8 sessions · more with --cursor eyJzb3J0…`
///
/// On a terminal the cursor is elided, because nobody types it; piped, it is
/// printed whole, because a script does.
fn sessions_footer(shown: usize, value: &Value, theme: &Theme) -> String {
    let noun = if shown == 1 { "session" } else { "sessions" };
    let mut footer = format!("{shown} {noun}");
    if let Some(cursor) = value
        .get("next_cursor")
        .and_then(Value::as_str)
        .filter(|cursor| !cursor.is_empty())
    {
        let cursor = sanitize(cursor);
        if theme.tty {
            footer.push_str(&format!(
                " · more with --cursor {}  (full cursor: --json)",
                elide(&cursor, 20)
            ));
        } else {
            footer.push_str(&format!(" · more with --cursor {cursor}"));
        }
    }
    footer
}

/// The session label the console would render, falling back the way the
/// server does: display title, then the captured name, then `untitled` with
/// the harness session's leading group so two untitled sessions can still be
/// told apart.
fn title(item: &Value) -> String {
    let title = sanitize(string_at(item, &["display_title"]));
    let harness_session = sanitize(string_at(item, &["harness_session_id"]));
    // An untitled session's display_title is the harness id cut short, which
    // reads as corruption rather than as a name; recognize and replace it.
    let is_placeholder = title.is_empty()
        || (!harness_session.is_empty() && harness_session.starts_with(&title) && title.len() < 16);
    if !is_placeholder {
        return title;
    }
    let name = sanitize(string_at(item, &["name"]));
    let name_is_placeholder = harness_session.starts_with(&name) && name.len() < 16;
    if !name.is_empty() && !name_is_placeholder {
        return name;
    }
    if harness_session.is_empty() {
        "untitled".to_owned()
    } else {
        format!("untitled ({})", short_id(&harness_session))
    }
}

/// The session's status: the deriver's word when it has one, otherwise the
/// liveness signal, otherwise `unknown`.
fn status_word(item: &Value) -> String {
    let status = sanitize(string_at(item, &["rollup", "status"]));
    if !status.is_empty() {
        return status;
    }
    if bool_at(item, &["live"]).unwrap_or(false) {
        return "live".to_owned();
    }
    "unknown".to_owned()
}

// ---------------------------------------------------------------------------
// sessions get

/// Render `GET /v1/sessions/{id}` as a record.
#[must_use]
pub fn session(value: &Value, theme: &Theme, now: OffsetDateTime) -> String {
    let item = value.get("session").unwrap_or(value);
    let id = sanitize(string_at(item, &["id"]));
    let status = status_word(item);
    let harness = {
        let name = sanitize(string_at(item, &["harness_id"]));
        let version = sanitize(string_at(item, &["harness_version"]));
        match (name.is_empty(), version.is_empty()) {
            (true, _) => String::new(),
            (false, true) => name,
            (false, false) => format!("{name} {version}"),
        }
    };

    let started = string_at(item, &["started_at"]);
    let last_seen = string_at(item, &["last_seen_at"]);
    let when = if started.is_empty() {
        String::new()
    } else {
        let mut when = format!("{} ({})", stamp(started, now), relative(started, now));
        if !last_seen.is_empty() && last_seen != started {
            when.push_str(&format!(", last seen {}", clock(last_seen)));
        }
        when
    };

    let usage = item.get("rollup").and_then(|r| r.get("usage"));
    let tokens = usage.map(tokens_line).unwrap_or_default();
    let cost = usage
        .and_then(|u| float_at(u, &["cost_usd"]))
        .filter(|c| *c > 0.0)
        .map(money_exact);

    let mut record = Record::new(title(item))
        .subtitle(&id)
        .subtitle(harness)
        .subtitle(sanitize(string_at(item, &["auth_subject"])))
        .field_toned("Status", &status, status_tone(&status))
        .field("Started", when)
        .maybe(
            "Turns",
            number_at(item, &["rollup", "turn_count"])
                .filter(|n| *n > 0)
                .map(count),
        )
        .field("Model", sanitize(string_at(item, &["rollup", "model"])))
        .field("Tokens", tokens)
        .maybe("Cost", cost)
        .field("Cwd", tilde(string_at(item, &["cwd"])));

    if let Some(preview) =
        Some(one_line(string_at(item, &["rollup", "preview"]))).filter(|p| !p.is_empty())
    {
        record = record.field_toned("Preview", elide(&preview, 120), Tone::Secondary);
    }
    if !id.is_empty() {
        record = record.next(format!("tapesctl traces list {id}"));
    }
    record.render(theme)
}

/// `194 in · 10,090 out · 463,145 cache read`, from a usage object.
fn tokens_line(usage: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(n) = number_at(usage, &["input_tokens"]).filter(|n| *n > 0) {
        parts.push(format!("{} in", count(n)));
    }
    if let Some(n) = number_at(usage, &["output_tokens"]).filter(|n| *n > 0) {
        parts.push(format!("{} out", count(n)));
    }
    if let Some(n) = number_at(usage, &["cache_read_tokens"]).filter(|n| *n > 0) {
        parts.push(format!("{} cache read", count(n)));
    }
    if let Some(n) = number_at(usage, &["cache_creation_tokens"]).filter(|n| *n > 0) {
        parts.push(format!("{} cache written", count(n)));
    }
    parts.join(" · ")
}

// ---------------------------------------------------------------------------
// traces list

/// Render `GET /v1/traces?session_id=` as a table: one row per turn.
#[must_use]
pub fn traces(value: &Value, theme: &Theme, now: OffsetDateTime) -> String {
    let Some(items) = value.get("items").and_then(Value::as_array) else {
        return "No traces.\n".to_owned();
    };
    if items.is_empty() {
        return "No traces.\n".to_owned();
    }

    let mut table = Table::new(vec![
        Column::new("#").right(),
        Column::new("status"),
        Column::new("spans").right().priority(3),
        Column::new("tokens")
            .right()
            .priority(4)
            .from_width(WIDE_FROM),
        Column::new("cost").right().priority(2),
        Column::new("took")
            .right()
            .priority(5)
            .from_width(WIDE_FROM),
        Column::new("started"),
        Column::new("prompt").flex(16).max(PROMPT_WIDTH),
        Column::new("trace").priority(1).max(48),
    ]);

    for (index, item) in items.iter().enumerate() {
        let status = sanitize(string_at(item, &["status"]));
        let tokens = match (
            number_at(item, &["usage", "input_tokens"]),
            number_at(item, &["usage", "output_tokens"]),
        ) {
            (Some(i), Some(o)) => format!("{} → {}", count(i), count(o)),
            _ => String::new(),
        };
        table.row(vec![
            Cell::new((index + 1).to_string(), Tone::Secondary),
            Cell::new(status.clone(), status_tone(&status)),
            Cell::or_absent(
                number_at(item, &["span_count"])
                    .map(count)
                    .unwrap_or_default(),
                Tone::Number,
                theme,
            ),
            Cell::or_absent(tokens, Tone::Number, theme),
            Cell::or_absent(
                money(float_at(item, &["usage", "cost_usd"])).unwrap_or_default(),
                Tone::Number,
                theme,
            ),
            Cell::or_absent(
                number_at(item, &["duration_ns"])
                    .map(duration_ns)
                    .unwrap_or_default(),
                Tone::Number,
                theme,
            ),
            Cell::new(
                relative(string_at(item, &["started_at"]), now),
                Tone::Secondary,
            ),
            Cell::or_absent(prompt_cell(item), Tone::Primary, theme),
            Cell::new(sanitize(string_at(item, &["trace_id"])), Tone::Secondary),
        ]);
    }

    let mut out = table.render(theme);
    out.push('\n');
    let noun = if items.len() == 1 { "trace" } else { "traces" };
    out.push_str(&theme.paint(Tone::Secondary, &format!("{} {noun}", items.len())));
    out.push('\n');
    out
}

/// The turn's prompt on one line, or `(synthetic turn)` when the server sent
/// an empty one on purpose.
fn prompt_cell(item: &Value) -> String {
    let prompt = one_line(string_at(item, &["user_prompt"]));
    if prompt.is_empty() && item.get("user_prompt").is_some() {
        "(synthetic turn)".to_owned()
    } else {
        prompt
    }
}

// ---------------------------------------------------------------------------
// traces get

/// Render `GET /v1/traces/{id}` as a record: the trace's own fields, with the
/// span count and a pointer to the spans listing.
#[must_use]
pub fn trace(value: &Value, theme: &Theme, now: OffsetDateTime) -> String {
    let item = value.get("trace").unwrap_or(value);
    let id = sanitize(string_at(item, &["trace_id"]));
    let status = sanitize(string_at(item, &["status"]));
    let span_count = value
        .get("spans")
        .and_then(Value::as_array)
        .map(|s| s.len() as i64)
        .or_else(|| number_at(item, &["span_count"]));

    let started = string_at(item, &["started_at"]);
    let ended = string_at(item, &["ended_at"]);
    let when = if started.is_empty() {
        String::new()
    } else {
        let mut when = stamp(started, now);
        if !ended.is_empty() {
            when.push_str(&format!(" → {}", clock(ended)));
        }
        if let Some(took) = number_at(item, &["duration_ns"]) {
            when.push_str(&format!(" ({})", duration_ns(took)));
        }
        when
    };

    let prompt = one_line(string_at(item, &["user_prompt"]));
    let title = if prompt.is_empty() {
        "(synthetic turn)".to_owned()
    } else {
        elide(&prompt, 100)
    };

    let mut record = Record::new(title)
        .subtitle(&id)
        .subtitle(sanitize(string_at(item, &["source"])))
        .subtitle(sanitize(string_at(value, &["session_id"])))
        .field_toned("Status", &status, status_tone(&status))
        .field("When", when)
        .maybe("Spans", span_count.map(count))
        .field(
            "Tokens",
            item.get("usage").map(tokens_line).unwrap_or_default(),
        )
        .maybe(
            "Cost",
            float_at(item, &["usage", "cost_usd"])
                .filter(|c| *c > 0.0)
                .map(money_exact),
        );

    let response = one_line(string_at(item, &["response_preview"]));
    if !response.is_empty() {
        record = record.field_toned("Response", elide(&response, 160), Tone::Secondary);
    }
    if !id.is_empty() {
        record = record.next(format!("tapesctl spans list {id}"));
    }
    record.render(theme)
}

// ---------------------------------------------------------------------------
// spans list

/// Render a trace's `spans` array as a table, in sequence order.
#[must_use]
pub fn spans(spans: &Value, theme: &Theme) -> String {
    let Some(items) = spans.as_array() else {
        return "No spans.\n".to_owned();
    };
    if items.is_empty() {
        return "No spans.\n".to_owned();
    }

    let mut table = Table::new(vec![
        Column::new("#").right(),
        Column::new("kind"),
        Column::new("name").flex(12).max(40),
        Column::new("status"),
        Column::new("model").priority(3).from_width(WIDE_FROM),
        Column::new("took").right().priority(2),
        Column::new("span").priority(1).max(48),
    ]);

    for item in items {
        let status = sanitize(string_at(item, &["status"]));
        let depth = if string_at(item, &["parent_span_id"]).is_empty() {
            0
        } else {
            1
        };
        let name = format!(
            "{}{}",
            "  ".repeat(depth),
            sanitize(string_at(item, &["name"]))
        );
        table.row(vec![
            Cell::new(
                number_at(item, &["seq"])
                    .map(|n| n.to_string())
                    .unwrap_or_default(),
                Tone::Secondary,
            ),
            Cell::or_absent(sanitize(string_at(item, &["kind"])), Tone::Secondary, theme),
            Cell::or_absent(name, Tone::Primary, theme),
            Cell::new(status.clone(), status_tone(&status)),
            Cell::or_absent(
                sanitize(string_at(item, &["model"])),
                Tone::Secondary,
                theme,
            ),
            Cell::or_absent(
                number_at(item, &["duration_ns"])
                    .map(duration_ns)
                    .unwrap_or_default(),
                Tone::Number,
                theme,
            ),
            Cell::new(sanitize(string_at(item, &["span_id"])), Tone::Secondary),
        ]);
    }

    let mut out = table.render(theme);
    out.push('\n');
    let noun = if items.len() == 1 { "span" } else { "spans" };
    out.push_str(&theme.paint(Tone::Secondary, &format!("{} {noun}", items.len())));
    out.push('\n');
    out
}

// ---------------------------------------------------------------------------
// spans get

/// Render one span as a record, with its input and output documents after the
/// fields when it has any.
#[must_use]
pub fn span(value: &Value, theme: &Theme, now: OffsetDateTime) -> String {
    let item = value.get("span").unwrap_or(value);
    let status = sanitize(string_at(item, &["status"]));
    let name = sanitize(string_at(item, &["name"]));
    let kind = sanitize(string_at(item, &["kind"]));
    let title = match (name.is_empty(), kind.is_empty()) {
        (false, false) => format!("{name} ({kind})"),
        (false, true) => name,
        (true, false) => kind,
        (true, true) => "span".to_owned(),
    };

    let started = string_at(item, &["started_at"]);
    let when = if started.is_empty() {
        String::new()
    } else {
        let mut when = stamp(started, now);
        if let Some(took) = number_at(item, &["duration_ns"]) {
            when.push_str(&format!(" ({})", duration_ns(took)));
        }
        when
    };

    let mut record = Record::new(title)
        .subtitle(sanitize(string_at(item, &["span_id"])))
        .subtitle(sanitize(string_at(item, &["trace_id"])))
        .field_toned("Status", &status, status_tone(&status))
        .field("When", when)
        .field("Model", sanitize(string_at(item, &["model"])))
        .field("Stop", sanitize(string_at(item, &["stop_reason"])))
        .field("Parent", sanitize(string_at(item, &["parent_span_id"])))
        .field(
            "Tokens",
            item.get("usage").map(tokens_line).unwrap_or_default(),
        )
        .maybe(
            "Cost",
            float_at(item, &["usage", "cost_usd"])
                .filter(|c| *c > 0.0)
                .map(money_exact),
        );

    for (label, key) in [("Input", "input"), ("Output", "output")] {
        if let Some(doc) = item.get(key).filter(|d| !is_empty_doc(d)) {
            if let Ok(rendered) = serde_json::to_string_pretty(doc) {
                record = record.field_toned(label, rendered, Tone::Secondary);
            }
        }
    }
    let _ = now;
    record.render(theme)
}

fn is_empty_doc(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        Value::String(s) => s.is_empty(),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// document readers

/// Read a string field nested under `path`, empty when any hop is missing.
fn string_at<'a>(value: &'a Value, path: &[&str]) -> &'a str {
    let mut node = value;
    for key in path {
        node = match node.get(*key) {
            Some(next) => next,
            None => return "",
        };
    }
    node.as_str().unwrap_or("")
}

/// Read an integer field nested under `path`.
fn number_at(value: &Value, path: &[&str]) -> Option<i64> {
    let mut node = value;
    for key in path {
        node = node.get(*key)?;
    }
    node.as_i64()
}

/// Read a floating-point field nested under `path`, accepting an integer as
/// well: the server may render a whole-dollar cost as `0` rather than `0.0`.
fn float_at(value: &Value, path: &[&str]) -> Option<f64> {
    let mut node = value;
    for key in path {
        node = node.get(*key)?;
    }
    node.as_f64().or_else(|| node.as_i64().map(|n| n as f64))
}

/// Read a boolean field nested under `path`.
fn bool_at(value: &Value, path: &[&str]) -> Option<bool> {
    let mut node = value;
    for key in path {
        node = node.get(*key)?;
    }
    node.as_bool()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use serde_json::json;
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-09-26 12:00 UTC);

    fn listing() -> Value {
        json!({
            "items": [
                {
                    "id": "01a0d365-2f42-77a1-8473-bd2e295244a4",
                    "display_title": "PLG drip email campaign grove",
                    "harness_id": "claude",
                    "harness_session_id": "9c1b0c3e-0000-0000-0000-000000000000",
                    "started_at": "2026-09-23T19:51:28Z",
                    "last_seen_at": "2026-09-24T02:07:00Z",
                    "live": false,
                    "rollup": {
                        "status": "completed",
                        "model": "claude-fable-5-1",
                        "turn_count": 19,
                        "usage": {"cost_usd": 41.6168}
                    }
                },
                {
                    "id": "01a0d365-2895-77f7-9ac2-dad41f0a1577",
                    "display_title": "51cc4b4a-cce",
                    "harness_id": "claude",
                    "harness_session_id": "51cc4b4a-cce8-44ab-95d3-69e50f26f96f",
                    "started_at": "2026-09-24T12:30:03Z",
                    "last_seen_at": "2026-09-24T12:30:08Z",
                    "live": false,
                    "rollup": {"status": "unknown", "turn_count": 0, "usage": {"cost_usd": 0}}
                }
            ],
            "next_cursor": "eyJzb3J0IjoibGFzdF9hY3RpdmUiLCJkaXIiOiJkZXNjIn0="
        })
    }

    #[test]
    fn an_empty_listing_renders_one_line_instead_of_a_header() {
        assert_eq!(
            sessions(&json!({"items": []}), None, &Theme::plain(80), NOW),
            "No sessions.\n"
        );
        assert_eq!(
            sessions(&json!({"next_cursor": "c"}), None, &Theme::plain(80), NOW),
            "No sessions.\n"
        );
    }

    #[test]
    fn the_sessions_table_at_eighty_columns() {
        let rendered = sessions(&listing(), None, &Theme::plain(80), NOW);
        assert_eq!(
            rendered,
            "TITLE                          STATUS     TURNS    COST  LAST ACTIVE  ID\n\
             PLG drip email campaign grove  completed     19  $41.62  2d ago       01a0d365\n\
             untitled (51cc4b4a)            unknown        —       —  1d ago       01a0d365\n\
             \n\
             2 sessions · more with --cursor eyJzb3J0IjoibGFzdF9…  (full cursor: --json)\n",
            "got:\n{rendered}"
        );
    }

    #[test]
    fn a_wide_terminal_adds_harness_model_and_the_full_id() {
        let rendered = sessions(&listing(), None, &Theme::plain(160), NOW);
        let header = rendered.lines().next().unwrap();
        assert!(header.contains("HARNESS"), "got:\n{rendered}");
        assert!(header.contains("MODEL"), "got:\n{rendered}");
        assert!(
            rendered.contains("01a0d365-2f42-77a1-8473-bd2e295244a4"),
            "got:\n{rendered}"
        );
        assert!(rendered.contains("claude-fable-5-1"), "got:\n{rendered}");
    }

    #[test]
    fn piped_output_keeps_the_full_id_and_cursor() {
        let rendered = sessions(&listing(), None, &Theme::piped(), NOW);
        assert!(
            rendered.contains("01a0d365-2f42-77a1-8473-bd2e295244a4"),
            "got:\n{rendered}"
        );
        assert!(
            rendered.contains("--cursor eyJzb3J0IjoibGFzdF9hY3RpdmUiLCJkaXIiOiJkZXNjIn0=\n"),
            "got:\n{rendered}"
        );
        assert!(!rendered.contains('—'), "got:\n{rendered}");
        assert!(!rendered.contains("full cursor"), "got:\n{rendered}");
    }

    #[test]
    fn sorting_by_start_shows_the_start_column() {
        let rendered = sessions(&listing(), Some("started_at"), &Theme::plain(80), NOW);
        assert!(
            rendered.lines().next().unwrap().contains("STARTED"),
            "got:\n{rendered}"
        );
        assert!(!rendered.contains("LAST ACTIVE"), "got:\n{rendered}");
    }

    #[test]
    fn a_placeholder_title_reads_as_untitled() {
        assert_eq!(
            title(
                &json!({"display_title": "51cc4b4a-cce", "harness_session_id": "51cc4b4a-cce8-44ab-95d3-69e50f26f96f"})
            ),
            "untitled (51cc4b4a)"
        );
        assert_eq!(title(&json!({"name": "Real name"})), "Real name");
        assert_eq!(title(&json!({})), "untitled");
    }

    #[test]
    fn a_live_session_without_a_status_word_reads_live() {
        let rendered = sessions(
            &json!({"items": [{"id": "s-1", "live": true}]}),
            None,
            &Theme::plain(80),
            NOW,
        );
        assert!(rendered.contains("live"), "got:\n{rendered}");
    }

    #[test]
    fn server_control_characters_are_sanitized_before_render() {
        let rendered = sessions(
            &json!({
                "items": [{
                    "id": "s-1",
                    "display_title": "evil\x1b[2Jtitle",
                    "harness_id": "cla\x1bude",
                    "rollup": {"status": "ok\x1b[31mred", "model": "gpt\r\n5"}
                }],
                "next_cursor": "abc\x1b[3J"
            }),
            None,
            &Theme::plain(200),
            NOW,
        );
        assert!(
            rendered.chars().all(|c| !c.is_control() || c == '\n'),
            "control characters leaked: {rendered:?}"
        );
        assert!(rendered.contains("evil [2Jtitle"), "got: {rendered}");
    }

    #[test]
    fn the_session_record() {
        let doc = json!({"session": {
            "id": "01a0d365-2b3c-75eb-9611-b6cc6fb281f1",
            "harness_id": "claude",
            "harness_version": "2.1.281",
            "cwd": "/somewhere/else/paper-forest",
            "started_at": "2026-09-24T05:01:11.445-07:00",
            "last_seen_at": "2026-09-24T05:20:59.626-07:00",
            "auth_subject": "local:bdougie",
            "display_title": "Paper Compute × ZeroDrift partnership one pager",
            "live": false,
            "rollup": {
                "status": "completed",
                "turn_count": 8,
                "model": "claude-fable-5-1",
                "usage": {"input_tokens": 1076, "output_tokens": 39288, "cost_usd": 5.7871}
            }
        }});
        let rendered = session(&doc, &Theme::plain(100), NOW);
        assert_eq!(
            rendered,
            "Paper Compute × ZeroDrift partnership one pager\n\
             01a0d365-2b3c-75eb-9611-b6cc6fb281f1 · claude 2.1.281 · local:bdougie\n\
             \n\
             Status   completed\n\
             Started  Sep 24 05:01 (1d ago), last seen 05:20\n\
             Turns    8\n\
             Model    claude-fable-5-1\n\
             Tokens   1,076 in · 39,288 out\n\
             Cost     $5.79\n\
             Cwd      /somewhere/else/paper-forest\n\
             \n\
             next  tapesctl traces list 01a0d365-2b3c-75eb-9611-b6cc6fb281f1\n",
            "got:\n{rendered}"
        );
    }

    #[test]
    fn the_traces_table() {
        let doc = json!({"items": [{
            "trace_id": "trc_txturn_11ebe314df3f279b21c3e64587bc29bc",
            "user_prompt": "read the notion page\nand then the pdf",
            "status": "ok",
            "started_at": "2026-09-24T05:01:11.445-07:00",
            "duration_ns": 279142000000_i64,
            "span_count": 70,
            "usage": {"input_tokens": 194, "output_tokens": 10090, "cost_usd": 1.4303}
        }, {
            "trace_id": "trc_2",
            "user_prompt": "",
            "status": "ok",
            "started_at": "2026-09-24T05:06:00-07:00"
        }]});
        let rendered = traces(&doc, &Theme::plain(160), NOW);
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(
            lines[0],
            "#  STATUS  SPANS        TOKENS   COST    TOOK  STARTED  PROMPT                                 TRACE",
            "got:\n{rendered}"
        );
        assert_eq!(
            lines[1],
            "1  ok         70  194 → 10,090  $1.43  4m 39s  1d ago   read the notion page and then the pdf  trc_txturn_11ebe314df3f279b21c3e64587bc29bc",
            "got:\n{rendered}"
        );
        assert_eq!(
            lines[2],
            "2  ok          —             —      —       —  1d ago   (synthetic turn)                       trc_2",
            "got:\n{rendered}"
        );
        assert_eq!(lines[4], "2 traces", "got:\n{rendered}");

        // Narrower, the prompt gives up width before any column is dropped.
        let narrow = traces(&doc, &Theme::plain(120), NOW);
        assert!(narrow.contains("read the notion pa…"), "got:\n{narrow}");
        assert!(
            narrow.lines().all(|l| crate::render::text::width(l) <= 120),
            "got:\n{narrow}"
        );
    }

    #[test]
    fn the_spans_table_indents_children() {
        let doc = json!([
            {"span_id": "agent_main", "seq": 0, "kind": "agent", "name": "main", "status": "ok", "duration_ns": 279142000000_i64},
            {"span_id": "txtool_1", "parent_span_id": "agent_main", "seq": 3, "kind": "tool", "name": "ToolSearch", "status": "ok", "duration_ns": 149141000000_i64}
        ]);
        let rendered = spans(&doc, &Theme::plain(100));
        assert!(rendered.contains("main"), "got:\n{rendered}");
        assert!(rendered.contains("  ToolSearch"), "got:\n{rendered}");
        assert!(rendered.contains("2 spans"), "got:\n{rendered}");
        assert!(rendered.contains("4m 39s"), "got:\n{rendered}");
    }

    #[test]
    fn the_trace_and_span_records_point_onward() {
        let doc = json!({
            "session_id": "s-1",
            "trace": {
                "trace_id": "trc_1",
                "user_prompt": "do the thing",
                "status": "ok",
                "source": "transcript",
                "started_at": "2026-09-24T05:01:11-07:00",
                "ended_at": "2026-09-24T05:05:50-07:00",
                "duration_ns": 279000000000_i64,
                "usage": {"input_tokens": 1, "output_tokens": 2, "cost_usd": 0.5}
            },
            "spans": [{"span_id": "a"}, {"span_id": "b"}]
        });
        let rendered = trace(&doc, &Theme::plain(100), NOW);
        assert!(
            rendered.starts_with("do the thing\ntrc_1 · transcript · s-1\n"),
            "got:\n{rendered}"
        );
        assert!(
            rendered.contains("When    Sep 24 05:01 → 05:05 (4m 39s)"),
            "got:\n{rendered}"
        );
        assert!(rendered.contains("Spans   2\n"), "got:\n{rendered}");
        assert!(rendered.contains("Cost    $0.5000"), "got:\n{rendered}");
        assert!(
            rendered.ends_with("next  tapesctl spans list trc_1\n"),
            "got:\n{rendered}"
        );

        let one = json!({"span_id": "sp", "trace_id": "trc_1", "kind": "tool", "name": "Read", "status": "ok",
            "input": [{"type": "tool_use"}], "output": []});
        let rendered = span(&one, &Theme::plain(100), NOW);
        assert!(
            rendered.starts_with("Read (tool)\nsp · trc_1\n"),
            "got:\n{rendered}"
        );
        assert!(rendered.contains("Input   [\n"), "got:\n{rendered}");
        assert!(!rendered.contains("Output"), "got:\n{rendered}");
    }
}

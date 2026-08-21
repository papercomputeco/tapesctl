//! The table view for `tapesctl sessions list`.
//!
//! `sessions list` renders its listing as a table by default; `--json` restores
//! the pretty-printed document. This module turns the undecoded
//! [`serde_json::Value`] the read client returns into that table, reading only
//! the fields the table has columns for and leaving the rest of the document
//! alone. That is the same projection spirit as `spans list`: a field the
//! server has not sent renders as a dash rather than failing the command, and a
//! field the server grows simply has no column yet.

use comfy_table::presets::UTF8_FULL;
use comfy_table::{Cell, Table};
use serde_json::Value;
use time::OffsetDateTime;
use time::UtcOffset;
use time::format_description::well_known::Rfc3339;

/// Longest title the table shows before eliding it with an ellipsis.
const TITLE_WIDTH: usize = 44;

/// Longest harness id before it is elided.
const HARNESS_WIDTH: usize = 16;

/// Longest model id before it is elided.
const MODEL_WIDTH: usize = 24;

/// What a missing field renders as, so an absent value is visibly absent.
const DASH: &str = "—";

/// Render the `GET /v1/sessions` document as a table, with the page cursor
/// (when the server says there is another page) as a trailing line.
///
/// The document is the undecoded response, so the columns read only what they
/// need and an absent field is a dash, never an error. An empty listing renders
/// one line instead of a header with no rows.
#[must_use]
pub fn render_sessions(value: &Value) -> String {
    let Some(items) = value.get("items").and_then(Value::as_array) else {
        return "No sessions.\n".to_owned();
    };

    if items.is_empty() {
        return "No sessions.\n".to_owned();
    }

    let mut table = Table::new();
    table.load_preset(UTF8_FULL).set_header(vec![
        Cell::new("ID"),
        Cell::new("TITLE"),
        Cell::new("STATUS"),
        Cell::new("HARNESS"),
        Cell::new("MODEL"),
        Cell::new("TURNS"),
        Cell::new("COST"),
        Cell::new("STARTED"),
    ]);

    for item in items {
        table.add_row(vec![
            Cell::new(sanitize(string_at(item, &["id"]))),
            Cell::new(title(item)),
            Cell::new(status(item)),
            Cell::new(elided(item, &["harness_id"], HARNESS_WIDTH)),
            Cell::new(elided(item, &["rollup", "model"], MODEL_WIDTH)),
            Cell::new(turns(item)),
            Cell::new(cost(item)),
            Cell::new(started_at(item)),
        ]);
    }

    let mut rendered = table.to_string();
    // comfy-table's Display does not end with a newline, so a listing with no
    // cursor would otherwise hand the shell a prompt glued to the bottom border.
    if !rendered.ends_with('\n') {
        rendered.push('\n');
    }
    if let Some(cursor) = value
        .get("next_cursor")
        .and_then(Value::as_str)
        .filter(|cursor| !cursor.is_empty())
    {
        rendered.push_str(&format!("\nnext cursor: {}\n", sanitize(cursor)));
    }
    rendered
}

/// The session label the console would render, falling back the way the server
/// does: display title, then the captured name.
fn title(item: &Value) -> String {
    let title = sanitize(string_at(item, &["display_title"]));
    if !title.is_empty() {
        return elide(&title, TITLE_WIDTH);
    }
    let name = sanitize(string_at(item, &["name"]));
    if !name.is_empty() {
        return elide(&name, TITLE_WIDTH);
    }
    DASH.to_owned()
}

/// The session's status: the deriver's word when it has one, otherwise the
/// liveness signal, otherwise a dash.
fn status(item: &Value) -> String {
    let status = sanitize(string_at(item, &["rollup", "status"]));
    if !status.is_empty() {
        return status;
    }
    if bool_at(item, &["live"]).unwrap_or(false) {
        return "live".to_owned();
    }
    DASH.to_owned()
}

/// The folded turn count, or a dash when the rollup did not arrive.
fn turns(item: &Value) -> String {
    match number_at(item, &["rollup", "turn_count"]) {
        Some(count) => count.to_string(),
        None => DASH.to_owned(),
    }
}

/// The folded spend in US dollars, to four decimals, or a dash.
fn cost(item: &Value) -> String {
    match float_at(item, &["rollup", "usage", "cost_usd"]) {
        Some(cost) => format!("${cost:.4}"),
        None => DASH.to_owned(),
    }
}

/// The session start time at second precision, or a dash.
fn started_at(item: &Value) -> String {
    let raw = sanitize(string_at(item, &["started_at"]));
    if raw.is_empty() {
        return DASH.to_owned();
    }
    format_seconds(&raw)
}

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
/// well — the server may render a whole-dollar cost as `0` rather than `0.0`.
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

/// Truncate to `width`, marking the cut with an ellipsis.
///
/// Counted in characters rather than bytes, so a multi-byte rune is never split
/// in half (the same guard `ports/search.rs` carries for the same reason).
fn elide(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        return value.to_owned();
    }
    let kept: String = value.chars().take(width.saturating_sub(3)).collect();
    format!("{kept}...")
}

/// Read a string field, neutralize its control characters, and elide to `width`.
fn elided(item: &Value, path: &[&str], width: usize) -> String {
    elide(&sanitize(string_at(item, path)), width)
}

/// Replace control characters with spaces so a server-returned value cannot
/// inject terminal control sequences (ESC, carriage return, backspace, C1
/// controls) into the listing, nor break a cell's layout with an embedded
/// newline.
///
/// These fields are the server's to set; sanitizing at the render boundary is
/// what keeps a hostile or buggy response from steering the user's terminal.
fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Render an RFC 3339 timestamp at second precision, in UTC.
///
/// A value that will not parse is shown as it arrived: it is the server's field,
/// and showing it beats showing nothing.
fn format_seconds(raw: &str) -> String {
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
    use serde_json::json;

    #[test]
    fn an_empty_listing_renders_one_line_instead_of_a_header() {
        assert_eq!(render_sessions(&json!({"items": []})), "No sessions.\n",);
    }

    #[test]
    fn a_document_without_items_renders_one_line_too() {
        assert_eq!(
            render_sessions(&json!({"next_cursor": "c"})),
            "No sessions.\n",
        );
    }

    #[test]
    fn a_row_renders_every_column() {
        let rendered = render_sessions(&json!({
            "items": [{
                "id": "01JDQ8F3K2M4N6P8R0T2V4X6Z8",
                "display_title": "Add a table view",
                "harness_id": "claude",
                "started_at": "2026-07-31T05:34:56-07:00",
                "live": false,
                "rollup": {
                    "status": "ended",
                    "model": "claude-opus",
                    "turn_count": 12,
                    "usage": {"cost_usd": 0.0421}
                }
            }],
            "next_cursor": ""
        }));

        assert!(
            rendered.contains("01JDQ8F3K2M4N6P8R0T2V4X6Z8"),
            "got: {rendered}"
        );
        assert!(rendered.contains("Add a table view"), "got: {rendered}");
        assert!(rendered.contains("ended"), "got: {rendered}");
        assert!(rendered.contains("claude"), "got: {rendered}");
        assert!(rendered.contains("claude-opus"), "got: {rendered}");
        assert!(rendered.contains("12"), "got: {rendered}");
        assert!(rendered.contains("$0.0421"), "got: {rendered}");
        assert!(rendered.contains("2026-07-31T12:34:56Z"), "got: {rendered}");
    }

    #[test]
    fn a_missing_field_is_a_dash_not_a_failure() {
        let rendered = render_sessions(&json!({"items": [{"id": "s-1"}]}));
        assert!(rendered.contains("—"), "got: {rendered}");
        assert!(rendered.contains("s-1"), "got: {rendered}");
    }

    #[test]
    fn a_whole_dollar_cost_is_still_a_dollar() {
        let rendered = render_sessions(&json!({
            "items": [{"id": "s-1", "rollup": {"usage": {"cost_usd": 0}}}]
        }));
        assert!(rendered.contains("$0.0000"), "got: {rendered}");
    }

    #[test]
    fn a_live_session_without_a_status_word_reads_live() {
        let rendered = render_sessions(&json!({
            "items": [{"id": "s-1", "live": true}]
        }));
        assert!(rendered.contains("live"), "got: {rendered}");
    }

    #[test]
    fn a_next_cursor_is_printed_for_paging() {
        let rendered = render_sessions(&json!({
            "items": [{"id": "s-1"}],
            "next_cursor": "abc123"
        }));
        assert!(rendered.contains("next cursor: abc123"), "got: {rendered}");
    }

    #[test]
    fn sanitize_replaces_control_characters_with_spaces() {
        assert_eq!(sanitize("plain"), "plain");
        assert_eq!(sanitize("a\tb"), "a b");
        assert_eq!(sanitize("a\r\nb"), "a  b");
        assert_eq!(sanitize("a\x1b[31mb"), "a [31mb");
    }

    #[test]
    fn server_control_characters_are_sanitized_before_render() {
        let rendered = render_sessions(&json!({
            "items": [{
                "id": "s-1",
                "display_title": "evil\x1b[2Jtitle",
                "harness_id": "cla\x1bude",
                "rollup": {
                    "status": "ok\x1b[31mred",
                    "model": "gpt\r\n5",
                }
            }],
            "next_cursor": "abc\x1b[3J"
        }));

        // The only control character the table may emit is the newline that
        // frames it; everything else must be neutralized.
        assert!(
            rendered.chars().all(|c| !c.is_control() || c == '\n'),
            "control characters leaked: {rendered:?}"
        );
        assert!(rendered.contains("evil [2Jtitle"), "got: {rendered}");
        assert!(rendered.contains("ok [31mred"), "got: {rendered}");
        assert!(rendered.contains("next cursor: abc [3J"), "got: {rendered}");
    }

    #[test]
    fn a_listing_ends_with_a_newline_either_way() {
        // Without this the shell prompt lands on the bottom border's line.
        let no_cursor = render_sessions(&json!({"items": [{"id": "s-1"}]}));
        assert!(no_cursor.ends_with('\n'), "got: {no_cursor:?}");
        assert!(!no_cursor.ends_with("\n\n"), "got: {no_cursor:?}");

        let with_cursor = render_sessions(&json!({
            "items": [{"id": "s-1"}],
            "next_cursor": "abc"
        }));
        assert!(
            with_cursor.ends_with("next cursor: abc\n"),
            "got: {with_cursor:?}"
        );
    }

    #[test]
    fn a_long_value_is_elided_without_splitting_a_rune() {
        let long = "é".repeat(200);
        let elided = elide(&long, MODEL_WIDTH);
        assert_eq!(elided.chars().count(), MODEL_WIDTH);
        assert!(elided.ends_with("..."));
        assert!(elided.starts_with('é'));
    }

    #[test]
    fn an_unparseable_timestamp_is_shown_rather_than_swallowed() {
        assert_eq!(format_seconds("not a time"), "not a time");
    }

    #[test]
    fn timestamps_print_at_second_precision_in_utc() {
        assert_eq!(
            format_seconds("2026-07-31T12:34:56.123456789Z"),
            "2026-07-31T12:34:56Z",
        );
        assert_eq!(
            format_seconds("2026-07-31T05:34:56-07:00"),
            "2026-07-31T12:34:56Z",
        );
    }
}

//! Formatting the values a view shows: money, counts, times, ids, and the
//! sanitizing every server string goes through before it reaches a terminal.

use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};
use unicode_width::UnicodeWidthStr;

/// Replace control and bidirectional formatting characters with spaces so a
/// server-returned value cannot inject terminal control sequences (ESC,
/// carriage return, backspace, C1 controls), reorder displayed text, or break
/// a row's layout with an embedded newline.
///
/// These fields are the server's to set; sanitizing at the render boundary is
/// what keeps a hostile or buggy response from steering the user's terminal.
#[must_use]
pub fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_control()
                || matches!(
                    c,
                    '\u{061c}'
                        | '\u{200e}'
                        | '\u{200f}'
                        | '\u{202a}'..='\u{202e}'
                        | '\u{2066}'..='\u{2069}'
                )
            {
                ' '
            } else {
                c
            }
        })
        .collect()
}

/// Collapse runs of whitespace (including newlines) into one space, for a
/// prompt or snippet shown on a single line.
#[must_use]
pub fn one_line(value: &str) -> String {
    sanitize(value)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The display width of `value` in terminal cells.
#[must_use]
pub fn width(value: &str) -> usize {
    UnicodeWidthStr::width(value)
}

/// Truncate to `width` cells, marking the cut with an ellipsis.
///
/// Measured in display cells rather than bytes or chars, so a wide glyph
/// counts for what it occupies and a multi-byte rune is never split.
#[must_use]
pub fn elide(value: &str, max: usize) -> String {
    if width(value) <= max {
        return value.to_owned();
    }
    let budget = max.saturating_sub(1);
    let mut kept = String::new();
    let mut used = 0;
    for c in value.chars() {
        let w = UnicodeWidthStr::width(c.encode_utf8(&mut [0; 4]) as &str);
        if used + w > budget {
            break;
        }
        kept.push(c);
        used += w;
    }
    let kept = kept.trim_end();
    format!("{kept}…")
}

/// Money in US dollars for a list: cents, `<$0.01` for a trace of spend, and
/// `None` for nothing at all so the caller can print the absent glyph.
#[must_use]
pub fn money(usd: Option<f64>) -> Option<String> {
    let usd = usd?;
    if usd <= 0.0 {
        return None;
    }
    if usd < 0.005 {
        return Some("<$0.01".to_owned());
    }
    Some(format!("${usd:.2}"))
}

/// Money in US dollars for a record view, where the exact figure matters:
/// four decimals under a dollar, two above.
#[must_use]
pub fn money_exact(usd: f64) -> String {
    if usd < 1.0 {
        format!("${usd:.4}")
    } else {
        format!("${usd:.2}")
    }
}

/// A count with thousands separators: `463,145`.
#[must_use]
pub fn count(n: i64) -> String {
    let negative = n < 0;
    let digits = n.unsigned_abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if negative {
        out.insert(0, '-');
    }
    out
}

/// A duration in nanoseconds as a human figure: `1.2s`, `4m 39s`, `2h 05m`.
#[must_use]
pub fn duration_ns(ns: i64) -> String {
    let secs = ns as f64 / 1e9;
    if secs < 1.0 {
        return format!("{}ms", (secs * 1000.0).round() as i64);
    }
    if secs < 60.0 {
        return format!("{secs:.1}s");
    }
    let total = secs.round() as i64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}h {m:02}m")
    } else {
        format!("{m}m {s:02}s")
    }
}

/// Parse an RFC 3339 timestamp, keeping the offset it arrived with.
#[must_use]
pub fn parse_time(raw: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(raw, &Rfc3339).ok()
}

/// A timestamp relative to `now` for a list: `just now`, `5m ago`, `3h ago`,
/// `2d ago`, then a calendar date once it is more than a week old.
///
/// A raw value that will not parse is shown as it arrived: it is the server's
/// field, and showing it beats showing nothing.
#[must_use]
pub fn relative(raw: &str, now: OffsetDateTime) -> String {
    let Some(then) = parse_time(raw) else {
        return sanitize(raw);
    };
    let ago = now - then;
    if ago < Duration::seconds(45) {
        return "just now".to_owned();
    }
    if ago < Duration::minutes(60) {
        return format!("{}m ago", ago.whole_minutes());
    }
    if ago < Duration::hours(24) {
        return format!("{}h ago", ago.whole_hours());
    }
    if ago < Duration::days(7) {
        return format!("{}d ago", ago.whole_days());
    }
    calendar(then, now)
}

/// A timestamp for a record view: `Sep 24 05:01`, with the year when it is
/// not this one. Rendered in the offset the server sent, which is the offset
/// the session ran in.
#[must_use]
pub fn stamp(raw: &str, now: OffsetDateTime) -> String {
    let Some(then) = parse_time(raw) else {
        return sanitize(raw);
    };
    format!(
        "{} {:02}:{:02}",
        calendar(then, now),
        then.hour(),
        then.minute()
    )
}

/// Just the clock, for a second timestamp on the same day as the first.
#[must_use]
pub fn clock(raw: &str) -> String {
    let Some(then) = parse_time(raw) else {
        return sanitize(raw);
    };
    format!("{:02}:{:02}", then.hour(), then.minute())
}

/// `Sep 24`, or `Sep 24 2025` when the year differs from `now`'s.
fn calendar(then: OffsetDateTime, now: OffsetDateTime) -> String {
    let month = &format!("{:?}", then.month())[..3];
    if then.year() == now.year() {
        format!("{month} {:>2}", then.day())
    } else {
        format!("{month} {:>2} {}", then.day(), then.year())
    }
}

/// The leading group of a UUID-shaped id, for a narrow column: `01a0d365`.
///
/// Only the display is shortened; the read commands still take the full id,
/// which `--json` carries.
#[must_use]
pub fn short_id(id: &str) -> String {
    let id = sanitize(id);
    match id.split_once('-') {
        Some((head, _)) if head.len() >= 8 => head.to_owned(),
        _ => elide(&id, 12),
    }
}

/// A path with the home directory collapsed to `~`.
#[must_use]
pub fn tilde(path: &str) -> String {
    let path = sanitize(path);
    match dirs::home_dir() {
        Some(home) => match path.strip_prefix(&*home.to_string_lossy()) {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("~{rest}"),
            _ => path,
        },
        None => path,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn sanitize_replaces_control_and_bidi_characters_with_spaces() {
        assert_eq!(sanitize("plain"), "plain");
        assert_eq!(sanitize("a\tb"), "a b");
        assert_eq!(sanitize("a\r\nb"), "a  b");
        assert_eq!(sanitize("a\x1b[31mb"), "a [31mb");
        assert_eq!(sanitize("a\u{202e}b\u{2066}c"), "a b c");
    }

    #[test]
    fn one_line_collapses_whitespace() {
        assert_eq!(one_line("a\n\n  b\tc"), "a b c");
    }

    #[test]
    fn elide_measures_cells_not_bytes() {
        let long = "é".repeat(200);
        let cut = elide(&long, 24);
        assert_eq!(width(&cut), 24);
        assert!(cut.ends_with('…'));
        assert_eq!(elide("short", 24), "short");
        // A double-width glyph counts twice, so eliding never overshoots.
        let wide = "日本語日本語日本語";
        assert!(width(&elide(wide, 7)) <= 7);
    }

    #[test]
    fn money_for_a_list_is_cents_or_nothing() {
        assert_eq!(money(None), None);
        assert_eq!(money(Some(0.0)), None);
        assert_eq!(money(Some(0.0021)).as_deref(), Some("<$0.01"));
        assert_eq!(money(Some(41.6168)).as_deref(), Some("$41.62"));
        assert_eq!(money_exact(0.0421), "$0.0421");
        assert_eq!(money_exact(5.7871), "$5.79");
    }

    #[test]
    fn counts_get_separators() {
        assert_eq!(count(0), "0");
        assert_eq!(count(999), "999");
        assert_eq!(count(1000), "1,000");
        assert_eq!(count(463_145), "463,145");
        assert_eq!(count(-1234), "-1,234");
    }

    #[test]
    fn durations_read_at_the_right_grain() {
        assert_eq!(duration_ns(250_000_000), "250ms");
        assert_eq!(duration_ns(1_500_000_000), "1.5s");
        assert_eq!(duration_ns(279_142_000_000), "4m 39s");
        assert_eq!(duration_ns(7_500_000_000_000), "2h 05m");
    }

    #[test]
    fn relative_times_step_through_the_grains() {
        let now = datetime!(2026-09-26 12:00 UTC);
        assert_eq!(relative("2026-09-26T11:59:50Z", now), "just now");
        assert_eq!(relative("2026-09-26T11:45:00Z", now), "15m ago");
        assert_eq!(relative("2026-09-26T09:00:00Z", now), "3h ago");
        assert_eq!(relative("2026-09-23T19:51:28Z", now), "2d ago");
        assert_eq!(relative("2026-09-01T00:00:00Z", now), "Sep  1");
        assert_eq!(relative("2025-12-25T00:00:00Z", now), "Dec 25 2025");
        assert_eq!(relative("not a time", now), "not a time");
    }

    #[test]
    fn stamps_keep_the_servers_offset() {
        let now = datetime!(2026-09-26 12:00 UTC);
        assert_eq!(stamp("2026-09-24T05:01:11.445-07:00", now), "Sep 24 05:01");
        assert_eq!(clock("2026-09-24T05:20:59.626-07:00"), "05:20");
    }

    #[test]
    fn short_ids_take_the_first_group() {
        assert_eq!(short_id("01a0d365-2f42-77a1-8473-bd2e295244a4"), "01a0d365");
        assert_eq!(short_id("s-1"), "s-1");
    }
}

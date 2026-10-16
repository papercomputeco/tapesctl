//! The flag grammar the recall commands share: who (`--subject`, with `me`)
//! and when (`--since` / `--until`, absolute or relative).
//!
//! Both are resolved here, client-side, before anything is sent: the server's
//! `subject` filter never sees `me`, and its time filters take RFC 3339 only.
//! `search`, `sessions list`, and anything else that grows the same flags go
//! through these two functions, so `me` and `7d` mean one thing everywhere.

use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};

use crate::error::{Result, error};

/// The `--subject` value that names this machine's own subject.
pub const ME: &str = "me";

/// The environment variable a capture reads its acting subject from.
pub const TAPES_AUTH_SUBJECT_ENV: &str = "TAPES_AUTH_SUBJECT";

/// This machine's own acting subject: what `start`, `capture`, and `sync`
/// stamp when not told otherwise.
///
/// `TAPES_AUTH_SUBJECT` when set, else `local:<username>` — the same default
/// the capture commands apply, so `--subject me` finds exactly the sessions
/// this machine recorded under its default identity. An ambient read, so it
/// happens at the command boundary and is passed down.
#[must_use]
pub fn own_subject() -> String {
    std::env::var(TAPES_AUTH_SUBJECT_ENV)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| format!("local:{}", crate::start::local_username()))
}

/// Resolve a `--subject` value, replacing `me` with `own`.
///
/// Blank is no filter. Anything else travels as typed: the match rule
/// (subject, email, email local part, name, first name) is the server's.
#[must_use]
pub fn resolve_subject(raw: Option<String>, own: impl FnOnce() -> String) -> Option<String> {
    let raw = raw?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else if trimmed.eq_ignore_ascii_case(ME) {
        Some(own())
    } else {
        Some(trimmed.to_owned())
    }
}

/// Resolve a `--since` / `--until` value into the RFC 3339 timestamp the
/// server takes.
///
/// An RFC 3339 timestamp passes through as typed. A relative duration —
/// a whole number and one of `m` (minutes), `h`, `d`, `w` — is that long
/// before `now`. Anything else is refused here, naming the accepted forms,
/// rather than sent for the server to reject.
pub fn resolve_time(
    flag: &'static str,
    raw: Option<String>,
    now: OffsetDateTime,
) -> Result<Option<String>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let value = raw.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if OffsetDateTime::parse(value, &Rfc3339).is_ok() {
        return Ok(Some(value.to_owned()));
    }
    let invalid = || {
        error::InvalidTimeBoundSnafu {
            flag,
            value: value.to_owned(),
        }
        .build()
    };
    // Split before the last character, on a char boundary: a byte index
    // could land inside a multi-byte character and panic.
    let cut = value.char_indices().last().map_or(0, |(at, _)| at);
    let (digits, unit) = value.split_at(cut);
    let amount = i64::from(digits.parse::<u32>().map_err(|_| invalid())?);
    let ago = match unit {
        "m" => Duration::minutes(amount),
        "h" => Duration::hours(amount),
        "d" => Duration::days(amount),
        "w" => Duration::weeks(amount),
        _ => return Err(invalid()),
    };
    let then = now.checked_sub(ago).ok_or_else(invalid)?;
    // Whole seconds: a sub-second fraction says nothing a person meant by
    // "7d", and a shorter timestamp reads better in a `-v` log.
    let then = then.replace_nanosecond(0).map_err(|_| invalid())?;
    then.format(&Rfc3339).map(Some).map_err(|_| invalid())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn me_is_resolved_and_everything_else_travels_as_typed() {
        let own = || "local:ada".to_owned();
        assert_eq!(
            resolve_subject(Some("me".into()), own),
            Some("local:ada".into())
        );
        assert_eq!(
            resolve_subject(Some(" ME ".into()), own),
            Some("local:ada".into())
        );
        assert_eq!(
            resolve_subject(Some("Ada Lovelace".into()), own),
            Some("Ada Lovelace".into())
        );
        assert_eq!(resolve_subject(Some("  ".into()), own), None);
        assert_eq!(resolve_subject(None, own), None);
    }

    #[test]
    fn me_is_not_resolved_unless_asked_for() {
        // The ambient read must not happen for a filter that does not need it.
        let own = || panic!("own subject read for a value that was not `me`");
        assert_eq!(
            resolve_subject(Some("matt".into()), own),
            Some("matt".into())
        );
    }

    #[test]
    fn relative_bounds_become_rfc3339_before_now() {
        let now = datetime!(2026-10-15 12:00:30.5 UTC);
        let at = |raw: &str| resolve_time("--since", Some(raw.into()), now).unwrap();
        assert_eq!(at("7d").as_deref(), Some("2026-10-08T12:00:30Z"));
        assert_eq!(at("24h").as_deref(), Some("2026-10-14T12:00:30Z"));
        assert_eq!(at("2w").as_deref(), Some("2026-10-01T12:00:30Z"));
        assert_eq!(at("30m").as_deref(), Some("2026-10-15T11:30:30Z"));
        assert_eq!(
            at("2026-07-01T00:00:00Z").as_deref(),
            Some("2026-07-01T00:00:00Z")
        );
        assert_eq!(resolve_time("--since", None, now).unwrap(), None);
    }

    #[test]
    fn a_bound_that_is_neither_form_is_refused_naming_the_flag() {
        let now = datetime!(2026-10-15 12:00 UTC);
        for bad in ["yesterday", "7", "d", "7y", "-3d", "2026-07-01", "7é"] {
            let err = resolve_time("--until", Some(bad.into()), now).unwrap_err();
            let shown = err.to_string();
            assert!(
                shown.contains("--until") && shown.contains("7d"),
                "{bad}: {shown}"
            );
        }
    }
}

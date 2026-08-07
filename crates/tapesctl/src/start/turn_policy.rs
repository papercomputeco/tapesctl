//! Whether an exchange is a turn at all.
//!
//! Two gates run ahead of every capture, and they are the only two questions
//! answered before a single byte of the exchange is examined: was this a
//! turn-shaped request, and did the provider actually complete it. Everything
//! else the capture path decides — can the body be decoded, is it JSON, is it
//! within the caps — is about an exchange these two already admitted.
//!
//! Like [`super::content_encoding`], this is one half of a contract whose other
//! half is implemented independently in Go at the gateway. Both halves are
//! specified as data in the shared drop-reason corpus authored in tapes at
//! `fixtures/drop-reason/`, vendored here at `vendor/tapes-drop-reason-fixtures/`
//! and run against the predicates below by `tests/drop_reason_corpus.rs`. The
//! corpus is what keeps them the same rules rather than two readings of the same
//! prose.
//!
//! # The reasons, and why they are named
//!
//! A capture path that declines to record a turn owes an answer to "why", and
//! the answer is a wire-visible string: it is what an operator greps for, and
//! on the gateway half it is also a metric label. So the reasons are the
//! corpus's spellings, not this module's — a client that agreed on the rule and
//! disagreed on its name would still leave two vocabularies.
//!
//! # Dropping is not silence
//!
//! Refusing to capture a failed exchange is not the same as discarding it. A
//! failed call has diagnostic value — an error body is how more than one
//! authentication problem has been diagnosed — and that value is preserved by
//! *reporting* the drop with its reason and its status, not by storing a
//! non-turn as a turn. Storing it is in fact worse for whoever is debugging:
//! ingest refuses a reduced response that has no assistant message, so the
//! failure surfaced as a rejected POST at the end of the pipeline rather than
//! as a named local decision at the start of it.

use url::Url;

/// The request was not a turn: not a turn path, or not a turn-shaped method on
/// one. Spelled as the shared corpus spells it.
pub const DROP_NON_TURN_REQUEST: &str = "non_turn_request";

/// The upstream did not complete the exchange, so there is no turn to record.
/// Spelled as the shared corpus spells it.
pub const DROP_UPSTREAM_STATUS: &str = "upstream_status";

/// The provider chat-completion endpoints a turn can be addressed to.
///
/// Matched as *clean suffixes* (query stripped, trailing slashes trimmed) so a
/// route prefix in front of the provider — a gateway's, or the ChatGPT
/// backend's own `/backend-api/codex` — resolves to the endpoint rather than
/// hiding it. Adjacent non-turn endpoints on the same host do not match:
/// `/v1/messages/count_tokens` answers with token counts, not assistant
/// content, and reducing it would put non-conversation in the conversation log.
const TURN_PATH_SUFFIXES: &[&str] = &[
    "/v1/chat/completions",
    "/v1/responses",
    "/codex/responses",
    "/v1/messages",
    "/api/chat",
];

/// Why this exchange will not be captured, or `None` if it survives both gates.
///
/// `path` must be the path the PROVIDER sees — the request path already
/// resolved against the upstream route — not the path the harness asked for.
/// The two are routinely different here in a way they are not at the gateway: a
/// plan-authenticated Codex is pointed at a bare origin and appends
/// `/responses`, while its upstream base already ends in `/backend-api/codex`.
/// Gating on the unresolved path would refuse every one of those turns while
/// looking, line for line, like the same rule.
///
/// The order is the corpus's specified precedence. An exchange can fail both
/// gates at once — a health probe that also returned 500 — and two
/// implementations reporting different reasons for it have given two different
/// answers to the same question, even though both correctly declined to
/// capture. Status first, because a non-success exchange is not examined
/// further.
#[must_use]
pub fn capture_refusal(method: &str, path: &str, status: u16) -> Option<&'static str> {
    if !is_capturable_upstream_status(status) {
        return Some(DROP_UPSTREAM_STATUS);
    }
    if !is_capturable_turn_request(method, path) {
        return Some(DROP_NON_TURN_REQUEST);
    }
    None
}

/// The path the provider sees, read off the URL a request is being sent to.
///
/// A named read of the resolved URL rather than string surgery over the inbound
/// path and the upstream base: whatever [`super::proxy`] builds the outbound
/// request from is by definition the path the provider is asked for, so reading
/// it back cannot disagree with what was actually sent. Re-deriving it would be
/// a second answer, and the first time the two differed the gate would be
/// judging an endpoint nobody called.
#[must_use]
pub fn provider_path(upstream: &Url) -> &str {
    upstream.path()
}

/// Whether `(method, path)` is a turn request at all — the
/// [`DROP_NON_TURN_REQUEST`] predicate.
///
/// An absent method is treated as capturable: an empty one means the method was
/// not observed rather than that a non-POST method was used, and refusing on
/// missing information would drop real turns.
#[must_use]
pub fn is_capturable_turn_request(method: &str, path: &str) -> bool {
    if !is_turn_request_path(path) {
        return false;
    }
    method.is_empty() || method.eq_ignore_ascii_case("POST")
}

/// Whether `status` leaves a turn capturable — the [`DROP_UPSTREAM_STATUS`]
/// predicate.
///
/// Exactly 200, and not "any 2xx". A turn is a completed exchange with a
/// provider, and the chat-completion endpoints this proxy fronts answer a
/// completed exchange with 200 and nothing else; widening it to a class would
/// admit shapes no provider sends here. The reference implementation applies
/// the same rule, and the corpus pins 201 and 204 as dropped so that widening
/// it has to delete a case rather than pass.
#[must_use]
pub fn is_capturable_upstream_status(status: u16) -> bool {
    status == 200
}

/// Whether `path` names a provider chat-completion endpoint.
fn is_turn_request_path(path: &str) -> bool {
    TURN_PATH_SUFFIXES
        .iter()
        .any(|suffix| path_has_clean_suffix(path, suffix))
}

/// `path` ends in `suffix`, ignoring a query string and trailing slashes.
fn path_has_clean_suffix(path: &str, suffix: &str) -> bool {
    let path = match path.find('?') {
        Some(i) => &path[..i],
        None => path,
    };
    path.trim_end_matches('/').ends_with(suffix)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    // The behaviour of the path and status rules themselves is table-tested
    // against the shared corpus in tests/drop_reason_corpus.rs. What is asserted
    // here is what the corpus deliberately does not express: the precedence
    // between the two gates, and the path resolution this client has to do that
    // the gateway is handed for free.

    #[test]
    fn a_completed_turn_survives_both_gates() {
        assert_eq!(capture_refusal("POST", "/v1/messages", 200), None);
    }

    #[test]
    fn a_failed_exchange_is_refused_before_its_request_line_is_judged() {
        // Both gates refuse this probe. The corpus specifies which reason wins,
        // because two implementations answering differently have given two
        // different answers to the same question.
        assert_eq!(
            capture_refusal("HEAD", "/v1/messages", 500),
            Some(DROP_UPSTREAM_STATUS),
        );
    }

    #[test]
    fn a_health_probe_on_a_turn_path_is_a_probe() {
        assert_eq!(
            capture_refusal("HEAD", "/v1/messages", 200),
            Some(DROP_NON_TURN_REQUEST),
        );
    }

    #[test]
    fn a_plan_authenticated_codex_turn_resolves_through_its_backend_prefix() {
        // The path the harness asks for is not a turn path on its own; the path
        // the provider sees is. Gating on the former would drop every turn of
        // every ChatGPT-plan Codex session while the rule read as identical to
        // the gateway's.
        assert!(!is_capturable_turn_request("POST", "/responses"));

        let upstream = Url::parse("https://chatgpt.com/backend-api/codex/responses").unwrap();
        assert!(is_capturable_turn_request("POST", provider_path(&upstream)));
    }

    #[test]
    fn an_api_key_codex_turn_resolves_too() {
        let upstream = Url::parse("https://api.openai.com/v1/responses").unwrap();
        assert!(is_capturable_turn_request("POST", provider_path(&upstream)));
    }
}

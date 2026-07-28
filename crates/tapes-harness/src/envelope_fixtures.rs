//! Producer-side oracle for the shared envelope fixture corpus vendored at
//! `vendor/tapes-envelope-fixtures/` (source: tapes `fixtures/envelope/` — see
//! that directory's `SOURCE.md`).
//!
//! The corpus pins the `X-Tapes-*` header ↔ session-envelope contract. This
//! crate is the **producer**: it turns a resolved session identity into the on-wire
//! header set. The parsers on the other side (tapes-extproc's
//! `ParseSessionEnvelope`, the tapes ingest reader) table-test against the same
//! files. Drift between the two halves is otherwise invisible until a captured
//! session lands mis-attributed, so this test exists to make the corpus
//! executable here rather than merely documentary.
//!
//! ### Which cases this side owns
//!
//! Each case declares a `direction`:
//!
//! * `roundtrip` — `encode(envelope) == headers`. Asserted here.
//! * `encode` — a *lossy* producer transform (session-name truncation,
//!   oversize-metadata drop, percent-encoding a path the reader won't decode
//!   back). The logical input is the case's `encode_from`, not its `envelope`;
//!   `encode(encode_from) == headers` is asserted here.
//! * `decode` — parser-only cases: malformed or missing-header input that a
//!   well-behaved producer never emits (empty parent header, metadata that
//!   isn't valid base64, a missing harness-id). Skipped here **by design** —
//!   there is no encode side to assert. The parser oracles cover them.
//!
//! ### What is compared
//!
//! Only the `x-tapes-*` headers. `x-paper-auth-org-id` / `x-paper-auth-subject`
//! appear in every case's header set but are **server-trusted**: the cloud edge
//! sets them from validated JWT claims. The producer must not emit them, and the
//! test asserts that it doesn't.
//!
//! The metadata header is compared as *decoded JSON*, not as a base64 string.
//! JSON key ordering is not part of the contract, so byte-comparing the encoded
//! blob would pin an implementation detail of whichever serializer produced the
//! fixture. Every other header is compared byte for byte.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use http::HeaderMap;
use serde::Deserialize;

use super::{
    HARNESS_ID_UNKNOWN, TapesAttribution, X_TAPES_HARNESS_METADATA, inject_tapes_attribution,
    inject_tapes_headers,
};

/// One `cases/*.json` file. Unknown fields (`grounding`, `notes`, `error`, …)
/// are ignored: they carry provenance for humans, not assertions for this side.
#[derive(Debug, Deserialize)]
struct FixtureCase {
    name: String,
    direction: String,
    headers: BTreeMap<String, String>,
    envelope: FixtureEnvelope,
    /// Present only on lossy (`direction: encode`) cases: the logical envelope
    /// a producer starts from, before truncation / drop / percent-encoding.
    #[serde(default)]
    encode_from: Option<FixtureEnvelope>,
}

/// The envelope side of a case. `org_id` / `auth_subject` are deliberately not
/// modelled — they are not the producer's to emit (see module docs).
#[derive(Debug, Clone, Deserialize)]
struct FixtureEnvelope {
    #[serde(default)]
    harness_id: Option<String>,
    #[serde(default)]
    harness_session_id: Option<String>,
    #[serde(default)]
    harness_version: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    parent_harness_session_id: Option<String>,
    #[serde(default)]
    harness_metadata: Option<serde_json::Value>,
}

fn cases_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("vendor")
        .join("tapes-envelope-fixtures")
        .join("cases")
}

/// Load every vendored case, sorted by path so failures report in a stable
/// order regardless of directory iteration order.
fn load_cases() -> Vec<FixtureCase> {
    let dir = cases_dir();
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .map(|entry| entry.expect("read dir entry").path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    paths.sort();

    assert!(
        !paths.is_empty(),
        "no envelope fixture cases under {} — run scripts/sync-envelope-fixtures.sh <tapes-checkout>",
        dir.display(),
    );

    paths
        .iter()
        .map(|p| {
            let bytes = std::fs::read(p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()));
            serde_json::from_slice(&bytes).unwrap_or_else(|e| panic!("parse {}: {e}", p.display()))
        })
        .collect()
}

/// Build the attribution a producer would hold for `env`.
///
/// This constructs [`TapesAttribution`] field-by-field rather than going
/// through `claude()` / `codex_session()` because the corpus spans harnesses
/// those constructors don't cover (`pi`) and field combinations they can't
/// express. The named constructors are what production uses; this exercises the
/// serialization they all funnel into.
fn attribution_from(env: &FixtureEnvelope) -> TapesAttribution {
    let metadata = match &env.harness_metadata {
        Some(serde_json::Value::Object(map)) => map.clone(),
        // A non-object metadata value is a parser-side concern; no producer
        // path can construct one (the field is typed as a JSON object).
        _ => serde_json::Map::new(),
    };

    TapesAttribution {
        harness_id: env
            .harness_id
            .clone()
            .unwrap_or_else(|| HARNESS_ID_UNKNOWN.to_owned()),
        session_id: env.harness_session_id.clone(),
        version: env.harness_version.clone(),
        cwd: env.cwd.clone(),
        name: env.name.clone(),
        parent_sid: env.parent_harness_session_id.clone(),
        metadata,
    }
}

/// The `x-tapes-*` subset of a header map, as plain strings.
fn tapes_headers(headers: &HeaderMap) -> BTreeMap<String, String> {
    headers
        .iter()
        .filter(|(name, _)| name.as_str().starts_with("x-tapes-"))
        .map(|(name, value)| {
            let v = value
                .to_str()
                .expect("emitted header value must be visible ASCII")
                .to_owned();
            (name.as_str().to_owned(), v)
        })
        .collect()
}

/// Decode a base64url(no-pad) metadata header into JSON.
fn decode_metadata(encoded: &str) -> serde_json::Value {
    let raw = URL_SAFE_NO_PAD
        .decode(encoded)
        .unwrap_or_else(|e| panic!("metadata header is not base64url(no-pad): {e}"));
    serde_json::from_slice(&raw)
        .unwrap_or_else(|e| panic!("metadata header does not decode to JSON: {e}"))
}

#[test]
fn produces_every_encodable_fixture_case() {
    let cases = load_cases();

    // A corpus that silently lost most of its files would otherwise "pass" on
    // whatever survived.
    assert!(
        cases.len() >= 15,
        "only {} envelope fixture cases loaded; the vendored corpus looks truncated",
        cases.len(),
    );

    let mut produced = 0_usize;
    let mut skipped = Vec::new();

    for case in &cases {
        // Skipping is driven purely by the case's own `direction`, never by a
        // hardcoded list here — a new case is covered the moment it is synced.
        if case.direction == "decode" {
            skipped.push(case.name.clone());
            continue;
        }
        assert!(
            case.direction == "roundtrip" || case.direction == "encode",
            "{}: unknown direction {:?}",
            case.name,
            case.direction,
        );

        // A lossy case encodes from `encode_from`; a round-tripping one from
        // its own envelope. The corpus reserves `encode_from` for lossy cases,
        // which are `direction: encode` — so a `roundtrip` case carrying one is
        // claiming `encode(envelope) == headers` while handing the producer a
        // different input, and whichever of the two it means, the case is not
        // saying it. Enforce it here rather than silently preferring
        // `encode_from`, which would let that inconsistency ride.
        assert!(
            case.encode_from.is_none() || case.direction == "encode",
            "{}: encode_from is reserved for lossy `encode` cases, but direction is {:?}",
            case.name,
            case.direction,
        );
        let logical = case.encode_from.as_ref().unwrap_or(&case.envelope);

        let mut headers = HeaderMap::new();
        inject_tapes_attribution(&mut headers, attribution_from(logical))
            .unwrap_or_else(|e| panic!("{}: inject failed: {e:?}", case.name));

        let got = tapes_headers(&headers);
        let want: BTreeMap<String, String> = case
            .headers
            .iter()
            .filter(|(name, _)| name.starts_with("x-tapes-"))
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();

        // Compare the header *sets* first: a missing or surplus header is a
        // clearer failure than a per-value mismatch on one of them.
        let got_names: Vec<&String> = got.keys().collect();
        let want_names: Vec<&String> = want.keys().collect();
        assert_eq!(
            got_names, want_names,
            "{}: emitted header set does not match the fixture",
            case.name,
        );

        for (name, want_value) in &want {
            let got_value = &got[name];
            if name == X_TAPES_HARNESS_METADATA {
                assert_eq!(
                    decode_metadata(got_value),
                    decode_metadata(want_value),
                    "{}: {name} decodes to different JSON",
                    case.name,
                );
            } else {
                assert_eq!(got_value, want_value, "{}: {name}", case.name);
            }
        }

        // The producer must not forge the server-trusted identity headers; the
        // cloud edge sets those from validated JWT claims.
        for name in headers.keys() {
            assert!(
                !name.as_str().starts_with("x-paper-auth-"),
                "{}: producer emitted server-trusted header {name}",
                case.name,
            );
        }

        produced += 1;
    }

    preserves_complete_inbound_envelopes(&cases);

    assert_eq!(
        produced + skipped.len(),
        cases.len(),
        "every case must be either produced or explicitly skipped",
    );
    assert!(
        produced >= 10,
        "only {produced} cases exercised the producer; skipped: {skipped:?}",
    );
}

/// The `unknown` harness-id is a distinct code path in
/// [`inject_tapes_attribution`] — it returns after one header rather than
/// walking the budget. The corpus's `unknown-bare` case pins the result, but
/// only in aggregate with everything else; assert the path directly so a
/// regression names itself.
#[test]
fn unknown_harness_case_emits_only_the_required_header() {
    let case = load_cases()
        .into_iter()
        .find(|c| c.name == "unknown-bare")
        .expect("corpus contains the unknown-bare case");

    let mut headers = HeaderMap::new();
    inject_tapes_attribution(&mut headers, attribution_from(&case.envelope)).unwrap();

    let got = tapes_headers(&headers);
    assert_eq!(got.len(), 1, "unknown harness attaches exactly one header");
    assert_eq!(got["x-tapes-harness-id"], HARNESS_ID_UNKNOWN);
}

/// Cases whose inbound headers already carry a complete envelope pin a
/// different contract from the rest of the corpus: the producer must leave
/// them alone.
///
/// The producer loop above cannot cover it. It reconstructs headers from the
/// parsed envelope via `inject_tapes_attribution`, which is the wrong entry
/// point — preservation is decided in `inject_tapes_headers`, by
/// `has_complete_inbound_tapes_envelope`, before any attribution is built. A
/// regression that broke complete-envelope detection would leave every
/// assertion above green while the producer silently overwrote a caller's identity
/// with `unknown`.
///
/// So drive the real entry point with the case's own inbound headers and a
/// `None` session — the non-Claude caller this contract exists for — and
/// require the X-Tapes-* set to come back untouched.
///
/// Selection is by shape, not by name: any case whose headers carry a usable
/// harness id and session id is a preservation case, so a future one is
/// covered the moment it is synced.
fn preserves_complete_inbound_envelopes(cases: &[FixtureCase]) {
    let mut checked = 0;

    for case in cases {
        let inbound: BTreeMap<String, String> = case
            .headers
            .iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
            .collect();

        let harness_id = inbound
            .get("x-tapes-harness-id")
            .map(|v| v.trim())
            .filter(|v| !v.is_empty() && *v != HARNESS_ID_UNKNOWN);
        let session_id = inbound
            .get("x-tapes-harness-session-id")
            .map(|v| v.trim())
            .filter(|v| !v.is_empty());
        if harness_id.is_none() || session_id.is_none() {
            continue;
        }

        let mut headers = HeaderMap::new();
        for (name, value) in &case.headers {
            let parsed_name = match http::HeaderName::from_bytes(name.as_bytes()) {
                Ok(n) => n,
                // A case may deliberately carry a header an HTTP stack would
                // reject; those are parser fixtures, not producer ones.
                Err(_) => continue,
            };
            let Ok(parsed_value) = http::HeaderValue::from_str(value) else {
                continue;
            };
            headers.insert(parsed_name, parsed_value);
        }
        let before = tapes_headers(&headers);
        if before.get("x-tapes-harness-id").map(String::as_str) != harness_id {
            // The header did not survive HeaderMap construction, so this case
            // is not exercising the preservation path.
            continue;
        }

        inject_tapes_headers(&mut headers, None, None)
            .unwrap_or_else(|e| panic!("{}: inject_tapes_headers failed: {e:?}", case.name));

        assert_eq!(
            tapes_headers(&headers),
            before,
            "{}: a complete inbound envelope must be preserved as-is",
            case.name,
        );
        checked += 1;
    }

    assert!(
        checked > 0,
        "no case exercised the complete-inbound-envelope preservation path; \
         the corpus should retain at least one (e.g. pi-complete)",
    );
}

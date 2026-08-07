//! The Rust half of the shared content-encoding decode contract.
//!
//! `crates/tapesctl/src/start/content_encoding.rs` implements a capture policy
//! — which content-codings a captured body may use, how stacked layers compose,
//! how much output is allowed, and what a corrupt or half-arrived stream is
//! worth. That policy is implemented a second time, independently, in Go
//! (`pkg/capture/contentencoding.go` in the tapes repository), because the same
//! bodies are captured on two paths: `tapesctl start` here and the AI Gateway
//! there. Capture fidelity is supposed to be identical on both.
//!
//! Until this file existed, "identical" was a claim by whoever last read both
//! implementations — and it had already decayed once. PCC-1126: this client
//! dropped every `content-encoding: zstd` request body (all of Codex/pi's
//! traffic) while the gateway route decoded the same bytes fine, with nothing
//! red anywhere. Both sides had tests; neither side had the *other side's*
//! tests.
//!
//! So the tests here are not this repository's. They are a vendored copy of the
//! corpus authored in tapes at `fixtures/content-encoding/`, table-run against
//! the real decoder. A policy change that lands on one side and not the other
//! now turns this file red rather than turning a session silently empty.
//!
//! Three gates, mirroring the authored home:
//!
//!  1. the oracle — every case decodes to its declared outcome;
//!  2. the DIGEST seal — recomputed here, so a stale or hand-edited vendored
//!     copy fails in *this* repository's CI rather than passing quietly against
//!     cases nobody upstream still has;
//!  3. the shape gate — the vendored copy is the corpus it claims to be
//!     (every case named after its file, every category known).
//!
//! What this file deliberately does NOT assert: `expect.error.detail` and
//! `contested`. Both are prose for the reader — `detail` names a sub-reason
//! inside a class the corpus keeps deliberately coarse, and `contested` records
//! an argument rather than a rule. Asserting either would pin one
//! implementation's phrasing as the contract.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tapesctl::start::content_encoding::{DecodeError, decode_content_encoding};

/// Where the vendored corpus lives. Refreshed by
/// `scripts/sync-content-encoding-fixtures.sh`; never hand-edited (the seal
/// below is what enforces that).
fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("vendor/tapes-content-encoding-fixtures")
}

/// Case files, sorted by base name — the same order the DIGEST is sealed in.
fn case_files() -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(corpus_dir().join("cases"))
        .expect("vendored corpus must have a cases/ directory")
        .map(|entry| entry.expect("readable dir entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no cases under {:?}", corpus_dir());
    files
}

// --- the case schema, mirroring vendor/.../README.upstream.md ---------------
//
// Every struct denies unknown fields, exactly as the Go loader does with
// DisallowUnknownFields. An unknown field is a case written against a schema
// this consumer does not implement, and ignoring it would let a case assert
// something no one here checks — which is the failure mode this whole corpus
// exists to prevent, reintroduced one level up.

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RepeatUtf8 {
    text: String,
    count: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RepeatByte {
    byte: u16,
    count: usize,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlaintextSpec {
    #[serde(default)]
    utf8: Option<String>,
    #[serde(default)]
    repeat_utf8: Option<RepeatUtf8>,
    #[serde(default)]
    repeat_byte: Option<RepeatByte>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TruncateSpec {
    #[serde(default)]
    drop_tail_bytes: Option<usize>,
    /// An absolute prefix, for cut points derived from the container format
    /// rather than from one encoder's output length.
    #[serde(default)]
    keep_head_bytes: Option<usize>,
    /// `[num, den]`: keep `len * num / den` bytes, integer division.
    #[serde(default)]
    keep_head_ratio: Option<[usize; 2]>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BuildSpec {
    plaintext: PlaintextSpec,
    /// Codings to apply, left-to-right, in the same order the header lists
    /// them. Building is left-to-right; decoding peels right-to-left.
    layers: Vec<String>,
    /// Encode the plaintext as this many independently-encoded, concatenated
    /// streams instead of one. Absent means one — an ordinary body.
    #[serde(default)]
    members: Option<usize>,
    #[serde(default)]
    truncate: Option<TruncateSpec>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BodySpec {
    #[serde(default)]
    bytes_b64: Option<String>,
    #[serde(default)]
    build: Option<BuildSpec>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DecodedSpec {
    #[serde(default)]
    equals_plaintext: bool,
    #[serde(default)]
    bytes_b64: Option<String>,
    #[serde(default)]
    nonempty_prefix_of_plaintext: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ErrorSpec {
    class: String,
    #[serde(default)]
    message_contains: Vec<String>,
    /// Read but never asserted: the sub-reason inside a deliberately coarse
    /// class, phrased for whichever implementation authored the case.
    #[serde(default)]
    #[allow(dead_code)]
    detail: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpectSpec {
    outcome: String,
    #[serde(default)]
    decoded: Option<DecodedSpec>,
    #[serde(default)]
    error: Option<ErrorSpec>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EncodingCase {
    name: String,
    category: String,
    description: String,
    /// `null` means the header was absent; `""` means present and empty. The
    /// decoder takes `Option<&str>` and must treat the two the same.
    encoding: Option<String>,
    body: BodySpec,
    expect: ExpectSpec,
    #[allow(dead_code)]
    grounding: String,
    /// Read but never asserted: an argument that travels with the case.
    #[serde(default)]
    #[allow(dead_code)]
    contested: Option<serde_json::Value>,
    #[serde(default)]
    #[allow(dead_code)]
    notes: Option<String>,
}

fn load_cases() -> Vec<(String, EncodingCase)> {
    case_files()
        .into_iter()
        .map(|path| {
            let file = path
                .file_name()
                .and_then(|name| name.to_str())
                .expect("case file name")
                .to_owned();
            let raw = fs::read_to_string(&path).expect("readable case file");
            let case: EncodingCase = serde_json::from_str(&raw)
                .unwrap_or_else(|err| panic!("{file}: does not match the case schema: {err}"));
            (file, case)
        })
        .collect()
}

// --- building a case's bytes ------------------------------------------------

fn build_plaintext(file: &str, spec: &PlaintextSpec) -> Vec<u8> {
    let mut forms: Vec<Vec<u8>> = Vec::new();
    if let Some(text) = &spec.utf8 {
        forms.push(text.as_bytes().to_vec());
    }
    if let Some(repeat) = &spec.repeat_utf8 {
        forms.push(repeat.text.as_bytes().repeat(repeat.count));
    }
    if let Some(repeat) = &spec.repeat_byte {
        let byte = u8::try_from(repeat.byte)
            .unwrap_or_else(|_| panic!("{file}: repeat_byte.byte must be 0-255"));
        forms.push(vec![byte; repeat.count]);
    }
    assert_eq!(
        forms.len(),
        1,
        "{file}: plaintext must set exactly one form"
    );
    forms.remove(0)
}

/// Apply one content-coding. Deliberately uses the same encoders the decoder
/// under test decodes with: the corpus asserts that a gzip stream of X decodes
/// to X, not that two compressors emit identical bytes — which they do not, and
/// which is why the corpus ships recipes rather than blobs.
fn apply_layer(file: &str, body: &[u8], layer: &str) -> Vec<u8> {
    match layer {
        "gzip" => {
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(body).expect("gzip write");
            encoder.finish().expect("gzip finish")
        }
        "zstd" => zstd::encode_all(body, 3).expect("zstd encode"),
        other => panic!("{file}: case recipe names a layer this consumer cannot build: {other:?}"),
    }
}

/// Cut a plaintext into the chunks a case asked to be encoded independently,
/// defaulting to one.
///
/// Chunks are as near equal as integer division allows, with the remainder on
/// the last. Every chunk must be non-empty: an empty member is a different case
/// — an empty stream inside a stream — and would make "two members" quietly
/// mean "one member and a zero-byte one".
fn split_into_members<'a>(
    file: &str,
    plaintext: &'a [u8],
    members: Option<usize>,
) -> Vec<&'a [u8]> {
    let n = members.unwrap_or(1);
    assert!(n >= 1, "{file}: members must be at least 1");
    assert!(
        plaintext.len() >= n,
        "{file}: {n} members needs at least {n} plaintext bytes, got {}",
        plaintext.len(),
    );

    let size = plaintext.len() / n;
    (0..n)
        .map(|i| {
            let start = i * size;
            let end = if i == n - 1 {
                plaintext.len()
            } else {
                start + size
            };
            &plaintext[start..end]
        })
        .collect()
}

/// The wire bytes for a case, plus the plaintext they were built from (`None`
/// for a `bytes_b64` case, which has no plaintext).
fn build_body(file: &str, case: &EncodingCase) -> (Vec<u8>, Option<Vec<u8>>) {
    match (&case.body.bytes_b64, &case.body.build) {
        (Some(encoded), None) => {
            let bytes = BASE64
                .decode(encoded)
                .unwrap_or_else(|err| panic!("{file}: bytes_b64: {err}"));
            (bytes, None)
        }
        (None, Some(build)) => {
            let plaintext = build_plaintext(file, &build.plaintext);
            // A multi-member body is the same plaintext encoded in more than
            // one go. The split is over the PLAINTEXT, not the encoded bytes,
            // so the member boundary lands at the same logical offset for
            // every compressor and the case can still assert equality with the
            // whole plaintext.
            let mut body = Vec::new();
            for chunk in split_into_members(file, &plaintext, build.members) {
                let mut encoded = chunk.to_vec();
                for layer in &build.layers {
                    encoded = apply_layer(file, &encoded, layer);
                }
                body.extend_from_slice(&encoded);
            }
            if let Some(truncate) = &build.truncate {
                body = apply_truncation(file, body, truncate);
            }
            (body, Some(plaintext))
        }
        _ => panic!("{file}: body must set exactly one of bytes_b64 or build"),
    }
}

/// Cut the *encoded* bytes, after every layer has been applied.
fn apply_truncation(file: &str, body: Vec<u8>, truncate: &TruncateSpec) -> Vec<u8> {
    match (
        truncate.drop_tail_bytes,
        truncate.keep_head_bytes,
        truncate.keep_head_ratio,
    ) {
        (Some(drop), None, None) => {
            assert!(
                body.len() > drop,
                "{file}: drop_tail_bytes {drop} would empty a {}-byte body",
                body.len(),
            );
            body[..body.len() - drop].to_vec()
        }
        (None, Some(keep), None) => {
            // It must actually cut: a keep count at or past the encoded length
            // would silently turn the case into an untruncated one.
            assert!(
                body.len() > keep,
                "{file}: keep_head_bytes must be shorter than the {}-byte encoded body",
                body.len(),
            );
            body[..keep].to_vec()
        }
        (None, None, Some([num, den])) => {
            assert!(den > 0, "{file}: keep_head_ratio denominator must be > 0");
            // Integer division on the encoded length, which differs per
            // compressor — which is why a ratio-truncated case can only assert
            // a property of its output, never a length.
            body[..body.len() * num / den].to_vec()
        }
        _ => panic!("{file}: truncate must set exactly one form"),
    }
}

/// Map this implementation's error onto the corpus's three-class taxonomy.
///
/// The classes exist because both implementations already distinguish exactly
/// these three — Rust structurally, Go by message — so pinning them costs
/// nothing and catches real drift: a decoder that starts passing an unknown
/// coding through instead of refusing it still fails, because `unsupported` is
/// not `decoded`.
fn classify(error: &DecodeError) -> &'static str {
    match error {
        DecodeError::Unsupported { .. } => "unsupported",
        DecodeError::TooLarge { .. } => "oversize",
        DecodeError::Read { .. } => "undecodable",
    }
}

/// Where the other implementation of this policy lives. Named in every failure
/// so a genuine policy change lands on both sides rather than only here.
const OTHER_HOME: &str = "tapes pkg/capture/contentencoding.go";

// --- gate 1: the oracle -----------------------------------------------------

/// The one case this implementation does not satisfy, named rather than
/// silently tolerated.
///
/// `divergence-empty-body-under-zstd` records — as *observed*, and flagged
/// upstream as a suspected bug rather than promoted to a rule — that Go's zstd
/// reader returns success with zero bytes for a zero-byte body, while its gzip
/// reader errors on the same input. The case's own `contested.open` asks a Rust
/// consumer to report what its binding does instead of assuming.
///
/// It errors: libzstd's streaming decoder calls a zero-byte input an incomplete
/// frame. So this is the corpus's first genuine cross-language divergence, and
/// the right place to resolve it is the reference implementation (make gzip and
/// zstd agree), not here — teaching either decoder to return empty-for-empty to
/// make a test green would silently swallow a body that was lost in flight,
/// which is the failure the whole corpus exists to make loud.
///
/// Skipping it here is therefore deliberate and bounded: the exact behaviour
/// this implementation has on that input is pinned by
/// [`an_empty_body_is_an_error_under_both_codings_here`] below, so the skip
/// cannot hide a later change on either side. When upstream resolves the pair,
/// that test goes red, this list empties, and the case rejoins the oracle.
const KNOWN_DIVERGENCE: &str = "divergence-empty-body-under-zstd.json";

#[test]
fn every_case_decodes_to_its_declared_outcome() {
    let cases = load_cases();
    assert_eq!(cases.len(), 27, "the vendored corpus is 27 cases");
    assert!(
        cases.iter().any(|(file, _)| file == KNOWN_DIVERGENCE),
        "the known-divergence exemption names a case that is no longer in the corpus; \
         delete the exemption",
    );

    // Every case is checked, and the failures are reported together. A corpus
    // consumer that stopped at the first red case would report one drifted rule
    // at a time across as many runs as there are rules.
    let mut failed: Vec<String> = Vec::new();
    for (file, case) in &cases {
        if file == KNOWN_DIVERGENCE {
            continue;
        }
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            check_case(file, case);
        }));
        if outcome.is_err() {
            failed.push(file.clone());
        }
    }
    assert!(
        failed.is_empty(),
        "{} of {} content-encoding cases do not hold against this decoder: {failed:?}\n  \
         Each is a rule the two capture paths no longer agree on; the panic output above\n  \
         says which. A genuine policy change belongs in {OTHER_HOME} too.",
        failed.len(),
        cases.len(),
    );
}

fn check_case(file: &str, case: &EncodingCase) {
    let (body, plaintext) = build_body(file, case);
    let result = decode_content_encoding(&body, case.encoding.as_deref());

    match case.expect.outcome.as_str() {
        outcome @ ("decoded" | "salvaged") => {
            let got = result.unwrap_or_else(|err| {
                panic!(
                    "{file}: case expects {outcome}, got error: {err}\n  \
                     This corpus is the shared capture-decode policy. If the policy\n  \
                     genuinely changed, the same change belongs in {OTHER_HOME}."
                )
            });
            assert_eq!(
                got.truncated,
                outcome == "salvaged",
                "{file}: a salvaged decode must be reported as truncated, \
                 and a clean one must not be",
            );

            let decoded = case
                .expect
                .decoded
                .as_ref()
                .unwrap_or_else(|| panic!("{file}: a non-error outcome must declare decoded"));
            assert_decoded(file, decoded, got.bytes.as_ref(), plaintext.as_deref());
        }

        "error" => {
            let err = result.err().unwrap_or_else(|| {
                panic!(
                    "{file}: case expects an error, got a successful decode.\n  \
                     A decoder that started accepting this input silently changed the\n  \
                     capture policy; the same change belongs in {OTHER_HOME}."
                )
            });
            let expected = case
                .expect
                .error
                .as_ref()
                .unwrap_or_else(|| panic!("{file}: an error outcome must declare error"));
            assert!(
                ["unsupported", "oversize", "undecodable"].contains(&expected.class.as_str()),
                "{file}: unknown error class {:?}",
                expected.class,
            );
            assert_eq!(
                classify(&err),
                expected.class,
                "{file}: wrong failure class for {err}",
            );
            let message = err.to_string();
            for want in &expected.message_contains {
                assert!(
                    message.contains(want),
                    "{file}: error message must contain {want:?}, got {message:?}",
                );
            }
        }

        other => panic!("{file}: unknown outcome {other:?}"),
    }
}

/// What this implementation actually does on a zero-byte body — the answer
/// `divergence-empty-body-under-zstd`'s `contested.open` asks a Rust consumer
/// for.
///
/// Both codings error here. Go's gzip reader errors too, but its zstd reader
/// returns success with zero bytes, so the pair that upstream describes as
/// "identical inputs, opposite outcomes, decided by which decoder the header
/// happened to name" is a Go-internal inconsistency that this implementation
/// does not reproduce: here the two codings already agree, and they agree on
/// the answer the corpus's `contested` block argues is the right one.
///
/// This test is the reason the oracle above may skip that case. It pins the
/// divergence from both ends: it goes red if this decoder ever starts accepting
/// an empty body, and the oracle's exemption goes stale the moment upstream
/// changes the case. Neither implementation is bent to make the other's test
/// green — see the case file's own `contested` block for why that would be the
/// wrong repair.
#[test]
fn an_empty_body_is_an_error_under_both_codings_here() {
    for coding in ["gzip", "zstd"] {
        let err = decode_content_encoding(b"", Some(coding))
            .err()
            .unwrap_or_else(|| panic!("{coding}: an empty body must not decode"));
        assert_eq!(
            classify(&err),
            "undecodable",
            "{coding}: an empty body is an unreadable stream, not an unsupported \
             coding or a bomb ({err})",
        );
    }

    // And the caller precondition that makes the whole question unreachable in
    // production, stated in `contested-empty-body-under-gzip`: a bodiless
    // request must never reach the decoder at all. An empty body with no
    // encoding claimed is not a decode failure — it is nothing to do.
    let got = decode_content_encoding(b"", None).expect("no coding claimed, nothing to undo");
    assert!(got.bytes.is_empty() && !got.truncated);
}

fn assert_decoded(file: &str, spec: &DecodedSpec, got: &[u8], plaintext: Option<&[u8]>) {
    match spec {
        DecodedSpec {
            equals_plaintext: true,
            bytes_b64: None,
            nonempty_prefix_of_plaintext: false,
        } => {
            let plaintext = plaintext
                .unwrap_or_else(|| panic!("{file}: equals_plaintext needs a build recipe"));
            assert_eq!(
                got.len(),
                plaintext.len(),
                "{file}: decoded length differs from the plaintext",
            );
            assert!(got == plaintext, "{file}: decoded bytes differ");
        }
        DecodedSpec {
            equals_plaintext: false,
            bytes_b64: Some(want),
            nonempty_prefix_of_plaintext: false,
        } => {
            let want = BASE64
                .decode(want)
                .unwrap_or_else(|err| panic!("{file}: decoded.bytes_b64: {err}"));
            assert_eq!(got, want.as_slice(), "{file}: decoded bytes differ");
        }
        DecodedSpec {
            equals_plaintext: false,
            bytes_b64: None,
            nonempty_prefix_of_plaintext: true,
        } => {
            let plaintext = plaintext.unwrap_or_else(|| {
                panic!("{file}: nonempty_prefix_of_plaintext needs a build recipe")
            });
            assert!(
                !got.is_empty(),
                "{file}: a salvage must produce output to be a salvage",
            );
            assert!(
                plaintext.starts_with(got),
                "{file}: a salvaged body must be a prefix of the original, \
                 not a corruption of it",
            );
        }
        _ => panic!("{file}: decoded must set exactly one form"),
    }
}

// --- gate 2: the seal -------------------------------------------------------

/// Recompute the corpus seal.
///
/// The DIGEST is authored upstream and vendored alongside `cases/`, so this is
/// what makes a *stale or hand-edited* vendored copy fail here rather than in
/// somebody else's repository. Without it, a case edited locally to make a red
/// test green would pass, and the two implementations would have quietly
/// stopped testing the same policy — the exact failure PCC-1126 was.
#[test]
fn the_vendored_corpus_matches_its_sealed_digest() {
    // For each cases/*.json, sorted by base name, feed
    // "<basename>  <sha256-hex-of-file-bytes>\n" into a SHA-256; the digest is
    // "sha256:" + hex of that hash. Same rule as fixtures/envelope/DIGEST and
    // fixtures/thread/DIGEST upstream.
    let mut outer = Sha256::new();
    for path in case_files() {
        let bytes = fs::read(&path).expect("readable case file");
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .expect("file name");
        outer.update(format!("{name}  {:x}\n", Sha256::digest(&bytes)));
    }
    let recomputed = format!("sha256:{:x}", outer.finalize());

    let sealed = fs::read_to_string(corpus_dir().join("DIGEST")).expect("vendored DIGEST");
    assert_eq!(
        recomputed,
        sealed.trim(),
        "vendored content-encoding corpus digest mismatch.\n  \
         The cases here are not the cases the sealed DIGEST covers, which means this\n  \
         copy was hand-edited or synced partially. Re-run\n  \
         scripts/sync-content-encoding-fixtures.sh against a tapes checkout at the\n  \
         SHA recorded in vendor/tapes-content-encoding-fixtures/SOURCE.md — and if the\n  \
         corpus genuinely changed upstream, sync the DIGEST with it and land the\n  \
         decoder change the new cases force in the same commit.",
    );
}

// --- gate 3: the shape ------------------------------------------------------

/// The vendored copy is the corpus it claims to be.
///
/// Cheap, but it is what catches a case file renamed on the way in — which the
/// seal would report as a digest mismatch without saying why, and which the
/// oracle would not notice at all.
#[test]
fn every_case_is_named_after_its_file_and_declares_a_known_category() {
    for (file, case) in load_cases() {
        assert_eq!(
            format!("{}.json", case.name),
            file,
            "case name must match its filename",
        );
        assert!(
            !case.description.is_empty(),
            "{file}: description is required",
        );
        assert!(!case.grounding.is_empty(), "{file}: grounding is required");
        assert!(
            [
                "identity",
                "supported",
                "stacked",
                "salvage",
                "limit",
                "error"
            ]
            .contains(&case.category.as_str()),
            "{file}: unknown category {:?}",
            case.category,
        );
    }
}

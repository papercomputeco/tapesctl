//! The Rust half of the shared turn-eligibility contract.
//!
//! `crates/tapesctl/src/start/turn_policy.rs` decides whether an exchange is a
//! turn at all: was the request turn-shaped on a provider chat-completion
//! endpoint, and did the upstream complete it. That policy is implemented a
//! second time, independently, in Go (`extproc/processor.go` in the tapes
//! repository), because the same traffic is captured on two paths — `tapesctl
//! start` here and the AI Gateway there. Capture fidelity is supposed to be
//! identical on both, and these two rules decide which exchanges the rest of
//! the pipeline ever sees.
//!
//! For a long time it was not identical, and nothing said so: the gateway
//! applied both gates and this client applied neither. The corpus recorded that
//! as a known divergence rather than papering over it, which is how it came to
//! be closed — and this file is what stops it reopening. The tests here are not
//! this repository's; they are a vendored copy of the corpus authored in tapes
//! at `fixtures/drop-reason/`, table-run against the real predicates.
//!
//! Four gates, mirroring the authored home:
//!
//!  1. the oracle — every case's executable examples hold against the predicate
//!     the proxy actually runs;
//!  2. the DIGEST seal — recomputed here, so a stale or hand-edited vendored
//!     copy fails in *this* repository's CI rather than passing quietly against
//!     cases nobody upstream still has;
//!  3. the shape gate — the vendored copy is the corpus it claims to be, and
//!     every case carries the input block its reason's predicate reads;
//!  4. vocabulary — every reason this client names is specified upstream, is
//!     classified `policy`, and is spelled exactly as the corpus spells it.
//!
//! Gate 4 is deliberately one-directional; `vendor/tapes-drop-reason-fixtures/SOURCE.md`
//! says why, and what that leaves unasserted.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest, Sha256};
use tapesctl::start::turn_policy::{
    DROP_NON_TURN_REQUEST, DROP_UPSTREAM_STATUS, is_capturable_turn_request,
    is_capturable_upstream_status,
};

/// Where the vendored corpus lives. Refreshed by
/// `scripts/sync-drop-reason-fixtures.sh`; never hand-edited (the seal below is
/// what enforces that).
fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("vendor/tapes-drop-reason-fixtures")
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
// something no one here checks.

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExampleRequest {
    method: String,
    path: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExampleResponse {
    status: u16,
}

/// One executable example. It carries exactly the input block its reason's
/// predicate reads — `request` for a rule over the request line, `response` for
/// one over the upstream status — and the block is what says which predicate
/// the example is an example *of*.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Example {
    description: String,
    #[serde(default)]
    request: Option<ExampleRequest>,
    #[serde(default)]
    response: Option<ExampleResponse>,
    expect: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DropReasonCase {
    name: String,
    class: String,
    /// The Go constant carrying this reason upstream. Read but not asserted:
    /// this implementation has no Go constants, and pinning one repo's
    /// identifier spelling from another is not a contract either side can keep.
    #[allow(dead_code)]
    constant: String,
    #[allow(dead_code)]
    summary: String,
    #[allow(dead_code)]
    trigger: String,
    #[allow(dead_code)]
    grounding: String,
    #[serde(default)]
    examples: Vec<Example>,
    /// Why this reason carries no examples. Prose for the reader; asserted only
    /// as "present exactly when examples are absent".
    #[serde(default)]
    not_expressible: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    notes: Option<String>,
}

/// The two values an example's `expect` may take. Named because the corpus
/// spells them out once per example and a typo would silently weaken an
/// assertion rather than fail it.
const EXPECT_ELIGIBLE: &str = "eligible";
const EXPECT_DROPPED: &str = "dropped";

fn load_cases() -> BTreeMap<String, DropReasonCase> {
    case_files()
        .into_iter()
        .map(|path| {
            let file = path.file_name().unwrap().to_str().unwrap().to_owned();
            let bytes = fs::read(&path).expect("readable case file");
            let case: DropReasonCase = serde_json::from_slice(&bytes).unwrap_or_else(|err| {
                panic!("{file}: {err}\n  the vendored schema and this consumer's have drifted")
            });
            (file, case)
        })
        .collect()
}

// --- gate 1: the oracle -----------------------------------------------------

/// Every executable example, against the predicate the proxy runs.
///
/// A reason with no evaluator here must declare `not_expressible`. That is the
/// same rule the authored home applies, and it is what keeps an example from
/// being added upstream that this side silently does not run — which would look
/// exactly like agreement.
#[test]
fn every_example_holds_against_this_implementation() {
    let mut executed = 0_usize;

    for (file, case) in load_cases() {
        for example in &case.examples {
            let want_eligible = match example.expect.as_str() {
                EXPECT_ELIGIBLE => true,
                EXPECT_DROPPED => false,
                other => panic!("{file}: unknown expect {other:?}"),
            };

            let got = match case.name.as_str() {
                DROP_NON_TURN_REQUEST => {
                    let request = example
                        .request
                        .as_ref()
                        .unwrap_or_else(|| panic!("{file}: {} needs a request", case.name));
                    is_capturable_turn_request(&request.method, &request.path)
                }
                DROP_UPSTREAM_STATUS => {
                    let response = example
                        .response
                        .as_ref()
                        .unwrap_or_else(|| panic!("{file}: {} needs a response", case.name));
                    is_capturable_upstream_status(response.status)
                }
                other => panic!(
                    "{file}: {other} carries examples but has no evaluator here.\n  \
                     Either this client grew the gate and owes it a branch, or the case \
                     owes a not_expressible."
                ),
            };

            assert_eq!(
                got, want_eligible,
                "{file}: {}\n  This is shared capture policy, and it is specified upstream at \
                 tapes fixtures/drop-reason/.\n  If it genuinely changed, the same change belongs \
                 in the other capture path too: tapes extproc/processor.go.",
                example.description,
            );
            executed += 1;
        }
    }

    // A corpus that stopped carrying examples would make every assertion above
    // vacuous, and this file would go green having tested nothing.
    assert!(
        executed >= 20,
        "only {executed} examples ran; the vendored corpus has lost its executable cases",
    );
}

// --- gate 2: the seal -------------------------------------------------------

/// Recompute the corpus seal.
///
/// The DIGEST is authored upstream and vendored alongside `cases/`, so this is
/// what makes a *stale or hand-edited* vendored copy fail here rather than in
/// somebody else's repository. Without it, a case edited locally to make a red
/// test green would pass, and the two implementations would have quietly
/// stopped testing the same policy.
#[test]
fn the_vendored_corpus_matches_its_sealed_digest() {
    // For each cases/*.json, sorted by base name, feed
    // "<basename>  <sha256-hex-of-file-bytes>\n" into a SHA-256; the digest is
    // "sha256:" + hex of that hash. Same rule as the sibling corpora.
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
        "vendored drop-reason corpus digest mismatch.\n  \
         The cases here are not the cases the sealed DIGEST covers, which means this\n  \
         copy was hand-edited or synced partially. Re-run\n  \
         scripts/sync-drop-reason-fixtures.sh against a tapes checkout at the SHA\n  \
         recorded in vendor/tapes-drop-reason-fixtures/SOURCE.md — and if the corpus\n  \
         genuinely changed upstream, sync the DIGEST with it and land the gate change\n  \
         the new cases force in the same commit.",
    );
}

// --- gate 3: the shape ------------------------------------------------------

/// The vendored copy is the corpus it claims to be.
///
/// Cheap, but it is what catches a case file renamed on the way in — which the
/// seal would report as a digest mismatch without saying why, and which the
/// oracle would not notice at all.
#[test]
fn every_case_is_named_after_its_file_and_classified() {
    for (file, case) in load_cases() {
        assert_eq!(
            format!("{}.json", case.name),
            file,
            "a case's name must match its filename: the name is the reason's wire string",
        );
        assert!(
            matches!(case.class.as_str(), "policy" | "transport"),
            "{file}: unknown class {:?}",
            case.class,
        );
        assert_eq!(
            case.examples.is_empty(),
            case.not_expressible.is_some(),
            "{file}: a case must carry examples or not_expressible, and not both",
        );
    }
}

/// Each example carries exactly the input block its reason reads.
///
/// Both blocks, or the wrong one, means an example whose predicate is ambiguous
/// — and an example nobody can run is the failure this corpus exists to prevent,
/// reintroduced one level up.
#[test]
fn every_example_carries_exactly_the_input_its_reason_reads() {
    for (file, case) in load_cases() {
        for example in &case.examples {
            assert!(
                !example.description.is_empty(),
                "{file}: every example needs a description",
            );
            match case.name.as_str() {
                DROP_NON_TURN_REQUEST => {
                    assert!(example.request.is_some(), "{file}: expected a request");
                    assert!(
                        example.response.is_none(),
                        "{file}: {} reads the request line, not the status",
                        case.name,
                    );
                }
                DROP_UPSTREAM_STATUS => {
                    assert!(example.response.is_some(), "{file}: expected a response");
                    assert!(
                        example.request.is_none(),
                        "{file}: {} reads the status, not the request line",
                        case.name,
                    );
                    // 0 is missing_status — a transport reason, and never an
                    // input this predicate is asked about.
                    assert_ne!(
                        example.response.as_ref().unwrap().status,
                        0,
                        "{file}: a status of 0 is missing_status, not {}",
                        case.name,
                    );
                }
                other => panic!("{file}: {other} carries examples with no evaluator here"),
            }
        }
    }
}

// --- gate 4: vocabulary -----------------------------------------------------

/// Every reason this client names is one the corpus specifies, as policy, with
/// this exact spelling.
///
/// The spelling is the point. These strings are wire-visible — a log field
/// here, a metric label value on the gateway half — so two implementations
/// agreeing on a rule and disagreeing on its name still produce two
/// vocabularies, and a dashboard written against one of them is silently blind
/// to the other.
#[test]
fn every_reason_this_client_names_is_specified_as_policy() {
    let cases = load_cases();
    let by_name: BTreeMap<&str, &DropReasonCase> =
        cases.values().map(|c| (c.name.as_str(), c)).collect();

    for reason in [DROP_NON_TURN_REQUEST, DROP_UPSTREAM_STATUS] {
        let case = by_name.get(reason).unwrap_or_else(|| {
            panic!(
                "this client drops turns for {reason:?}, which the corpus does not specify.\n  \
                 A reason nobody classified is a reason the next implementation guesses about: \
                 add it upstream, in fixtures/drop-reason/, before naming it here."
            )
        });
        assert_eq!(
            case.class, "policy",
            "{reason} is specified as {:?}, not policy. Applying a transport reason as a \
             capture rule makes one deployment's plumbing everyone's contract.",
            case.class,
        );
    }
}

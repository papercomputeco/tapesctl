//! What this CLI exposes of the vendored core contract.
//!
//! # The contract itself lives in the shared crate
//!
//! The vendored `tapes-api.yaml`, its provenance, the reduction of it into an
//! operation table, and the request assembly that follows — resolve an
//! operation, route each value to the location the document declared for it,
//! refuse an undeclared parameter, refuse a path placeholder with no value —
//! are all [`tapes_client::core`] since the read surface moved out of this
//! repository. Both clients that speak this API build against one copy of the
//! published asset instead of a copy each, and the seal check runs in that
//! crate's CI.
//!
//! What is left here is the part that is genuinely tapesctl's: the coverage
//! tables.
//!
//! # The coverage gate
//!
//! Vendoring a contract invites a quieter failure than drift: an operation the
//! server grows that this CLI silently never exposes. [`EXPOSED_OPERATIONS`]
//! and [`UNEXPOSED_OPERATIONS`] partition every `operationId` in the vendored
//! document, and a test fails — naming the unmapped operations — the moment a
//! contract bump adds one that is in neither list. Being in
//! [`UNEXPOSED_OPERATIONS`] is a deliberate, reviewed decision with a recorded
//! reason, not a default.
//!
//! The tables stay here rather than moving with the contract because they are
//! a statement about *this CLI's* surface: another client exposes a different
//! set, and a shared table would report on the union of two surfaces and
//! silently stop protecting whichever one differs.

/// The vendored ingest contract. Nothing at runtime reads it — the capture
/// path keeps its hand-written request construction — but the conformance
/// tests in `tests/ingest_conformance.rs` hold that construction to it.
///
/// It stays vendored here, unlike the read contract, because this is its only
/// consumer: no other client vendors it and nothing at runtime reads it.
pub const TAPES_INGEST_YAML: &str = include_str!("../../contracts/tapes-ingest.yaml");

// The operation ids and the reduction of the vendored document, re-exported so
// that a command names an operation through this module — the one that also
// holds the coverage tables judging that name. Request assembly is not
// re-exported any more: no call site builds a request by hand since the shared
// `CoreClient` began doing it.
pub use tapes_client::core::{core, ops};

/// Every vendored operation the CLI drives, and the surface that drives it.
///
/// The second column is prose for the failure message and the reviewer; the
/// first is what the gate checks.
pub const EXPOSED_OPERATIONS: &[(&str, &str)] = &[
    (ops::LIST_SESSIONS, "tapesctl sessions list"),
    (ops::GET_SESSION, "tapesctl sessions get"),
    (ops::GET_SESSION_TRACES, "tapesctl sessions traces"),
    (ops::LIST_RAW_TURNS, "tapesctl sessions raw-turns"),
    (ops::EXPORT_SESSION, "tapesctl export"),
    (ops::LIST_TRACES, "tapesctl traces list"),
    (
        ops::GET_TRACE,
        "tapesctl traces get, and the spans-list projection",
    ),
    (ops::GET_SPAN, "tapesctl spans get"),
    (ops::SEARCH_SPANS, "tapesctl search"),
    (ops::SEED_DEMO, "tapesctl seed"),
    (
        ops::LIST_CASSETTES,
        "cassette discovery, which generates the <cassette> <method> surface",
    ),
];

/// Vendored operations this CLI deliberately does not expose, each with the
/// reason on record. An entry here is an allow-list decision, not a backlog
/// dump: removing a reason that has stopped being true is how an operation
/// graduates to [`EXPOSED_OPERATIONS`].
pub const UNEXPOSED_OPERATIONS: &[(&str, &str)] = &[
    (
        "ping",
        "liveness probe for orchestrators; a CLI health verb has not been asked for",
    ),
    (
        "runDerive",
        "operator re-derive; a data-mutating admin sweep does not belong on the read CLI yet",
    ),
    (
        "repairRawTurnAttribution",
        "operator attribution repair; same admin-surface reasoning as runDerive",
    ),
    (
        "openMcpStream",
        "MCP transport; spoken by MCP clients over a stream, not callable as a one-shot command",
    ),
    ("invokeMcp", "MCP transport; see openMcpStream"),
    ("closeMcpSession", "MCP transport; see openMcpStream"),
    (
        "deleteSession",
        "destructive write; the core surface is read-only today and a delete verb needs its own design",
    ),
    (
        "updateSession",
        "session rename/edit; not ported from the Go CLI, which never had it either",
    ),
    (
        "exportSessions",
        "bulk export window; tapesctl export is per-session today, the bulk port is future work",
    ),
    (
        "getStats",
        "aggregate stats; not ported from the Go CLI yet",
    ),
    (
        "listSessionSkills",
        "core's copy of a surface the skills cassette owns; tapesctl reaches skills through the \
         discovered `cassettes skills` commands, and these routes are deleted from core with the \
         cassette cutover's batched removal",
    ),
    (
        "listSkills",
        "core's copy of a cassette-owned surface; see listSessionSkills",
    ),
    (
        "createSkill",
        "core's copy of a cassette-owned surface; see listSessionSkills",
    ),
    (
        "getSkill",
        "core's copy of a cassette-owned surface; see listSessionSkills",
    ),
    (
        "updateSkill",
        "core's copy of a cassette-owned surface; see listSessionSkills",
    ),
    (
        "deleteSkill",
        "core's copy of a cassette-owned surface; see listSessionSkills",
    ),
    (
        "duplicateSkill",
        "core's copy of a cassette-owned surface; see listSessionSkills",
    ),
    (
        "getSkillMarkdown",
        "core's copy of a cassette-owned surface; see listSessionSkills",
    ),
    (
        "listSkillVersions",
        "core's copy of a cassette-owned surface; see listSessionSkills",
    ),
    (
        "publishSkill",
        "core's copy of a cassette-owned surface; see listSessionSkills",
    ),
    (
        "generateSkill",
        "core's copy of a cassette-owned surface; see listSessionSkills",
    ),
];

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::collections::BTreeSet;
    use tapes_client::core::call_for;
    use tapes_client::core::coverage;

    #[test]
    fn the_vendored_contract_parses_and_reduces() {
        // The one place a corrupt vendored document is allowed to fail loudly.
        let surface = core().expect("the vendored tapes-api contract must parse");
        assert!(surface.operation_ids().count() > 0);
    }

    #[test]
    fn the_ingest_contract_parses_too() {
        // Runtime never reads it, but the conformance tests do — a corrupt
        // vendored copy should fail here, not in a test that looks like an
        // ingest regression.
        let document: Value = serde_yaml::from_str(TAPES_INGEST_YAML).unwrap();
        assert!(document.get("paths").is_some());
    }

    #[test]
    fn every_vendored_operation_is_exposed_or_deliberately_unexposed() {
        // THE coverage gate: a contract bump that adds an operation fails here
        // with the new ids listed, until each is mapped or allow-listed. The
        // mechanism is the shared crate's; the tables it judges are this
        // CLI's, which is the whole reason they did not move with the
        // contract.
        if let Err(report) = coverage::check(EXPOSED_OPERATIONS, UNEXPOSED_OPERATIONS) {
            panic!("{report}");
        }
    }

    #[test]
    fn the_coverage_tables_name_only_operations_the_contract_has() {
        // A stale table entry means the contract dropped or renamed an
        // operation; the mapping must move in the same change.
        let surface = core().unwrap();
        let known: BTreeSet<&str> = surface.operation_ids().collect();
        for (id, _) in EXPOSED_OPERATIONS.iter().chain(UNEXPOSED_OPERATIONS) {
            assert!(
                known.contains(id),
                "{id:?} is in a coverage table but not in the vendored tapes-api contract",
            );
        }
    }

    #[test]
    fn no_operation_is_both_exposed_and_unexposed() {
        let exposed: BTreeSet<&str> = EXPOSED_OPERATIONS.iter().map(|(id, _)| *id).collect();
        for (id, _) in UNEXPOSED_OPERATIONS {
            assert!(!exposed.contains(id), "{id:?} is in both tables");
        }
    }

    #[test]
    fn every_exposed_operation_resolves_to_a_contract_method() {
        let surface = core().unwrap();
        for (id, command) in EXPOSED_OPERATIONS {
            let method = surface.method(id);
            assert!(
                method.is_ok(),
                "{command} maps {id:?}, which did not resolve"
            );
        }
    }

    #[test]
    fn an_unknown_operation_is_an_error_not_a_guessed_route() {
        let err = core().unwrap().method("launchMissiles").unwrap_err();
        assert!(err.to_string().contains("launchMissiles"), "got: {err}");
    }

    #[test]
    fn a_value_is_routed_by_the_contracts_declared_location() {
        let surface = core().unwrap();
        let method = surface.method(ops::GET_SESSION_TRACES).unwrap();
        let call = call_for(
            method,
            vec![("id", "s-1".to_owned()), ("payload", "preview".to_owned())],
        )
        .unwrap();

        assert_eq!(call.method, "GET");
        assert_eq!(call.path, "/v1/sessions/{id}/traces");
        assert_eq!(call.path_params, vec![("id".to_owned(), "s-1".to_owned())]);
        assert_eq!(
            call.query,
            vec![("payload".to_owned(), "preview".to_owned())]
        );
    }

    #[test]
    fn an_undeclared_parameter_is_refused_before_any_request() {
        // Sending it anyway is exactly the drift the vendored contract exists
        // to catch; the server ignoring an unknown query param would hide it.
        let surface = core().unwrap();
        let method = surface.method(ops::GET_SESSION).unwrap();
        let err = call_for(
            method,
            vec![("id", "s-1".to_owned()), ("payolad", "full".to_owned())],
        )
        .unwrap_err();
        assert!(err.to_string().contains("payolad"), "got: {err}");
    }

    #[test]
    fn a_missing_path_parameter_is_refused_because_no_url_could_be_built() {
        let surface = core().unwrap();
        let method = surface.method(ops::GET_SPAN).unwrap();
        let err = call_for(method, vec![("trace_id", "t-1".to_owned())]).unwrap_err();
        assert!(err.to_string().contains("span_id"), "got: {err}");
    }

    #[test]
    fn the_shared_contract_errors_keep_this_clis_wording() {
        // The contract layer moved out; its refusals still have to read as
        // this CLI's errors, because they are what a user sees when a command
        // names something the contract does not have.
        let err: crate::error::Error = core().unwrap().method("launchMissiles").unwrap_err().into();
        assert_eq!(
            err.to_string(),
            "the vendored tapes-api contract has no operation \"launchMissiles\"",
        );
    }
}

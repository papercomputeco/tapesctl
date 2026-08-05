//! The vendored core contract, and the surface reduced from it.
//!
//! # One reducer, two document sources
//!
//! The generated cassette surface already answers "what can this server do?"
//! by reducing an OpenAPI document to callable methods — discovered at runtime,
//! because the cassette set is deployment configuration. The core tapes API is
//! the opposite kind of fact: it is a *published contract*, sealed in the tapes
//! repository (`api/CONTRACT`) and attached to releases, so the right copy to
//! build from is the vendored one in `contracts/tapes-api.yaml`, pinned by
//! fingerprint (see `contracts/PROVENANCE.md`).
//!
//! Both feed [`crate::cassette::spec::reduce_methods`]. What used to be a set
//! of hand-written URL builders in [`crate::api::client`] is now a lookup into
//! this surface: the verb, the path template, and the set of declared
//! parameters all come from the contract bytes, and a request naming a
//! parameter the contract does not declare is refused before it is sent.
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

use std::sync::LazyLock;

use serde_json::Value;
use snafu::OptionExt;

use crate::api::client::Call;
use crate::cassette::spec::{self, Location, Method};
use crate::error::{Result, error};

/// The vendored read-API contract, byte-for-byte what
/// `contracts/tapes-api.yaml` holds.
pub const TAPES_API_YAML: &str = include_str!("../../contracts/tapes-api.yaml");

/// The vendored ingest contract. Nothing at runtime reads it — the capture
/// path keeps its hand-written request construction — but the conformance
/// tests in `tests/ingest_conformance.rs` hold that construction to it.
pub const TAPES_INGEST_YAML: &str = include_str!("../../contracts/tapes-ingest.yaml");

/// Operation ids of the vendored contract, named once so the client methods,
/// the coverage tables, and the tests cannot drift apart on a string.
pub mod ops {
    /// `GET /v1/sessions`
    pub const LIST_SESSIONS: &str = "listSessions";
    /// `GET /v1/sessions/{id}`
    pub const GET_SESSION: &str = "getSession";
    /// `GET /v1/sessions/{id}/traces`
    pub const GET_SESSION_TRACES: &str = "getSessionTraces";
    /// `GET /v1/sessions/{id}/raw_turns`
    pub const LIST_RAW_TURNS: &str = "listRawTurns";
    /// `GET /v1/sessions/{id}/export`
    pub const EXPORT_SESSION: &str = "exportSession";
    /// `GET /v1/traces`
    pub const LIST_TRACES: &str = "listTraces";
    /// `GET /v1/traces/{trace_id}`
    pub const GET_TRACE: &str = "getTrace";
    /// `GET /v1/traces/{trace_id}/spans/{span_id}`
    pub const GET_SPAN: &str = "getSpan";
    /// `GET /v1/search/spans`
    pub const SEARCH_SPANS: &str = "searchSpans";
    /// `POST /v1/admin/seed/demo`
    pub const SEED_DEMO: &str = "seedDemo";
    /// `GET /v1/cassettes`
    pub const LIST_CASSETTES: &str = "listCassettes";
}

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
        "server-side skills store; tapesctl's skill commands author locally (~/.tapes/skills) and do not speak this API",
    ),
    (
        "listSkills",
        "server-side skills store; see listSessionSkills",
    ),
    (
        "createSkill",
        "server-side skills store; see listSessionSkills",
    ),
    (
        "getSkill",
        "server-side skills store; see listSessionSkills",
    ),
    (
        "updateSkill",
        "server-side skills store; see listSessionSkills",
    ),
    (
        "deleteSkill",
        "server-side skills store; see listSessionSkills",
    ),
    (
        "duplicateSkill",
        "server-side skills store; see listSessionSkills",
    ),
    (
        "getSkillMarkdown",
        "server-side skills store; see listSessionSkills",
    ),
    (
        "listSkillVersions",
        "server-side skills store; see listSessionSkills",
    ),
    (
        "publishSkill",
        "server-side skills store; see listSessionSkills",
    ),
    (
        "generateSkill",
        "server-side generation; tapesctl skill generate runs its own extraction against a user-chosen LLM instead",
    ),
];

/// The core read surface, reduced from the vendored contract.
#[derive(Debug)]
pub struct CoreSurface {
    methods: Vec<Method>,
}

impl CoreSurface {
    /// Reduce a contract document from its YAML bytes.
    fn from_yaml(yaml: &str) -> Option<Self> {
        let document: Value = serde_yaml::from_str(yaml).ok()?;
        let methods = spec::reduce_methods(&document);
        if methods.is_empty() {
            // An empty surface means the bytes were YAML but not a contract;
            // treat it exactly like a parse failure rather than serving a
            // client where every operation lookup fails one at a time.
            return None;
        }
        Some(Self { methods })
    }

    /// Look one operation up by the contract's own `operationId`.
    pub fn method(&self, operation_id: &str) -> Result<&Method> {
        self.methods
            .iter()
            .find(|method| method.operation_id.as_deref() == Some(operation_id))
            .context(error::ContractOperationSnafu {
                operation: operation_id,
            })
    }

    /// Every `operationId` in the vendored document, for the coverage gate.
    pub fn operation_ids(&self) -> impl Iterator<Item = &str> {
        self.methods
            .iter()
            .filter_map(|method| method.operation_id.as_deref())
    }
}

/// The surface, reduced once per process. `None` only for a build whose
/// embedded document is corrupt, which the contract tests fail long before.
static CORE: LazyLock<Option<CoreSurface>> =
    LazyLock::new(|| CoreSurface::from_yaml(TAPES_API_YAML));

/// The core surface, or the build-defect error.
pub fn core() -> Result<&'static CoreSurface> {
    CORE.as_ref().context(error::VendoredContractSnafu {
        surface: "tapes-api",
    })
}

/// Build the [`Call`] for one operation from wire-named values.
///
/// This is where "drive through the contract" becomes enforceable: the verb
/// and path template are the document's, every value is routed by the
/// document's declared location for that name, a name the document does not
/// declare is refused, and a path placeholder left without a value is refused
/// (no URL could be built from it). Values are given under their wire names —
/// the same names the deleted hand-written builders used — so the call sites
/// read as the requests they make.
pub fn call_for<'m>(method: &'m Method, values: Vec<(&str, String)>) -> Result<Call<'m>> {
    let operation = || {
        method
            .operation_id
            .clone()
            .unwrap_or_else(|| method.name.clone())
    };

    let mut call = Call {
        method: &method.http_method,
        path: &method.path,
        ..Default::default()
    };

    for (wire, value) in values {
        let declared = method
            .params
            .iter()
            .find(|param| param.wire == wire)
            .with_context(|| error::ContractParameterSnafu {
                operation: operation(),
                parameter: wire,
            })?;
        let pair = (declared.wire.clone(), value);
        match declared.location {
            Location::Path => call.path_params.push(pair),
            Location::Query => call.query.push(pair),
            Location::Header => call.headers.push(pair),
        }
    }

    // A path placeholder without a value cannot produce a callable URL; the
    // substitution would leave a literal `{id}` segment addressing nothing.
    for param in method.path_params() {
        if !call.path_params.iter().any(|(name, _)| *name == param.wire) {
            return error::ContractPathParameterSnafu {
                operation: operation(),
                parameter: param.wire.clone(),
            }
            .fail();
        }
    }

    Ok(call)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn the_vendored_contract_parses_and_reduces() {
        // The one place a corrupt vendored document is allowed to fail loudly.
        let surface = core().expect("contracts/tapes-api.yaml must parse");
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
        // with the new ids listed, until each is mapped or allow-listed.
        let exposed: BTreeSet<&str> = EXPOSED_OPERATIONS.iter().map(|(id, _)| *id).collect();
        let unexposed: BTreeSet<&str> = UNEXPOSED_OPERATIONS.iter().map(|(id, _)| *id).collect();

        let unmapped: Vec<&str> = core()
            .unwrap()
            .operation_ids()
            .filter(|id| !exposed.contains(id) && !unexposed.contains(id))
            .collect();

        assert!(
            unmapped.is_empty(),
            "operations in contracts/tapes-api.yaml that tapesctl neither exposes nor \
             allow-lists: {unmapped:?} — add each to EXPOSED_OPERATIONS (and wire it up) \
             or to UNEXPOSED_OPERATIONS with the reason it stays unexposed",
        );
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
                "{id:?} is in a coverage table but not in contracts/tapes-api.yaml",
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
}

//! The `/v1/cassettes` discovery document.
//!
//! Only the fields this client acts on are modelled. The rest of the document —
//! `tables`, `depends`, `config` and the other manifest projections — is an
//! operator's view of what a cassette *is*, and deployment/configuration is
//! deliberately not part of the generated command surface.
//!
//! Note which digest is which: `manifest_digest` covers the cassette's manifest,
//! while the `ETag` on the OpenAPI route covers the republished document. They
//! are two digests over two different byte streams, so the cache revalidates
//! with the ETag and keeps `manifest_digest` only for reporting.

use serde::{Deserialize, Serialize};

/// The document served at `GET /v1/cassettes`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Discovery {
    /// The newest cassette contract this core serves. May legitimately be empty
    /// when the configured set is malformed.
    #[serde(default)]
    pub contract_version: String,

    /// What is installed here. Never null, and ordered by name.
    #[serde(default)]
    pub cassettes: Vec<DiscoveryEntry>,

    /// Configured cassette sources that could not be loaded. Never null.
    ///
    /// Carried so `--help` can say *why* an expected cassette is missing rather
    /// than leaving the user to guess; an operator's typo in a cassette URL is
    /// otherwise indistinguishable from the cassette not existing.
    #[serde(default)]
    pub problems: Vec<Problem>,
}

/// One served cassette.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DiscoveryEntry {
    /// The cassette's name, which is also its noun on the command line.
    pub name: String,

    /// The cassette's own version, when core could project its manifest.
    #[serde(default)]
    pub version: Option<String>,

    /// A human-facing name, when the manifest carries one.
    #[serde(default)]
    pub display_name: Option<String>,

    /// One line of prose, used as the subcommand's `about`.
    #[serde(default)]
    pub description: Option<String>,

    /// Where the cassette is mounted, `/v1/cassettes/<name>`.
    #[serde(default)]
    pub route_prefix: String,

    /// Where this cassette's OpenAPI document is served.
    #[serde(default)]
    pub openapi_path: String,

    /// How current core's cached copy of that document is: `fresh`, `stale`, or
    /// `missing`. `missing` is normal at boot, before anything is fetched.
    #[serde(default)]
    pub openapi_status: String,

    /// Digest of the cassette's manifest. Reported, not used as a cache key —
    /// see the module docs.
    #[serde(default)]
    pub manifest_digest: String,
}

impl DiscoveryEntry {
    /// Whether core has a document to serve for this cassette.
    ///
    /// A `missing` spec is not an error: core publishes the cassette as soon as
    /// it is admitted and fetches the document on its own schedule. There is
    /// simply nothing to generate commands from yet.
    #[must_use]
    pub fn has_spec(&self) -> bool {
        !self.openapi_path.is_empty() && self.openapi_status != "missing"
    }
}

/// A configured cassette source core refused, and why.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Problem {
    /// The configured OpenAPI URL, with any credential already redacted by the
    /// server.
    #[serde(default)]
    pub subject: String,

    /// Prose. Never parsed.
    #[serde(default)]
    pub reason: String,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn a_document_decodes_from_the_servers_own_field_names() {
        let document: Discovery = serde_json::from_str(
            r#"{
                "contract_version": "v1",
                "cassettes": [{
                    "name": "summary",
                    "version": "0.3.1",
                    "display_name": "Summaries",
                    "description": "Rolling summaries",
                    "route_prefix": "/v1/cassettes/summary",
                    "tables": ["summary.summary"],
                    "config": [{"key": "llm.model", "type": "string", "required": false, "secret": false}],
                    "openapi_path": "/v1/cassettes/summary/openapi.json",
                    "openapi_status": "fresh",
                    "manifest_digest": "sha256:abc"
                }],
                "problems": [{"subject": "http://sidecar.invalid/openapi", "reason": "kind is required"}]
            }"#,
        )
        .unwrap();

        assert_eq!(document.contract_version, "v1");
        assert_eq!(document.cassettes[0].name, "summary");
        assert_eq!(
            document.cassettes[0].openapi_path,
            "/v1/cassettes/summary/openapi.json",
        );
        assert!(document.cassettes[0].has_spec());
        assert_eq!(document.problems[0].reason, "kind is required");
    }

    #[test]
    fn fields_this_client_does_not_model_do_not_break_the_decode() {
        // `tables` and `config` are an operator's view and are skipped on
        // purpose; a stricter decode would turn a server that grows a field into
        // a client that cannot read discovery at all.
        let document: Discovery = serde_json::from_str(
            r#"{"contract_version":"v1","cassettes":[{"name":"x","route_prefix":"/v1/cassettes/x",
                "openapi_path":"/v1/cassettes/x/openapi.json","openapi_status":"fresh",
                "manifest_digest":"","a_field_from_the_future":7}],"problems":[]}"#,
        )
        .unwrap();

        assert_eq!(document.cassettes[0].name, "x");
    }

    #[test]
    fn an_empty_install_is_a_document_not_an_error() {
        let document: Discovery =
            serde_json::from_str(r#"{"contract_version":"v1","cassettes":[],"problems":[]}"#)
                .unwrap();
        assert!(document.cassettes.is_empty());
    }

    #[test]
    fn a_cassette_whose_spec_core_has_not_fetched_yet_generates_nothing() {
        // `missing` is the honest answer at boot, not a failure.
        let entry = DiscoveryEntry {
            name: "summary".to_owned(),
            openapi_path: "/v1/cassettes/summary/openapi.json".to_owned(),
            openapi_status: "missing".to_owned(),
            ..Default::default()
        };
        assert!(!entry.has_spec());

        // `stale` still has a document, and a stale surface beats none: core
        // keeps serving it precisely so a client can read a cassette that is
        // currently down.
        let stale = DiscoveryEntry {
            openapi_status: "stale".to_owned(),
            ..entry
        };
        assert!(stale.has_spec());
    }
}

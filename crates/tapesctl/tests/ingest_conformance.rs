//! Conformance of tapesctl's ingest requests to the vendored ingest contract.
//!
//! The capture path is deliberately not generated from
//! `contracts/tapes-ingest.yaml` — its payload types carry invariants (raw-only
//! capture, verbatim `RawValue` embedding, omission semantics) that a schema
//! cannot express. What the vendored contract *can* do is hold the hand-written
//! construction to account: every test here reads the contract document and
//! asserts the request tapesctl actually builds — route, verb, content type,
//! envelope field names — against it, rather than against a copied string.
//!
//! Several of these facts used to live only as prose ("wire-shape gotchas" in
//! `start::ingest`): the parent id that must be omitted rather than sent empty,
//! the identity fields that must be sent even when empty, the base64 alphabet
//! `raw_response` must use. Each is machine-checked against the contract here.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;

use serde_json::Value;
use serde_json::value::RawValue;
use tapes_capture::envelope::TapesAttribution;
use tapes_harnesses::transcript::{
    SubagentMeta, TranscriptFile, TranscriptSession, build_payload, jsonl_to_records,
};
use tapesctl::api::contract::TAPES_INGEST_YAML;
use tapesctl::start::ingest::{
    IngestClient, SessionEnvelope, TurnMeta, TurnPayload, encode_raw_response, status_class,
};
use tapesctl::transcript::client::TranscriptClient;
use url::Url;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The vendored ingest contract, parsed.
fn contract() -> Value {
    serde_yaml::from_str(TAPES_INGEST_YAML).expect("contracts/tapes-ingest.yaml must parse")
}

/// The `(path, verb, operation)` of one operation, found by its id — so the
/// tests read routes out of the document instead of repeating them.
fn operation(document: &Value, operation_id: &str) -> (String, String, Value) {
    let paths = document["paths"].as_object().unwrap();
    for (route, item) in paths {
        let Some(item) = item.as_object() else {
            continue;
        };
        for (verb, op) in item {
            if op.get("operationId").and_then(Value::as_str) == Some(operation_id) {
                return (route.clone(), verb.clone(), op.clone());
            }
        }
    }
    panic!("no operation {operation_id:?} in the vendored ingest contract");
}

/// The property names a component schema declares.
fn schema_properties(document: &Value, component: &str) -> BTreeSet<String> {
    document["components"]["schemas"][component]["properties"]
        .as_object()
        .unwrap_or_else(|| panic!("schema {component} has no properties"))
        .keys()
        .cloned()
        .collect()
}

/// The property names a component schema requires. Ingest schemas carry no
/// `required` list today, and that absence is itself load-bearing — it is what
/// makes omission a contract-legal spelling of "absent".
fn schema_required(document: &Value, component: &str) -> BTreeSet<String> {
    document["components"]["schemas"][component]["required"]
        .as_array()
        .map(|names| {
            names
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// The keys of a serialized JSON object.
fn keys_of(value: &Value) -> BTreeSet<String> {
    value.as_object().unwrap().keys().cloned().collect()
}

/// A fully-attributed session, exercising every envelope field tapesctl sends.
fn attribution() -> TapesAttribution {
    let mut attribution = TapesAttribution::unknown();
    attribution.harness_id = "claude".to_owned();
    attribution.session_id = Some("sid-1".to_owned());
    attribution.version = Some("2.0.0".to_owned());
    attribution.cwd = Some("/tmp/project".to_owned());
    attribution.name = Some("named-session".to_owned());
    attribution.parent_sid = Some("sid-0".to_owned());
    attribution
        .metadata
        .insert("source".to_owned(), Value::from("test"));
    attribution
}

/// A meta block with every field tapesctl's proxy populates.
fn full_meta() -> TurnMeta {
    TurnMeta {
        request_id: "req-1".to_owned(),
        thread_id: Some("thread-9".to_owned()),
        method: "POST".to_owned(),
        path: "/v1/messages".to_owned(),
        content_type: Some("text/event-stream".to_owned()),
        content_encoding: Some("gzip".to_owned()),
        stream: Some("true".to_owned()),
        upstream_status: 200,
        upstream_status_class: status_class(200),
        request_bytes: 128,
        response_bytes: 4096,
        elapsed_seconds: 1.25,
    }
}

/// A fully-populated turn, serialized — the widest request tapesctl can make.
fn full_turn_json(request: &RawValue) -> Value {
    let payload = TurnPayload {
        provider: "anthropic",
        request,
        response: (),
        raw_response: Some(encode_raw_response(b"event: ping\n\n")),
        raw_response_encoding: Some("gzip".to_owned()),
        meta: full_meta(),
        session: Some(SessionEnvelope::from_attribution(
            &attribution(),
            "0ea3c2cc-fe9d-41ff-aab1-4134ad00c350",
            "local:test",
        )),
    };
    serde_json::to_value(&payload).unwrap()
}

#[test]
fn the_turn_post_targets_the_contracts_route() {
    // The path tapesctl joins onto the ingest base is the contract's own
    // `ingestTurn` route, and the verb and body rules match it: POST, with a
    // required application/json body.
    let document = contract();
    let (route, verb, op) = operation(&document, "ingestTurn");

    let client = IngestClient::new(&Url::parse("http://127.0.0.1:8090").unwrap()).unwrap();
    assert_eq!(client.endpoint().path(), route);
    assert_eq!(verb, "post");
    assert_eq!(op["requestBody"]["required"], Value::from(true));
    assert!(
        op["requestBody"]["content"]
            .get("application/json")
            .is_some(),
        "the contract takes JSON, which is what post_turn sends",
    );
}

#[tokio::test]
async fn the_wire_request_matches_the_contract_end_to_end() {
    // The strongest form of the claim: mount a server whose matchers are built
    // FROM the contract document — route, verb, content type — and let the
    // real client construct the request. The 202 body is the contract's ack
    // shape, and the client must read it as success.
    let document = contract();
    let (route, verb, op) = operation(&document, "ingestTurn");
    assert!(
        op["responses"].get("202").is_some(),
        "the accepted status is part of the contract",
    );

    let server = MockServer::start().await;
    Mock::given(method(verb.to_uppercase().as_str()))
        .and(path(route.as_str()))
        .and(header("content-type", "application/json"))
        .respond_with(ResponseTemplate::new(202).set_body_string(r#"{"status":"accepted"}"#))
        .expect(1)
        .mount(&server)
        .await;

    let request = RawValue::from_string(r#"{"model":"claude"}"#.to_owned()).unwrap();
    let payload = TurnPayload {
        provider: "anthropic",
        request: &request,
        response: (),
        raw_response: Some(encode_raw_response(b"event: ping\n\n")),
        raw_response_encoding: None,
        meta: full_meta(),
        session: Some(SessionEnvelope::from_attribution(
            &attribution(),
            "",
            "local:test",
        )),
    };

    let client = IngestClient::new(&Url::parse(&server.uri()).unwrap()).unwrap();
    client.post_turn(&payload).await.unwrap();
}

#[test]
fn every_turn_field_sent_is_declared_by_the_contract() {
    // An undeclared field would ride to the server and be silently dropped —
    // or worse, silently read — without either side noticing the drift. Every
    // key tapesctl emits, at every level of the envelope, must be a property
    // the contract declares. (The reverse is not required: the contract keeps
    // fields tapesctl deliberately does not send, like `agent_name` and
    // `captured_at`.)
    let document = contract();
    let request = RawValue::from_string(r#"{"model":"claude"}"#.to_owned()).unwrap();
    let turn = full_turn_json(&request);

    let undeclared: Vec<String> = keys_of(&turn)
        .difference(&schema_properties(&document, "TurnPayload"))
        .cloned()
        .collect();
    assert!(undeclared.is_empty(), "TurnPayload sends {undeclared:?}");

    let undeclared: Vec<String> = keys_of(&turn["meta"])
        .difference(&schema_properties(&document, "TurnMeta"))
        .cloned()
        .collect();
    assert!(undeclared.is_empty(), "TurnMeta sends {undeclared:?}");

    let undeclared: Vec<String> = keys_of(&turn["session"])
        .difference(&schema_properties(&document, "IngestEnvelope"))
        .cloned()
        .collect();
    assert!(
        undeclared.is_empty(),
        "SessionEnvelope sends {undeclared:?}"
    );
}

#[test]
fn raw_response_uses_the_contracts_byte_format() {
    // The contract says `format: byte` — OpenAPI's name for standard padded
    // base64, which is how Go's encoding/json renders []byte. base64url or
    // unpadded output would decode to different bytes server-side; this pins
    // the alphabet ('+' and '/', not '-' and '_') and the padding to the
    // contract rather than to a comment.
    let document = contract();
    assert_eq!(
        document["components"]["schemas"]["TurnPayload"]["properties"]["raw_response"]["format"],
        Value::from("byte"),
    );
    assert_eq!(encode_raw_response(b"ab"), "YWI=");
    assert_eq!(encode_raw_response(&[0xfb, 0xff]), "+/8=");
}

#[test]
fn an_absent_parent_is_omitted_because_omission_is_the_contracts_absent() {
    // The prose gotcha, machine-checked: `parent_harness_session_id` is a
    // declared plain-string property that no `required` list names — so the
    // contract's only spellings are a real value or omission, and tapesctl
    // must never invent a third (`""`), which the server rejects.
    let document = contract();
    let properties = schema_properties(&document, "IngestEnvelope");
    assert!(properties.contains("parent_harness_session_id"));
    assert!(!schema_required(&document, "IngestEnvelope").contains("parent_harness_session_id"));

    let mut orphan = attribution();
    orphan.parent_sid = None;
    let json =
        serde_json::to_value(SessionEnvelope::from_attribution(&orphan, "", "local:test")).unwrap();
    assert!(
        json.get("parent_harness_session_id").is_none(),
        "got: {json}"
    );

    let json = serde_json::to_value(SessionEnvelope::from_attribution(
        &attribution(),
        "",
        "local:test",
    ))
    .unwrap();
    assert_eq!(json["parent_harness_session_id"], "sid-0");
}

#[test]
fn identity_fields_are_sent_even_when_empty() {
    // The contract prose says org_id and auth_subject "MUST be set on every
    // non-nil envelope" and are persisted verbatim; the Go fields carry no
    // omitempty. The machine-checkable half: both are declared properties, and
    // tapesctl emits both keys even for the empty local sentinel.
    let document = contract();
    let properties = schema_properties(&document, "IngestEnvelope");
    assert!(properties.contains("org_id"));
    assert!(properties.contains("auth_subject"));

    let json =
        serde_json::to_value(SessionEnvelope::from_attribution(&attribution(), "", "")).unwrap();
    assert_eq!(json["org_id"], "");
    assert_eq!(json["auth_subject"], "");
}

#[test]
fn a_raw_only_turn_sends_a_null_reduction_which_the_contract_permits() {
    // Raw-only capture is tapesctl's whole ingest posture: `response` is
    // always null and the verbatim bytes ride in raw_response. The contract
    // declares `response` and requires nothing — so "no reduction supplied" is
    // legal, and the reduction stays the server's job.
    let document = contract();
    assert!(schema_properties(&document, "TurnPayload").contains("response"));
    assert!(!schema_required(&document, "TurnPayload").contains("response"));

    let request = RawValue::from_string(r#"{"model":"claude"}"#.to_owned()).unwrap();
    let turn = full_turn_json(&request);
    assert!(turn["response"].is_null());
    assert!(keys_of(&turn).contains("response"));
}

#[test]
fn the_transcript_post_targets_the_contracts_route() {
    // The transcript lane's endpoint constant lives in tapes-harnesses;
    // tapesctl joins it onto the base. What must hold HERE is that the URL the
    // TranscriptClient actually builds is the contract's `ingestTranscript`
    // route, with the same POST-a-required-JSON-body rules as the turn lane.
    let document = contract();
    let (route, verb, op) = operation(&document, "ingestTranscript");

    let client = TranscriptClient::new(&Url::parse("http://127.0.0.1:8090").unwrap()).unwrap();
    assert_eq!(client.endpoint().path(), route);
    assert_eq!(verb, "post");
    assert_eq!(op["requestBody"]["required"], Value::from(true));
    assert!(
        op["requestBody"]["content"]
            .get("application/json")
            .is_some()
    );
}

#[tokio::test]
async fn the_transcript_wire_request_matches_the_contract_end_to_end() {
    let document = contract();
    let (route, verb, op) = operation(&document, "ingestTranscript");
    assert!(
        op["responses"].get("202").is_some(),
        "the accepted status is part of the contract",
    );

    let server = MockServer::start().await;
    Mock::given(method(verb.to_uppercase().as_str()))
        .and(path(route.as_str()))
        .and(header("content-type", "application/json"))
        .respond_with(
            ResponseTemplate::new(202)
                .set_body_string(r#"{"status":"accepted","deduped":true,"records":2}"#),
        )
        .expect(1)
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("main.jsonl");
    std::fs::write(
        &file_path,
        "{\"type\":\"user\"}\n{\"type\":\"assistant\"}\n",
    )
    .unwrap();
    let file = TranscriptFile {
        path: file_path,
        agent_id: None,
        meta: SubagentMeta::default(),
    };
    let session = TranscriptSession::new("claude", "sid-1").with_auth_subject("local:test");

    let client = TranscriptClient::new(&Url::parse(&server.uri()).unwrap()).unwrap();
    let outcome = client.upload_file(&session, &file).await.unwrap();
    assert!(outcome.deduped, "the contract's ack fields must be read");
    assert_eq!(outcome.records, 2);
}

#[test]
fn every_transcript_field_sent_is_declared_by_the_contract() {
    // The widest transcript payload tapesctl can build: a subagent file whose
    // meta carries every field. Each emitted key must be declared, and
    // `records` must serialize as the JSON array the contract describes —
    // which is what makes the server's content-hash dedup byte-stable.
    let document = contract();

    let session = TranscriptSession::new("claude", "sid-1")
        .with_harness_version(Some("2.0.0".to_owned()))
        .with_cwd(Some("/tmp/project".to_owned()))
        .with_auth_subject("local:test");
    let file = TranscriptFile {
        path: "/tmp/unused.jsonl".into(),
        agent_id: Some("a1".to_owned()),
        meta: SubagentMeta {
            tool_use_id: "toolu_1".to_owned(),
            agent_type: "general-purpose".to_owned(),
            description: "delegated work".to_owned(),
        },
    };
    let records = RawValue::from_string(jsonl_to_records(b"{\"type\":\"user\"}\n")).unwrap();
    let json = serde_json::to_value(build_payload(&session, &file, &records)).unwrap();

    let undeclared: Vec<String> = keys_of(&json)
        .difference(&schema_properties(&document, "TranscriptPayload"))
        .cloned()
        .collect();
    assert!(
        undeclared.is_empty(),
        "TranscriptPayload sends {undeclared:?}"
    );

    let undeclared: Vec<String> = keys_of(&json["session"])
        .difference(&schema_properties(&document, "IngestEnvelope"))
        .cloned()
        .collect();
    assert!(
        undeclared.is_empty(),
        "the session block sends {undeclared:?}"
    );

    assert!(
        json["records"].is_array(),
        "records must reach the server as a JSON array: {}",
        json["records"],
    );
}

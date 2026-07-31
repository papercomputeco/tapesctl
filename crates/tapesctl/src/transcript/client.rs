//! Transcript delivery: one transcript file in, one
//! `POST /v1/ingest/transcript` out.
//!
//! This is the half `tapes-harnesses` deliberately does not own. The crate
//! discovers the upload set, converts JSONL to records, and decides *when* to
//! push; the HTTP call, the credential, and the response parsing differ per
//! client, so they live here — the same split [`super::super::start::ingest`]
//! makes for the wire lane.
//!
//! # Why a blind re-push is safe
//!
//! The server keys a transcript row on
//! `transcript:<harness_session_id>:<agent|main>:<sha256(records)[..8]>`, so
//! identical content answers `deduped: true` and a grown transcript appends a
//! new version. That is what lets [`super::sync`] re-offer everything it finds
//! without tracking what a previous process already sent, and what lets the
//! tailer err toward pushing again. A dedup is a **success**, not a no-op to
//! retry.
//!
//! The corollary is that `records` must reach the server byte-for-byte: the
//! hash is taken over the bytes as received, so re-serializing them — which
//! would reorder map keys — registers identical content as a new version.
//! [`tapes_harnesses::transcript::jsonl_to_records`] preserves each line
//! verbatim and the payload embeds it as a [`RawValue`] to keep it that way.
//!
//! # No auth header
//!
//! paperd rides its own `X-Paper-Auth` channel so the Paper cloud edge admits
//! the request. That is explicitly not part of the tapes contract: a standalone
//! client posting to its own ingest server sends `Content-Type` and nothing
//! else, exactly like the Go reference client.

use serde::Deserialize;
use serde_json::value::RawValue;
use snafu::ResultExt;
use tapes_harnesses::transcript::{
    INGEST_PATH, TranscriptFile, TranscriptPayload, TranscriptSession, build_payload,
    jsonl_to_records,
};
use url::Url;

use crate::error::{Error, Result, error};

/// How much of an error response body is kept. A rejection names the offending
/// field in its first line; retaining megabytes of a proxy's HTML error page
/// would only bloat the log.
const ERROR_BODY_CAP: usize = 4096;

/// What the server said about one uploaded transcript file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UploadOutcome {
    /// This exact content version was already stored. The normal answer for an
    /// unchanged transcript, and a success.
    pub deduped: bool,
    /// How many records the server counted in the uploaded file.
    pub records: usize,
}

/// The server's 202 acknowledgement. Both fields default so a server that grows
/// the response — or trims it — does not turn a successful upload into an error.
#[derive(Debug, Default, Deserialize)]
struct TranscriptAck {
    #[serde(default)]
    deduped: bool,
    #[serde(default)]
    records: usize,
}

/// A client for one tapes ingest server's transcript lane.
#[derive(Debug, Clone)]
pub struct TranscriptClient {
    http: reqwest::Client,
    endpoint: Url,
}

impl TranscriptClient {
    /// Build a client posting to `base` + [`INGEST_PATH`].
    pub fn new(base: &Url) -> Result<Self> {
        let endpoint = base.join(INGEST_PATH).context(error::TranscriptUrlSnafu)?;
        Ok(Self {
            http: reqwest::Client::new(),
            endpoint,
        })
    }

    /// The resolved transcript endpoint, for logging.
    #[must_use]
    pub fn endpoint(&self) -> &Url {
        &self.endpoint
    }

    /// Post one already-assembled payload.
    pub async fn post_transcript(&self, payload: &TranscriptPayload<'_>) -> Result<UploadOutcome> {
        let response = self
            .http
            .post(self.endpoint.clone())
            .json(payload)
            .send()
            .await
            .context(error::TranscriptSendSnafu)?;

        let status = response.status();
        if !status.is_success() {
            let mut body = response.text().await.unwrap_or_default();
            body.truncate(ERROR_BODY_CAP);
            return Err(Error::TranscriptRejected {
                status: status.as_u16(),
                body,
            });
        }

        // A body that does not parse is not a failed upload: the server already
        // answered 2xx, and the ack is only advisory detail for the log.
        let ack: TranscriptAck = response.json().await.unwrap_or_default();
        Ok(UploadOutcome {
            deduped: ack.deduped,
            records: ack.records,
        })
    }

    /// Read one transcript file off disk and push it.
    ///
    /// Reading here rather than in the caller is what keeps the fingerprint
    /// honest: the tailer fingerprints *before* calling this, so a transcript
    /// that grows mid-upload stays dirty and is pushed again next tick instead
    /// of being recorded as fully sent.
    pub async fn upload_file(
        &self,
        session: &TranscriptSession,
        file: &TranscriptFile,
    ) -> Result<UploadOutcome> {
        let raw = std::fs::read(&file.path).context(error::TranscriptReadSnafu {
            path: file.path.clone(),
        })?;
        let records =
            RawValue::from_string(jsonl_to_records(&raw)).context(error::TranscriptRecordsSnafu)?;
        let payload = build_payload(session, file, &records);
        self.post_transcript(&payload).await
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use tapes_harnesses::transcript::SubagentMeta;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    fn session() -> TranscriptSession {
        TranscriptSession::new("claude", "sid-1")
            .with_cwd(Some("/tmp/project".to_owned()))
            .with_auth_subject("local:test")
    }

    fn write_jsonl(dir: &std::path::Path, name: &str, body: &str) -> TranscriptFile {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        TranscriptFile {
            path,
            agent_id: None,
            meta: SubagentMeta::default(),
        }
    }

    async fn ingest_server(template: ResponseTemplate) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/ingest/transcript"))
            .respond_with(template)
            .mount(&server)
            .await;
        server
    }

    #[test]
    fn the_transcript_path_is_joined_onto_the_base_url() {
        let client = TranscriptClient::new(&Url::parse("http://127.0.0.1:8090").unwrap()).unwrap();
        assert_eq!(
            client.endpoint().as_str(),
            "http://127.0.0.1:8090/v1/ingest/transcript",
        );
    }

    #[test]
    fn a_base_url_with_a_trailing_path_still_resolves_to_the_transcript_route() {
        let client =
            TranscriptClient::new(&Url::parse("http://127.0.0.1:8090/base/").unwrap()).unwrap();
        assert_eq!(
            client.endpoint().as_str(),
            "http://127.0.0.1:8090/v1/ingest/transcript",
        );
    }

    #[tokio::test]
    async fn a_dedup_is_reported_as_success_not_as_an_error() {
        // Re-pushing an unchanged transcript is the normal steady state; if it
        // surfaced as an error the tailer would back off against its own
        // correct behaviour.
        let server = ingest_server(
            ResponseTemplate::new(202)
                .set_body_string(r#"{"status":"accepted","deduped":true,"records":3}"#),
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        let file = write_jsonl(dir.path(), "sid-1.jsonl", "{\"a\":1}\n");

        let client = TranscriptClient::new(&Url::parse(&server.uri()).unwrap()).unwrap();
        let outcome = client.upload_file(&session(), &file).await.unwrap();

        assert!(outcome.deduped);
        assert_eq!(outcome.records, 3);
    }

    #[tokio::test]
    async fn the_records_array_reaches_the_server_with_key_order_intact() {
        // The dedup key is a hash of these exact bytes, so a re-serialization
        // that reorders keys would register identical content as a new version
        // on every single push.
        let server = ingest_server(
            ResponseTemplate::new(202).set_body_string(r#"{"status":"accepted","records":2}"#),
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        let file = write_jsonl(
            dir.path(),
            "sid-1.jsonl",
            "{\"zeta\":1,\"alpha\":2}\n{\"x\":\"y\"}\n",
        );

        let client = TranscriptClient::new(&Url::parse(&server.uri()).unwrap()).unwrap();
        client.upload_file(&session(), &file).await.unwrap();

        let requests = server.received_requests().await.unwrap();
        let body = String::from_utf8(requests[0].body.clone()).unwrap();
        assert!(
            body.contains(r#""records":[{"zeta":1,"alpha":2},{"x":"y"}]"#),
            "got: {body}",
        );
    }

    #[tokio::test]
    async fn no_auth_header_is_sent() {
        // X-Paper-Auth is paperd's channel to the Paper edge, not part of the
        // tapes contract; a standalone client sends Content-Type and nothing
        // else.
        let server = ingest_server(ResponseTemplate::new(202).set_body_string("{}")).await;
        let dir = tempfile::tempdir().unwrap();
        let file = write_jsonl(dir.path(), "sid-1.jsonl", "{\"a\":1}\n");

        let client = TranscriptClient::new(&Url::parse(&server.uri()).unwrap()).unwrap();
        client.upload_file(&session(), &file).await.unwrap();

        let requests: Vec<Request> = server.received_requests().await.unwrap();
        assert!(requests[0].headers.get("x-paper-auth").is_none());
        assert!(requests[0].headers.get("authorization").is_none());
    }

    #[tokio::test]
    async fn a_rejection_carries_the_servers_own_explanation() {
        // A 400 names the offending envelope field; the bare status does not.
        let server = ingest_server(ResponseTemplate::new(400).set_body_string(
            r#"{"error":"transcript ingest requires session.harness_session_id"}"#,
        ))
        .await;
        let dir = tempfile::tempdir().unwrap();
        let file = write_jsonl(dir.path(), "sid-1.jsonl", "{\"a\":1}\n");

        let client = TranscriptClient::new(&Url::parse(&server.uri()).unwrap()).unwrap();
        let err = client.upload_file(&session(), &file).await.unwrap_err();

        assert!(
            format!("{err}").contains("harness_session_id"),
            "got: {err}",
        );
    }

    #[tokio::test]
    async fn a_success_with_an_unparseable_body_is_still_a_success() {
        // The ack is advisory. The server already said 2xx, so refusing the
        // upload over its body would re-push content that is already stored.
        let server = ingest_server(ResponseTemplate::new(202).set_body_string("not json")).await;
        let dir = tempfile::tempdir().unwrap();
        let file = write_jsonl(dir.path(), "sid-1.jsonl", "{\"a\":1}\n");

        let client = TranscriptClient::new(&Url::parse(&server.uri()).unwrap()).unwrap();
        let outcome = client.upload_file(&session(), &file).await.unwrap();

        assert!(!outcome.deduped);
    }

    #[tokio::test]
    async fn a_missing_transcript_file_is_an_error_rather_than_an_empty_push() {
        // Pushing "[]" for a file that vanished would store an empty version of
        // a transcript that still exists elsewhere.
        let server = ingest_server(ResponseTemplate::new(202).set_body_string("{}")).await;
        let dir = tempfile::tempdir().unwrap();
        let file = TranscriptFile {
            path: dir.path().join("gone.jsonl"),
            agent_id: None,
            meta: SubagentMeta::default(),
        };

        let client = TranscriptClient::new(&Url::parse(&server.uri()).unwrap()).unwrap();
        assert!(client.upload_file(&session(), &file).await.is_err());
    }
}

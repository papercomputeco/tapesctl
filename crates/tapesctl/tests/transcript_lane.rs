//! End-to-end tests for the transcript lane's *lifecycle*.
//!
//! The unit tests in `transcript::tailer` drive `tick` directly, which proves
//! the decision logic. These drive the real spawned task through its real
//! shutdown, which is what proves the thing the lane actually promises: that a
//! session's fork skeleton reaches ingest before the process goes away.
//!
//! That distinction has teeth. Changing the shutdown from "fire the trigger and
//! await the handle" to "abort the task" leaves every unit test green and
//! silently drops the exit push — the single most important upload the lane
//! makes, because it is the one carrying the completed subagent transcripts.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use tapes_harnesses::attribution::ClaudeSessionFile;
use tapes_harnesses::attribution::claude::fork_parent::encode_cwd;
use tapesctl::transcript::client::TranscriptClient;
use tapesctl::transcript::tailer::{self, SessionTracker, TailerConfig};
use url::Url;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CWD: &str = "/tmp/tapesctl-transcript-lane";
const SID: &str = "0ea3c2cc-fe9d-41ff-aab1-4134ad00c350";

fn session_file(pid: i64, sid: &str, cwd: &str) -> ClaudeSessionFile {
    ClaudeSessionFile {
        pid,
        session_id: sid.to_owned(),
        cwd: Some(cwd.to_owned()),
        version: Some("2.1.161".to_owned()),
        peer_protocol: None,
        kind: None,
        entrypoint: None,
        name: None,
        status: None,
        proc_start: None,
        started_at: None,
        updated_at: None,
        extra: serde_json::Map::new(),
    }
}

/// Lay out a harness transcript tree: the main transcript plus a `subagents/`
/// directory with one forked agent and its fork metadata.
fn transcript_tree(root: &Path, subagents: &[&str]) -> PathBuf {
    let projects_dir = root.join(encode_cwd(CWD));
    std::fs::create_dir_all(&projects_dir).unwrap();
    std::fs::write(
        projects_dir.join(format!("{SID}.jsonl")),
        format!("{{\"cwd\":\"{CWD}\",\"version\":\"2.1.161\"}}\n{{\"type\":\"user\"}}\n"),
    )
    .unwrap();

    if !subagents.is_empty() {
        let sub_dir = projects_dir.join(SID).join("subagents");
        std::fs::create_dir_all(&sub_dir).unwrap();
        for agent in subagents {
            std::fs::write(
                sub_dir.join(format!("agent-{agent}.jsonl")),
                "{\"type\":\"assistant\"}\n",
            )
            .unwrap();
            std::fs::write(
                sub_dir.join(format!("agent-{agent}.meta.json")),
                format!(
                    r#"{{"toolUseId":"toolu_{agent}","agentType":"explorer","description":"look at {agent}"}}"#
                ),
            )
            .unwrap();
        }
    }
    root.to_path_buf()
}

async fn ingest_server() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/ingest/transcript"))
        .respond_with(
            ResponseTemplate::new(202)
                .set_body_string(r#"{"status":"accepted","deduped":false,"records":2}"#),
        )
        .mount(&server)
        .await;
    server
}

/// Every transcript body the server received.
async fn bodies(server: &MockServer) -> Vec<serde_json::Value> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|request| serde_json::from_slice(&request.body).unwrap())
        .collect()
}

#[tokio::test]
async fn the_shutdown_push_delivers_the_whole_fork_skeleton() {
    // The acceptance criterion, end to end: a session with subagents must put
    // the main transcript *and* every subagent transcript on the wire, each
    // carrying the Task tool_use that forked it — because that edge is what the
    // deriver turns into the nested rows the console renders.
    let server = ingest_server().await;
    let tree = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let root = transcript_tree(tree.path(), &["a1", "a2"]);

    let tracker = SessionTracker::new();
    tracker.observe(&session_file(4242, SID, CWD));

    let client = TranscriptClient::new(&Url::parse(&server.uri()).unwrap()).unwrap();
    let config = TailerConfig::new(root, sessions.path().to_path_buf(), "local:test".to_owned());
    let (shutdown, handle) = tailer::spawn(client, tracker, config);

    // The harness exiting is what fires this in `tapesctl start`.
    shutdown.send(()).unwrap();
    handle.await.unwrap();

    let bodies = bodies(&server).await;
    assert_eq!(
        bodies.len(),
        3,
        "main transcript plus both subagents, got: {bodies:#?}",
    );

    let main = bodies
        .iter()
        .find(|body| body.get("agent_id").is_none())
        .expect("the main transcript must be pushed");
    assert_eq!(main["session"]["harness_session_id"], SID);
    assert_eq!(main["session"]["harness_id"], "claude");
    assert_eq!(main["session"]["cwd"], CWD);
    assert_eq!(main["session"]["auth_subject"], "local:test");
    assert!(
        main["records"].is_array(),
        "records must be a JSON array, got: {}",
        main["records"],
    );

    for agent in ["a1", "a2"] {
        let sub = bodies
            .iter()
            .find(|body| body["agent_id"] == agent)
            .unwrap_or_else(|| panic!("subagent {agent} must be pushed"));
        assert_eq!(
            sub["tool_use_id"],
            format!("toolu_{agent}"),
            "the fork edge must ride along or the deriver cannot nest this agent",
        );
        assert_eq!(sub["agent_type"], "explorer");
        assert_eq!(sub["session"]["harness_session_id"], SID);
        // A subagent's rows are the launched session's rows. Filing them under
        // the sentinel would strand the whole fork subtree even though the
        // session id above is right.
        assert_eq!(sub["session"]["harness_id"], "claude");
    }
}

#[tokio::test]
async fn a_session_the_proxy_never_attributed_is_not_pushed() {
    // The tracker is the scope rule: transcripts on disk for a session whose
    // traffic did not flow through this proxy belong to someone else's capture.
    let server = ingest_server().await;
    let tree = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let root = transcript_tree(tree.path(), &["a1"]);

    let client = TranscriptClient::new(&Url::parse(&server.uri()).unwrap()).unwrap();
    let config = TailerConfig::new(root, sessions.path().to_path_buf(), "local:test".to_owned());
    let (shutdown, handle) = tailer::spawn(client, SessionTracker::new(), config);

    shutdown.send(()).unwrap();
    handle.await.unwrap();

    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_dead_ingest_server_does_not_hang_the_shutdown() {
    // `tapesctl start` awaits this task, so a transcript endpoint that refuses
    // every request must still let the command exit — the harness has already
    // finished, and the user is waiting on their prompt.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/ingest/transcript"))
        .respond_with(ResponseTemplate::new(502).set_body_string("nope"))
        .mount(&server)
        .await;
    let tree = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let root = transcript_tree(tree.path(), &["a1"]);

    let tracker = SessionTracker::new();
    tracker.observe(&session_file(4242, SID, CWD));
    let client = TranscriptClient::new(&Url::parse(&server.uri()).unwrap()).unwrap();
    let config = TailerConfig::new(root, sessions.path().to_path_buf(), "local:test".to_owned());
    let (shutdown, handle) = tailer::spawn(client, tracker, config);

    shutdown.send(()).unwrap();
    let finished = tokio::time::timeout(Duration::from_secs(10), handle).await;

    assert!(
        finished.is_ok(),
        "the tailer must finish its shutdown pass even when every push fails",
    );
    finished.unwrap().unwrap();
}

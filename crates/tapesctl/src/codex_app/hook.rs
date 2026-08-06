//! `tapesctl plugin hook <harness> --handoff <path>` — the command an
//! installed hook plugin runs at every lifecycle boundary.
//!
//! Codex writes the boundary's payload to this process's stdin and waits for
//! it. That single fact decides nearly everything here.
//!
//! # It never fails
//!
//! A hook that exits non-zero, or that hangs, degrades the app the user is
//! trying to work in — for telemetry. So every path in this module ends in
//! `Ok(())`: an unreadable handoff, a payload the crate refuses, a capture
//! proxy that is not running, a proxy that answers 401. Each is logged and
//! then dropped. The consequence of dropping one is well defined and is the
//! rule this harness is held to: traffic whose session was never introduced
//! files under `unknown`, and nothing is guessed.
//!
//! For the same reason nothing is written to stdout. Codex reads a hook's
//! stdout, so a diagnostic printed there is a diagnostic injected into the
//! user's session.
//!
//! # It sends the projection, not the payload
//!
//! The payload on stdin carries the user's prompt, the assistant's output, and
//! whatever else Codex chose to include. [`parse_observation`] allowlists it
//! away, and this process sends only the
//! [`LifecycleReport`](super::LifecycleReport) projection of what survived —
//! so the prompt does not cross the loopback socket, does not reach the proxy,
//! and cannot end up in the proxy's logs. Allowlisting at the receiver instead
//! would put all three back on the table.

use std::io::Read as _;
use std::path::Path;
use std::time::Duration;

use tapes_harnesses::attribution::codex_app::parse_observation;
use tracing::{debug, warn};

use super::{Handoff, LIFECYCLE_SECRET_HEADER, LifecycleReport};
use crate::cli::PluginHookArgs;
use crate::error::Result;

/// How much of stdin is read before the payload is refused.
///
/// A lifecycle payload is metadata plus at most one prompt or assistant
/// message. A megabyte is far past any of those and still small enough that a
/// runaway producer cannot make this process the memory problem.
pub const MAX_PAYLOAD_BYTES: usize = 1024 * 1024;

/// How long a report may take before the hook gives up on it.
///
/// The app is blocked on this process for the whole of it, so the budget is
/// set by what a user would notice rather than by what a loopback POST could
/// conceivably need. The receiver is on the same machine; if it has not
/// answered in two seconds it is not going to.
pub const REPORT_TIMEOUT: Duration = Duration::from_secs(2);

/// Run one hook invocation. Always succeeds; see the module docs.
pub async fn run(args: &PluginHookArgs) -> Result<()> {
    let mut payload = Vec::new();
    if let Err(err) = read_payload(&mut std::io::stdin().lock(), &mut payload) {
        warn!(error = %err, "could not read the lifecycle payload; reporting nothing");
        return Ok(());
    }
    report(&args.harness, &args.handoff, &payload).await;
    Ok(())
}

/// Read stdin under [`MAX_PAYLOAD_BYTES`].
///
/// Reading one byte past the cap is what makes "exactly at the cap" a success
/// and anything larger an error, without a second `read` to find out.
fn read_payload(source: &mut impl std::io::Read, into: &mut Vec<u8>) -> std::io::Result<()> {
    let read = source
        .take(MAX_PAYLOAD_BYTES as u64 + 1)
        .read_to_end(into)?;
    if read > MAX_PAYLOAD_BYTES {
        into.clear();
        return Err(std::io::Error::other(format!(
            "lifecycle payload exceeded {MAX_PAYLOAD_BYTES} bytes",
        )));
    }
    Ok(())
}

/// Parse, project, and deliver — logging and swallowing every failure.
async fn report(harness: &str, handoff_path: &Path, payload: &[u8]) {
    if let Err(err) = super::resolve_hook_harness(harness) {
        warn!(harness, error = %err, "not a hook-captured harness; reporting nothing");
        return;
    }
    let handoff = match Handoff::read(handoff_path) {
        Ok(handoff) => handoff,
        Err(err) => {
            warn!(
                path = %handoff_path.display(),
                error = %err,
                "no usable handoff; the session will be captured as unattributed",
            );
            return;
        }
    };
    // The crate decides what a lifecycle boundary is and which of its fields
    // may survive. An event it does not recognise is a refusal here, not a
    // passthrough — the same closed allowlist on both ends of the hook.
    let observation = match parse_observation(payload) {
        Ok(observation) => observation,
        Err(err) => {
            warn!(error = %err, "unrecognised lifecycle payload; reporting nothing");
            return;
        }
    };
    let url = match handoff.lifecycle_url() {
        Ok(url) => url,
        Err(err) => {
            warn!(error = %err, "the handoff does not name a usable report endpoint");
            return;
        }
    };

    let client = match reqwest::Client::builder().timeout(REPORT_TIMEOUT).build() {
        Ok(client) => client,
        Err(err) => {
            warn!(error = %err, "could not build the report client");
            return;
        }
    };
    let response = client
        .post(url)
        // The secret proves this report came from the installation that wrote
        // the handoff. It is never logged, here or at the receiver.
        .header(LIFECYCLE_SECRET_HEADER, &handoff.secret)
        .json(&LifecycleReport::from_observation(&observation))
        .send()
        .await;

    match response {
        Ok(response) if response.status().is_success() => {
            debug!(session_id = %observation.session_id, "lifecycle boundary reported");
        }
        Ok(response) => warn!(
            status = response.status().as_u16(),
            "the capture proxy refused the lifecycle report; \
             this session's turns will be captured as unattributed",
        ),
        // By far the most common case, and not a defect: nobody is capturing
        // right now. Debug rather than warn, or every desktop session on a
        // machine with the plugin installed logs five warnings.
        Err(err) => debug!(error = %err, "no capture proxy answered the lifecycle report"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use tapes_harnesses::attribution::codex_app::LifecycleEvent;

    fn session_start() -> Vec<u8> {
        serde_json::json!({
            "hook_event_name": "SessionStart",
            "session_id": "root-session",
            "cwd": "/tmp/work",
            "model": "gpt-5-codex",
            "source": "startup",
        })
        .to_string()
        .into_bytes()
    }

    /// The projection is what crosses the socket, so the prompt has to be gone
    /// before the bytes are ever serialized — not filtered at the far end.
    #[test]
    fn the_report_carries_identity_and_never_the_prompt() {
        let payload = serde_json::json!({
            "hook_event_name": "UserPromptSubmit",
            "session_id": "root-session",
            "cwd": "/tmp/work",
            "turn_id": "turn-1",
            "prompt": "prompt-secret-must-never-escape",
        })
        .to_string();

        let observation = parse_observation(payload.as_bytes()).unwrap();
        let report = LifecycleReport::from_observation(&observation);
        assert_eq!(report.session_id, "root-session");
        assert_eq!(report.cwd, "/tmp/work");
        assert_eq!(report.agent_id, None);

        let wire = serde_json::to_string(&report).unwrap();
        assert!(
            !wire.contains("prompt-secret-must-never-escape"),
            "got: {wire}"
        );
        assert!(!wire.contains("turn-1"), "got: {wire}");
    }

    /// On a subagent boundary the root stays the key and the child is named
    /// beside it — the pair that lets a sub-thread's traffic find its session.
    #[test]
    fn a_subagent_boundary_names_the_child_beside_the_root() {
        let payload = serde_json::json!({
            "hook_event_name": "SubagentStart",
            "session_id": "root-session",
            "cwd": "/tmp/work",
            "turn_id": "turn-1",
            "agent_id": "child-thread",
            "agent_type": "explorer",
        })
        .to_string();

        let report =
            LifecycleReport::from_observation(&parse_observation(payload.as_bytes()).unwrap());
        assert_eq!(report.session_id, "root-session");
        assert_eq!(report.agent_id.as_deref(), Some("child-thread"));
    }

    #[test]
    fn a_report_round_trips_through_its_wire_form() {
        let report =
            LifecycleReport::from_observation(&parse_observation(&session_start()).unwrap());
        let wire = serde_json::to_string(&report).unwrap();
        assert_eq!(
            serde_json::from_str::<LifecycleReport>(&wire).unwrap(),
            report
        );
    }

    #[test]
    fn a_stop_boundary_carries_no_child() {
        let payload = serde_json::json!({
            "hook_event_name": "Stop",
            "session_id": "root-session",
            "cwd": "/tmp/work",
            "turn_id": "turn-1",
        })
        .to_string();
        let observation = parse_observation(payload.as_bytes()).unwrap();
        assert!(matches!(observation.event, LifecycleEvent::Stop { .. }));
        assert_eq!(
            LifecycleReport::from_observation(&observation).agent_id,
            None
        );
    }

    #[test]
    fn a_payload_at_the_cap_is_read_and_one_past_it_is_refused() {
        let mut at_cap = Vec::new();
        read_payload(
            &mut std::io::Cursor::new(vec![b'x'; MAX_PAYLOAD_BYTES]),
            &mut at_cap,
        )
        .unwrap();
        assert_eq!(at_cap.len(), MAX_PAYLOAD_BYTES);

        let mut over = Vec::new();
        let err = read_payload(
            &mut std::io::Cursor::new(vec![b'x'; MAX_PAYLOAD_BYTES + 1]),
            &mut over,
        )
        .unwrap_err();
        assert!(err.to_string().contains("exceeded"), "got: {err}");
        assert!(over.is_empty(), "an over-cap payload must not be retained");
    }

    /// Every failure the hook can meet has to end quietly: the app is blocked
    /// on this process, and a non-zero exit degrades the session the user is
    /// working in.
    #[tokio::test]
    async fn no_handoff_no_proxy_and_a_junk_payload_all_end_quietly() {
        let dir = tempfile::tempdir().unwrap();
        let absent = dir.path().join("handoff.json");

        // No handoff at all.
        report("codex-app", &absent, &session_start()).await;
        // A handoff naming a port nothing serves.
        let handoff = dir.path().join("present.json");
        std::fs::write(
            &handoff,
            serde_json::json!({
                "version": super::super::HANDOFF_VERSION,
                "harness_id": "codex-app",
                // Port 1 on loopback: reserved, and nothing this test runs is
                // listening on it.
                "proxy_addr": "127.0.0.1:1",
                "secret": "0".repeat(64),
                "provider_id": super::super::PROVIDER_ID,
                "installed_at": "2026-08-05T00:00:00Z",
            })
            .to_string(),
        )
        .unwrap();
        report("codex-app", &handoff, &session_start()).await;
        // A payload the crate refuses.
        report("codex-app", &handoff, b"not a lifecycle event").await;
        // A harness with no hook surface.
        report("pi", &handoff, &session_start()).await;
    }
}

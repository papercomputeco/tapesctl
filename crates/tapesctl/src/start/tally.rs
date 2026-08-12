//! What a launch actually captured, and how to say it out loud.
//!
//! # Why a count and not a flag
//!
//! The exit summary used to rest on one bit: did any turn resolve a session?
//! If none did, it printed "no turns were captured" — which is true only when
//! the proxy saw no conversation at all. A launch that captured turns and
//! filed every one of them under `unknown` printed the same sentence, and that
//! is the failure this module exists to make unsayable: silent unattribution
//! reported as silence.
//!
//! Those two outcomes need different words because they need different
//! reactions. Nothing captured is usually benign — a harness opened and quit
//! without calling a model. Captured but unattributed is always a bug: the
//! turns are on the server, under a session nobody can find by name. So the
//! tally keeps both numbers, and [`unattributed_warning`] goes to stderr
//! whenever the second one is non-zero.
//!
//! # What counts as captured
//!
//! A turn is counted when ingest has accepted it, not when the proxy decided
//! to capture it. The summary's job is to tell the caller what they can go and
//! look at; a turn the server rejected is not that, and it already has its own
//! warning on the log path.
//!
//! The session link obeys the same rule, and that is not a detail. Attribution
//! *resolves* while the request is being forwarded, long before the turn is
//! posted — so a session id is known for turns ingest goes on to reject. A
//! summary that took its link from whichever session resolved first could point
//! at a session holding none of the turns that landed, while the launch's real
//! outcome — captured, unattributed — went unsaid. So the id is recorded here
//! by the accepting capture task, from the envelope that turn was filed under,
//! and there is nowhere else for the link to come from: [`CaptureSnapshot`]
//! carries the only session id the summary can see, and a rejected turn never
//! puts one there.
//!
//! # Why the tally has to be drained
//!
//! A capture is finished on a task detached from the exchange that produced it,
//! because the alternative — finishing it inline — puts an ingest round trip in
//! front of the last SSE token the user is watching arrive. The cost is that
//! nothing in the shutdown path naturally waits for those tasks: a harness that
//! exits the instant its final response lands leaves that turn's POST in
//! flight, and a summary taken right then is short by exactly the turns the
//! caller most recently made. In the one-turn case it is short by all of them,
//! and prints the "no turns were captured" sentence this module exists to stop
//! printing.
//!
//! Hence [`CaptureTally::begin`], which registers a capture before it is
//! spawned, and [`CaptureTally::drain`], which waits for the registered ones to
//! finish. The wait is bounded: ingest has no request timeout of its own, so an
//! unbounded drain would let a wedged server hold a terminal the harness has
//! already given back. Whatever has not landed by the deadline is reported as
//! still in flight — see [`CaptureSnapshot::in_flight`] — rather than waited on
//! or silently dropped from the count.

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::Notify;

/// How long shutdown waits for in-flight captures before printing the summary.
///
/// Long enough for POSTs already on the wire to a healthy local ingest, short
/// enough that a wedged one costs the caller a pause rather than a hang. The
/// ingest client sets no request timeout, so this is the only bound there is.
pub const CAPTURE_DRAIN_DEADLINE: Duration = Duration::from_secs(5);

/// Running totals for one launch's proxy, shared with every capture task.
///
/// The two counters and the session slot are written by independent capture
/// tasks and read once, after [`drain`](Self::drain). They are `Relaxed`
/// because they are not what orders that read: `in_flight` is, and it is
/// `AcqRel`/`Acquire`. Observing it at zero synchronises with every capture
/// task's release of it, which is sequenced after that task's writes here — so
/// a drained tally reads back everything its captures recorded.
#[derive(Debug, Default)]
pub struct CaptureTally {
    captured: AtomicUsize,
    unattributed: AtomicUsize,
    /// The session the first accepted attributed turn was filed under.
    ///
    /// A `OnceLock` because the link names one session and later turns must not
    /// change which — the same "first one wins" rule the proxy used when it
    /// announced sessions, minus the announcement's fatal property of firing
    /// before ingest had accepted anything.
    session: OnceLock<String>,
    /// Captures spawned and not yet finished.
    in_flight: AtomicUsize,
    /// Woken when `in_flight` reaches zero.
    idle: Notify,
}

impl CaptureTally {
    /// A tally with nothing counted yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a capture that is about to be spawned.
    ///
    /// Call this *before* the spawn and move the guard into the task. Between
    /// the two there must be no instant at which the tally looks idle while a
    /// capture is still owed to it, and a guard created inside the task would
    /// leave exactly that gap — the spawn returns immediately, the task may not
    /// be polled for a while, and a shutdown landing in between would drain a
    /// tally that has already been promised a turn.
    #[must_use]
    pub fn begin(self: &Arc<Self>) -> CaptureInFlight {
        self.in_flight.fetch_add(1, Ordering::AcqRel);
        CaptureInFlight {
            tally: Arc::clone(self),
        }
    }

    /// Record one turn that ingest accepted.
    ///
    /// `session_id` is the session it was filed under, and `None` means it
    /// landed under `unknown` — the two are the same fact, so they are one
    /// argument rather than a flag beside an id that could disagree with it.
    pub fn record(&self, session_id: Option<&str>) {
        self.captured.fetch_add(1, Ordering::Relaxed);
        match session_id {
            // `set` fails only when a session is already nominated, which is
            // the intended outcome: the first accepted attributed turn names
            // the link.
            Some(id) => drop(self.session.set(id.to_owned())),
            None => drop(self.unattributed.fetch_add(1, Ordering::Relaxed)),
        }
    }

    /// Wait for the registered captures to finish, for at most `grace`.
    ///
    /// Returns when the last one finishes or when the deadline passes,
    /// whichever comes first — never later, because exit must not be hostage to
    /// an ingest that has stopped answering.
    pub async fn drain(&self, grace: Duration) {
        let _ = tokio::time::timeout(grace, self.idle()).await;
    }

    /// Resolve once no capture is in flight. Waits forever if one never ends,
    /// which is why the only caller wraps it in a timeout.
    async fn idle(&self) {
        loop {
            // Armed before the count is read, not after. `notify_waiters`
            // stores no permit — it wakes whoever is registered at the instant
            // it fires — so a capture finishing between the load and the await
            // would be a wake-up this loop slept through.
            let notified = self.idle.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            if self.in_flight.load(Ordering::Acquire) == 0 {
                break;
            }
            notified.await;
        }
    }

    /// Read the counters, the nominated session, and what is still owed.
    ///
    /// One read of all four so they cannot disagree: a caller that read the
    /// counts and then the in-flight depth could see a capture land in between
    /// and report a remainder that is already included.
    #[must_use]
    pub fn snapshot(&self) -> CaptureSnapshot {
        CaptureSnapshot {
            in_flight: self.in_flight.load(Ordering::Acquire),
            counts: CaptureCounts {
                captured: self.captured.load(Ordering::Relaxed),
                unattributed: self.unattributed.load(Ordering::Relaxed),
            },
            session: self.session.get().cloned(),
        }
    }
}

/// One registered capture, counted until this is dropped.
///
/// A guard rather than a matching `finish()` call so a capture that returns
/// early — and [`TurnCapture::run`](super::proxy) has half a dozen early
/// returns — or panics outright still releases the drain. Anything else would
/// turn one abandoned capture into a shutdown that waits out the whole
/// deadline.
#[derive(Debug)]
pub struct CaptureInFlight {
    tally: Arc<CaptureTally>,
}

impl Drop for CaptureInFlight {
    fn drop(&mut self) {
        if self.tally.in_flight.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.tally.idle.notify_waiters();
        }
    }
}

/// One reading of a [`CaptureTally`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CaptureSnapshot {
    /// Turns ingest accepted, and how many went unattributed.
    pub counts: CaptureCounts,
    /// The session an accepted attributed turn was filed under, if any.
    ///
    /// The summary's only source for a link. `None` covers both "nothing was
    /// attributed" and "the attributed turns were rejected", which are the same
    /// thing as far as a link is concerned: there is no session the caller can
    /// open and find this launch's turns in.
    pub session: Option<String>,
    /// Captures still unfinished when this was read.
    ///
    /// Non-zero only after a [`drain`](CaptureTally::drain) hit its deadline,
    /// and it means the counts beside it are a floor rather than a total.
    pub in_flight: usize,
}

/// Turn totals for one launch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CaptureCounts {
    /// Turns ingest accepted, attributed or not.
    pub captured: usize,
    /// How many of those carried no session and were filed under `unknown`.
    pub unattributed: usize,
}

impl CaptureCounts {
    /// Turns that landed under a session someone can look up.
    ///
    /// Saturating because the two counters are incremented independently: a
    /// torn read cannot underflow into a colossal count.
    #[must_use]
    pub fn attributed(&self) -> usize {
        self.captured.saturating_sub(self.unattributed)
    }

    /// Whether this launch has something to warn about.
    #[must_use]
    pub fn has_unattributed(&self) -> bool {
        self.unattributed > 0
    }
}

/// Which sentence a finished launch has earned.
///
/// Split out from the printing so the choice itself can be tested: the bug this
/// replaces was not in the wording, it was in collapsing three outcomes onto
/// two sentences.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitSummary {
    /// The proxy captured nothing. The only case allowed to say so.
    NothingCaptured,
    /// Turns landed under a session, which can be linked.
    Session(String),
    /// Turns landed, and none of them under a session.
    Unattributed(CaptureCounts),
}

/// Decide what to say about a finished launch.
///
/// Everything it reads is a record of what ingest accepted, which is what keeps
/// the three outcomes from bleeding into each other: the count decides whether
/// anything landed, and the session id — nominated by an accepted turn or by
/// nobody — decides whether what landed can be linked to.
#[must_use]
pub fn exit_summary_for(snapshot: &CaptureSnapshot) -> ExitSummary {
    if snapshot.counts.captured == 0 {
        ExitSummary::NothingCaptured
    } else if let Some(id) = snapshot.session.as_deref() {
        ExitSummary::Session(id.to_owned())
    } else {
        ExitSummary::Unattributed(snapshot.counts)
    }
}

/// `turn` or `turns`, for a count that is read by a person.
fn turns(count: usize) -> &'static str {
    if count == 1 { "turn" } else { "turns" }
}

/// The stdout line for a launch whose captures never resolved a session.
///
/// Reached only when nothing was attributed, so there is no session link to
/// print instead — but turns did land, and saying "no turns were captured"
/// here would send the caller looking for a bug in the proxy when the bug is
/// in attribution.
#[must_use]
pub fn unattributed_line(counts: CaptureCounts) -> String {
    format!(
        "tapesctl: captured {} {} ({} unattributed — filed as unknown)",
        counts.captured,
        turns(counts.captured),
        counts.unattributed,
    )
}

/// The stderr warning for any launch that filed a turn under `unknown`.
///
/// Separate from the stdout line because it fires in the mixed case too, where
/// the session link is printed and is true — and still incomplete, because some
/// of the session's turns are not under it.
#[must_use]
pub fn unattributed_warning(counts: CaptureCounts) -> String {
    format!(
        "tapesctl: warning: {} captured {} could not be attributed to this \
         session and {} filed as unknown",
        counts.unattributed,
        turns(counts.unattributed),
        if counts.unattributed == 1 {
            "was"
        } else {
            "were"
        },
    )
}

/// The stderr note for a launch whose captures outlived the drain deadline.
///
/// The summary above it is then a floor, not a total, and saying so is the
/// whole point: a count printed without this note claims to be everything.
#[must_use]
pub fn in_flight_note(in_flight: usize) -> String {
    format!(
        "tapesctl: warning: {} {} still being captured at exit; the counts above may be short",
        in_flight,
        turns(in_flight),
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// A snapshot as a launch that never drained anything would read it.
    fn snapshot(captured: usize, unattributed: usize, session: Option<&str>) -> CaptureSnapshot {
        CaptureSnapshot {
            counts: CaptureCounts {
                captured,
                unattributed,
            },
            session: session.map(str::to_owned),
            in_flight: 0,
        }
    }

    #[test]
    fn a_fresh_tally_has_captured_nothing() {
        assert_eq!(CaptureTally::new().snapshot(), CaptureSnapshot::default());
    }

    #[test]
    fn an_attributed_turn_counts_as_captured_but_not_as_unattributed() {
        let tally = CaptureTally::new();
        tally.record(Some("sid-1"));
        let got = tally.snapshot();
        assert_eq!(got.counts.captured, 1);
        assert_eq!(got.counts.unattributed, 0);
        assert_eq!(got.counts.attributed(), 1);
        assert!(!got.counts.has_unattributed());
        assert_eq!(got.session.as_deref(), Some("sid-1"));
    }

    #[test]
    fn an_unattributed_turn_counts_in_both() {
        // The whole point: an unattributed turn is still a capture. Counting it
        // only as a failure would leave `captured` at zero and reproduce the
        // "no turns were captured" lie this module exists to remove.
        let tally = CaptureTally::new();
        tally.record(None);
        let got = tally.snapshot();
        assert_eq!(got.counts.captured, 1);
        assert_eq!(got.counts.unattributed, 1);
        assert_eq!(got.counts.attributed(), 0);
        assert!(got.counts.has_unattributed());
        assert_eq!(got.session, None);
    }

    #[test]
    fn a_mixed_launch_reports_both_halves() {
        let tally = CaptureTally::new();
        tally.record(Some("sid-1"));
        tally.record(None);
        tally.record(Some("sid-1"));
        let got = tally.snapshot();
        assert_eq!(got.counts.captured, 3);
        assert_eq!(got.counts.unattributed, 1);
        assert_eq!(got.counts.attributed(), 2);
    }

    #[test]
    fn the_first_accepted_session_is_the_one_that_is_linked() {
        // One launch, one link. A subagent turn arriving under its own session
        // id must not retarget the link the main session already earned.
        let tally = CaptureTally::new();
        tally.record(Some("sid-first"));
        tally.record(Some("sid-second"));
        assert_eq!(tally.snapshot().session.as_deref(), Some("sid-first"));
    }

    #[test]
    fn the_unattributed_line_names_both_numbers() {
        let line = unattributed_line(CaptureCounts {
            captured: 4,
            unattributed: 4,
        });
        assert_eq!(
            line,
            "tapesctl: captured 4 turns (4 unattributed — filed as unknown)"
        );
    }

    #[test]
    fn the_unattributed_line_never_claims_nothing_was_captured() {
        // The regression guard: whatever this line says, it must not be the
        // sentence reserved for a launch that captured nothing.
        let line = unattributed_line(CaptureCounts {
            captured: 2,
            unattributed: 2,
        });
        assert!(!line.contains("no turns were captured"), "got: {line}");
        assert!(line.contains("captured 2 turns"), "got: {line}");
    }

    #[test]
    fn one_turn_is_singular_in_both_messages() {
        let counts = CaptureCounts {
            captured: 1,
            unattributed: 1,
        };
        assert_eq!(
            unattributed_line(counts),
            "tapesctl: captured 1 turn (1 unattributed — filed as unknown)"
        );
        assert_eq!(
            unattributed_warning(counts),
            "tapesctl: warning: 1 captured turn could not be attributed to this \
             session and was filed as unknown"
        );
    }

    #[test]
    fn a_launch_that_captured_nothing_says_so() {
        assert_eq!(
            exit_summary_for(&CaptureSnapshot::default()),
            ExitSummary::NothingCaptured
        );
    }

    #[test]
    fn a_launch_that_attributed_its_turns_gets_the_session() {
        assert_eq!(
            exit_summary_for(&snapshot(2, 0, Some("sid-9"))),
            ExitSummary::Session("sid-9".to_owned())
        );
    }

    #[test]
    fn a_launch_that_captured_only_unattributed_turns_does_not_claim_silence() {
        // The regression this module exists for. Before the split, this launch
        // and the captured-nothing launch printed the same sentence, so the
        // four harness defects that silently stopped attributing all looked
        // like a session that never called a model.
        let got = snapshot(3, 3, None);
        assert_eq!(
            exit_summary_for(&got),
            ExitSummary::Unattributed(got.counts)
        );
        assert_ne!(exit_summary_for(&got), ExitSummary::NothingCaptured);
    }

    #[test]
    fn a_mixed_launch_keeps_the_session_and_still_has_something_to_warn_about() {
        let got = snapshot(5, 2, Some("sid-9"));
        assert_eq!(
            exit_summary_for(&got),
            ExitSummary::Session("sid-9".to_owned())
        );
        assert!(
            got.counts.has_unattributed(),
            "the warning is what covers the turns the link does not"
        );
    }

    #[test]
    fn a_rejected_attributed_turn_never_nominates_the_link() {
        // The race this rule exists for. Attribution resolves while the request
        // is still being forwarded, so the session id of a turn ingest goes on
        // to reject is known — and used to be the id the summary linked. Here
        // the attributed turn is rejected (so it is never recorded) and the
        // turns that do land are unattributed: the honest summary is the
        // unattributed one, and a link to the rejected turn's session would
        // point at a session holding none of them.
        let tally = CaptureTally::new();
        tally.record(None);
        tally.record(None);
        let got = tally.snapshot();

        assert_eq!(got.session, None, "a rejected turn recorded nothing");
        assert_eq!(
            exit_summary_for(&got),
            ExitSummary::Unattributed(got.counts)
        );
    }

    #[test]
    fn an_announced_session_whose_turns_never_landed_is_not_reported_as_captured() {
        // The same rule with nothing accepted at all: a launch whose only turn
        // was rejected has captured nothing, however well attributed that turn
        // was on the way out.
        assert_eq!(
            exit_summary_for(&CaptureTally::new().snapshot()),
            ExitSummary::NothingCaptured
        );
    }

    #[test]
    fn the_warning_counts_only_the_unattributed_turns() {
        // In a mixed launch the link is printed for the attributed turns, so
        // the warning must speak only for the ones that went missing.
        let counts = CaptureCounts {
            captured: 9,
            unattributed: 2,
        };
        assert_eq!(
            unattributed_warning(counts),
            "tapesctl: warning: 2 captured turns could not be attributed to this \
             session and were filed as unknown"
        );
    }

    #[tokio::test]
    async fn draining_an_idle_tally_returns_at_once() {
        let tally = Arc::new(CaptureTally::new());
        // A deadline long enough that returning within it proves the drain did
        // not wait for it.
        tokio::time::timeout(
            Duration::from_secs(5),
            tally.drain(Duration::from_secs(600)),
        )
        .await
        .expect("an idle tally has nothing to wait for");
    }

    #[tokio::test]
    async fn a_drain_waits_for_a_capture_that_records_late() {
        // The shutdown race in miniature: the capture is registered before the
        // task that records it is spawned, and the tally is read only after the
        // drain. Without the wait the read would happen while the sleep is
        // still running and report zero.
        let tally = Arc::new(CaptureTally::new());
        let in_flight = tally.begin();
        assert_eq!(tally.snapshot().counts.captured, 0);

        let recorder = Arc::clone(&tally);
        tokio::spawn(async move {
            let _in_flight = in_flight;
            tokio::time::sleep(Duration::from_millis(150)).await;
            recorder.record(Some("sid-late"));
        });

        tally.drain(Duration::from_secs(5)).await;
        let got = tally.snapshot();
        assert_eq!(got.counts.captured, 1, "the late capture must be counted");
        assert_eq!(got.session.as_deref(), Some("sid-late"));
        assert_eq!(got.in_flight, 0, "a clean drain owes nothing");
    }

    #[tokio::test]
    async fn a_drain_gives_up_on_a_capture_that_never_finishes() {
        // Bounded, because ingest sets no request timeout of its own: a POST to
        // a server that has stopped answering must cost the caller the deadline
        // and not the terminal.
        let tally = Arc::new(CaptureTally::new());
        let in_flight = tally.begin();
        tokio::spawn(async move {
            let _in_flight = in_flight;
            std::future::pending::<()>().await;
        });

        tally.drain(Duration::from_millis(50)).await;
        let got = tally.snapshot();
        assert_eq!(got.counts.captured, 0);
        assert_eq!(
            got.in_flight, 1,
            "what the drain gave up on is reported, not dropped",
        );
        assert_eq!(
            in_flight_note(got.in_flight),
            "tapesctl: warning: 1 turn still being captured at exit; \
             the counts above may be short",
        );
    }

    /// Register a capture and record it, unattributed, after `delay`.
    fn record_after(tally: &Arc<CaptureTally>, delay: Duration) {
        let in_flight = tally.begin();
        let recorder = Arc::clone(tally);
        tokio::spawn(async move {
            let _in_flight = in_flight;
            tokio::time::sleep(delay).await;
            recorder.record(None);
        });
    }

    #[tokio::test]
    async fn a_drain_waits_for_every_registered_capture() {
        // One slow capture among several must hold the drain: the summary is a
        // total, and a partial one is the undercount this drain exists to stop.
        let tally = Arc::new(CaptureTally::new());
        for delay in [0, 20, 120] {
            record_after(&tally, Duration::from_millis(delay));
        }

        tally.drain(Duration::from_secs(5)).await;
        let got = tally.snapshot();
        assert_eq!(got.counts.captured, 3);
        assert_eq!(got.counts.unattributed, 3);
        assert_eq!(got.in_flight, 0);
    }
}

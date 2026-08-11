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

use std::sync::atomic::{AtomicUsize, Ordering};

/// Running totals for one launch's proxy, shared with every capture task.
///
/// Both counters are `Relaxed`: they are incremented from independent capture
/// tasks and read once, after the server has shut down and those tasks have
/// been joined. That read is ordered by the shutdown, not by these atomics, so
/// nothing here needs to synchronise anything else.
#[derive(Debug, Default)]
pub struct CaptureTally {
    captured: AtomicUsize,
    unattributed: AtomicUsize,
}

impl CaptureTally {
    /// A tally with nothing counted yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one turn that ingest accepted, and whether it carried a session.
    pub fn record(&self, attributed: bool) {
        self.captured.fetch_add(1, Ordering::Relaxed);
        if !attributed {
            self.unattributed.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Read both counters.
    #[must_use]
    pub fn snapshot(&self) -> CaptureCounts {
        CaptureCounts {
            captured: self.captured.load(Ordering::Relaxed),
            unattributed: self.unattributed.load(Ordering::Relaxed),
        }
    }
}

/// One reading of a [`CaptureTally`].
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
/// The count leads, not the announced session id. A session id is announced
/// when attribution *resolves*, which happens before the turn is posted — so a
/// launch whose only turn was then rejected by ingest has a session id and
/// nothing to show for it, and "no turns were captured" is the honest answer.
#[must_use]
pub fn exit_summary_for(session_id: Option<&str>, counts: CaptureCounts) -> ExitSummary {
    if counts.captured == 0 {
        ExitSummary::NothingCaptured
    } else if let Some(id) = session_id {
        ExitSummary::Session(id.to_owned())
    } else {
        ExitSummary::Unattributed(counts)
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_tally_has_captured_nothing() {
        assert_eq!(
            CaptureTally::new().snapshot(),
            CaptureCounts {
                captured: 0,
                unattributed: 0
            }
        );
    }

    #[test]
    fn an_attributed_turn_counts_as_captured_but_not_as_unattributed() {
        let tally = CaptureTally::new();
        tally.record(true);
        let counts = tally.snapshot();
        assert_eq!(counts.captured, 1);
        assert_eq!(counts.unattributed, 0);
        assert_eq!(counts.attributed(), 1);
        assert!(!counts.has_unattributed());
    }

    #[test]
    fn an_unattributed_turn_counts_in_both() {
        // The whole point: an unattributed turn is still a capture. Counting it
        // only as a failure would leave `captured` at zero and reproduce the
        // "no turns were captured" lie this module exists to remove.
        let tally = CaptureTally::new();
        tally.record(false);
        let counts = tally.snapshot();
        assert_eq!(counts.captured, 1);
        assert_eq!(counts.unattributed, 1);
        assert_eq!(counts.attributed(), 0);
        assert!(counts.has_unattributed());
    }

    #[test]
    fn a_mixed_launch_reports_both_halves() {
        let tally = CaptureTally::new();
        tally.record(true);
        tally.record(false);
        tally.record(true);
        let counts = tally.snapshot();
        assert_eq!(counts.captured, 3);
        assert_eq!(counts.unattributed, 1);
        assert_eq!(counts.attributed(), 2);
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
            exit_summary_for(None, CaptureCounts::default()),
            ExitSummary::NothingCaptured
        );
    }

    #[test]
    fn a_launch_that_attributed_its_turns_gets_the_session() {
        let counts = CaptureCounts {
            captured: 2,
            unattributed: 0,
        };
        assert_eq!(
            exit_summary_for(Some("sid-9"), counts),
            ExitSummary::Session("sid-9".to_owned())
        );
    }

    #[test]
    fn a_launch_that_captured_only_unattributed_turns_does_not_claim_silence() {
        // The regression this module exists for. Before the split, this launch
        // and the captured-nothing launch printed the same sentence, so the
        // four harness defects that silently stopped attributing all looked
        // like a session that never called a model.
        let counts = CaptureCounts {
            captured: 3,
            unattributed: 3,
        };
        assert_eq!(
            exit_summary_for(None, counts),
            ExitSummary::Unattributed(counts)
        );
        assert_ne!(exit_summary_for(None, counts), ExitSummary::NothingCaptured);
    }

    #[test]
    fn a_mixed_launch_keeps_the_session_and_still_has_something_to_warn_about() {
        let counts = CaptureCounts {
            captured: 5,
            unattributed: 2,
        };
        assert_eq!(
            exit_summary_for(Some("sid-9"), counts),
            ExitSummary::Session("sid-9".to_owned())
        );
        assert!(
            counts.has_unattributed(),
            "the warning is what covers the turns the link does not"
        );
    }

    #[test]
    fn an_announced_session_whose_turns_never_landed_is_not_reported_as_captured() {
        // Attribution resolves before the turn is posted, so a session id can
        // be announced for a turn ingest then rejected. Counting decides.
        assert_eq!(
            exit_summary_for(Some("sid-9"), CaptureCounts::default()),
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
}

//! Compile-time regression checks for the transcript library surface.

use std::future::Future;

use tapesctl::cli::SyncArgs;
use tapesctl::transcript::client::{TranscriptClient, UploadOutcome};
use tapesctl::transcript::sync::{SyncConfig, SyncSummary};

#[test]
fn transcript_outcomes_and_sync_keep_their_original_public_shapes() {
    let outcome = UploadOutcome {
        deduped: true,
        records: 7,
    };
    let UploadOutcome { deduped, records } = outcome;
    assert!(deduped);
    assert_eq!(records, 7);

    let summary = SyncSummary {
        sessions: 1,
        files: 2,
        stored: 1,
        deduped: 1,
        failed: 0,
    };
    assert_eq!(summary.stored + summary.deduped, summary.files);

    #[allow(clippy::result_large_err)]
    fn run_compat(args: SyncArgs) -> impl Future<Output = tapesctl::Result<()>> {
        tapesctl::transcript::sync::run(args)
    }
    fn sweep_compat<'a>(
        client: &'a TranscriptClient,
        config: &'a SyncConfig,
    ) -> impl Future<Output = SyncSummary> + 'a {
        tapesctl::transcript::sync::sweep_into(client, config)
    }

    let _ = run_compat;
    let _ = sweep_compat;
}

//! Error type for the `tapesctl` CLI.

use std::path::PathBuf;

use snafu::Snafu;

/// Convenience alias defaulting the error to this crate's [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Errors surfaced by the CLI.
///
/// Note what is *not* here: a failure to capture a turn. Capture problems — an
/// oversize body, an ingest rejection, a request body that is not JSON — are
/// logged and the turn is skipped, because a telemetry failure must never take
/// the harness down with it. Only failures that stop the session from running
/// at all reach this type.
#[derive(Debug, Snafu)]
#[snafu(module, visibility(pub(crate)))]
pub enum Error {
    /// A command is wired up but its implementation has not landed yet.
    #[snafu(display("{what} is not implemented yet"))]
    NotImplemented {
        /// The command that was invoked.
        what: &'static str,
    },

    /// The named harness has no launch recipe here.
    #[snafu(display("unsupported harness {harness:?} (supported: claude, codex)"))]
    UnsupportedHarness {
        /// What the user asked for.
        harness: String,
    },

    /// No ingest server was named, so there would be nowhere to send turns.
    /// Failing loudly beats running a capture session that captures nothing.
    #[snafu(display("no tapes ingest URL: pass --tapes-url or set TAPES_URL"))]
    MissingTapesUrl,

    /// `--tapes-url` was not a URL.
    #[snafu(display("invalid tapes URL"))]
    TapesUrl {
        /// Underlying parse failure.
        source: url::ParseError,
    },

    /// `--upstream` was not a URL.
    #[snafu(display("invalid upstream URL"))]
    UpstreamUrl {
        /// Underlying parse failure.
        source: url::ParseError,
    },

    /// `--web-url` was not a URL.
    #[snafu(display("invalid web console URL"))]
    WebUrl {
        /// Underlying parse failure.
        source: url::ParseError,
    },

    /// The ingest endpoint could not be built from the base URL.
    #[snafu(display("could not build the ingest endpoint"))]
    IngestUrl {
        /// Underlying join failure.
        source: url::ParseError,
    },

    /// A launch recipe could not express the requested configuration.
    #[snafu(display("could not build the launch plan"))]
    LaunchPlan {
        /// Underlying recipe failure.
        source: tapes_harnesses::launch::LaunchError,
    },

    /// The envelope could not be stamped onto the outbound request.
    #[snafu(display("could not stamp the capture envelope"))]
    Envelope {
        /// Underlying header failure.
        source: tapes_harnesses::envelope::HeaderError,
    },

    /// The proxy could not take a loopback port.
    #[snafu(display("could not bind the capture proxy"))]
    Bind {
        /// Underlying IO failure.
        source: std::io::Error,
    },

    /// The harness binary could not be started.
    #[snafu(display("could not start {harness}"))]
    SpawnHarness {
        /// Which binary was being launched.
        harness: &'static str,
        /// Underlying IO failure.
        source: std::io::Error,
    },

    /// A launch config document could not be written.
    #[snafu(display("could not write the launch config at {}", path.display()))]
    ConfigFile {
        /// Where the write was attempted.
        path: PathBuf,
        /// Underlying IO failure.
        source: std::io::Error,
    },

    /// No home directory, so the harness session directories cannot be located.
    #[snafu(display("could not locate the harness session directory"))]
    NoHomeDir,

    /// Reading the inbound request body failed.
    #[snafu(display("could not read the request body"))]
    RequestBody {
        /// Underlying body error, type-erased so callers need not thread the
        /// body's generic error parameter through this type.
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// The upstream request itself failed.
    #[snafu(display("upstream request failed"))]
    Upstream {
        /// Underlying transport failure.
        source: reqwest::Error,
    },

    /// The turn could not be delivered to ingest.
    #[snafu(display("could not reach the tapes ingest server"))]
    IngestSend {
        /// Underlying transport failure.
        source: reqwest::Error,
    },

    /// Ingest refused the turn. The body is carried because a 400 names the
    /// offending envelope field and a 422 names the unprocessable turn.
    #[snafu(display("tapes ingest rejected the turn ({status}): {body}"))]
    IngestRejected {
        /// HTTP status returned.
        status: u16,
        /// Response body, verbatim.
        body: String,
    },
}

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

    /// No server was named. On the capture side there would be nowhere to send
    /// turns, and failing loudly beats running a session that captures nothing;
    /// on the read side there is nothing to query.
    #[snafu(display("no tapes server URL: pass --tapes-url or set TAPES_URL"))]
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

    // --- the transcript lane ------------------------------------------------
    /// The transcript endpoint could not be built from the base URL.
    #[snafu(display("could not build the transcript ingest endpoint"))]
    TranscriptUrl {
        /// Underlying join failure.
        source: url::ParseError,
    },

    /// A transcript file could not be read off disk.
    #[snafu(display("could not read the transcript at {}", path.display()))]
    TranscriptRead {
        /// Where the read was attempted.
        path: PathBuf,
        /// Underlying IO failure.
        source: std::io::Error,
    },

    /// The JSONL could not be framed as a JSON array. Only reachable if the
    /// crate's converter ever emitted invalid JSON, which its own tests forbid.
    #[snafu(display("could not frame the transcript records"))]
    TranscriptRecords {
        /// Underlying JSON failure.
        source: serde_json::Error,
    },

    /// The transcript could not be delivered.
    #[snafu(display("could not reach the tapes transcript endpoint"))]
    TranscriptSend {
        /// Underlying transport failure.
        source: reqwest::Error,
    },

    /// Ingest refused the transcript. A 400 names the offending envelope field.
    #[snafu(display("tapes ingest rejected the transcript ({status}): {body}"))]
    TranscriptRejected {
        /// HTTP status returned.
        status: u16,
        /// Response body, verbatim.
        body: String,
    },

    /// Some transcripts in a sweep could not be delivered. Reported as a failure
    /// because `sync` is an explicit request to move data — unlike background
    /// capture, which must degrade silently rather than take a harness down.
    #[snafu(display("{failed} of {files} transcript(s) could not be delivered"))]
    SyncIncomplete {
        /// How many files failed.
        failed: usize,
        /// How many were offered in total.
        files: usize,
    },

    // --- the read API -------------------------------------------------------
    /// An API endpoint could not be built from the base URL.
    #[snafu(display("could not build the API endpoint"))]
    ApiUrl {
        /// Underlying join failure.
        source: url::ParseError,
    },

    /// The configured base URL cannot carry a path (e.g. `mailto:`), so no
    /// route can be appended to it.
    #[snafu(display("the tapes URL cannot be used as a base for API routes"))]
    NotABase,

    /// The API request itself failed.
    #[snafu(display("could not reach the tapes API"))]
    ApiSend {
        /// Underlying transport failure.
        source: reqwest::Error,
    },

    /// The API answered with a non-success status. The body is carried because
    /// every tapes error body names the offending parameter.
    #[snafu(display("tapes API returned {status} for {endpoint}: {body}"))]
    ApiStatus {
        /// HTTP status returned.
        status: u16,
        /// Endpoint that was called.
        endpoint: String,
        /// Response body, verbatim.
        body: String,
    },

    /// The API answered with something that is not JSON.
    #[snafu(display("could not decode the tapes API response"))]
    ApiDecode {
        /// Underlying JSON failure.
        source: serde_json::Error,
    },

    /// The server's response shape changed out from under this client.
    #[snafu(display("unexpected server contract: {detail}"))]
    ApiContract {
        /// What changed.
        detail: &'static str,
    },

    /// A response could not be rendered for printing.
    #[snafu(display("could not render the response"))]
    RenderJson {
        /// Underlying JSON failure.
        source: serde_json::Error,
    },

    /// `--detail` was not an export grain the server accepts.
    #[snafu(display("invalid --detail {detail:?} (valid values: spans, traces)"))]
    InvalidExportDetail {
        /// What the user asked for.
        detail: String,
    },

    /// `--payload` was not a grain the server accepts.
    #[snafu(display("invalid --payload {payload:?} (valid values: full, preview)"))]
    InvalidPayloadDetail {
        /// What the user asked for.
        payload: String,
    },

    // --- the generated cassette surface -------------------------------------
    /// Discovery named an OpenAPI document somewhere other than on this server.
    /// Refused rather than followed: `Url::join` treats an absolute URL as a
    /// replacement, so honouring it would fetch a spec from a host the user
    /// never named.
    #[snafu(display("cassette discovery named a non-relative OpenAPI path {path:?}"))]
    CassetteSpecPath {
        /// What discovery published.
        path: String,
    },

    /// A cassette's spec described an operation with a verb that is not an HTTP
    /// method.
    #[snafu(display("cassette spec used an unusable HTTP method {method:?}"))]
    CassetteMethod {
        /// The offending verb.
        method: String,
    },

    /// A cassette noun parsed but is not on the surface. Only reachable if the
    /// surface changed between building the parser and dispatching.
    #[snafu(display("no cassette named {name:?} is served here"))]
    UnknownCassette {
        /// The noun that was invoked.
        name: String,
    },

    /// A cassette method parsed but is not on the cassette.
    #[snafu(display("cassette {cassette:?} has no method {method:?}"))]
    UnknownCassetteMethod {
        /// The cassette that was invoked.
        cassette: String,
        /// The method that was invoked.
        method: String,
    },

    /// `--body @<path>` could not be read.
    #[snafu(display("could not read the request body at {path}"))]
    BodyFile {
        /// Where the read was attempted.
        path: String,
        /// Underlying IO failure.
        source: std::io::Error,
    },

    /// `--body` was not JSON. Checked here so the failure names the quoting
    /// mistake rather than arriving as a cassette's schema error.
    #[snafu(display("--body is not valid JSON"))]
    InvalidBody {
        /// Underlying JSON failure.
        source: serde_json::Error,
    },

    // --- ported commands ----------------------------------------------------
    /// The export output file could not be created.
    #[snafu(display("could not create the export file at {}", path.display()))]
    ExportFile {
        /// Where the create was attempted.
        path: PathBuf,
        /// Underlying IO failure.
        source: std::io::Error,
    },

    /// The export body failed mid-stream.
    #[snafu(display("the export stream failed"))]
    ExportStream {
        /// Underlying transport failure.
        source: reqwest::Error,
    },

    /// The export could not be written out.
    #[snafu(display("could not write the export"))]
    ExportWrite {
        /// Underlying IO failure.
        source: std::io::Error,
    },

    /// The skill name would escape the skills directory.
    #[snafu(display(
        "invalid skill name {name:?}: a skill name is a bare file stem \
         (letters, digits, `.`, `_`, `-`), never a path"
    ))]
    SkillName {
        /// The rejected name.
        name: String,
    },

    /// The named skill could not be read.
    #[snafu(display("could not read the skill at {}", path.display()))]
    SkillRead {
        /// Where the read was attempted.
        path: PathBuf,
        /// Underlying IO failure.
        source: std::io::Error,
    },

    /// The skill could not be written to its destination.
    #[snafu(display("could not write the skill to {}", path.display()))]
    SkillWrite {
        /// Where the write was attempted.
        path: PathBuf,
        /// Underlying IO failure.
        source: std::io::Error,
    },

    /// The working directory could not be read, so a `--local` destination
    /// cannot be resolved.
    #[snafu(display("could not determine the working directory"))]
    WorkingDir {
        /// Underlying IO failure.
        source: std::io::Error,
    },
}

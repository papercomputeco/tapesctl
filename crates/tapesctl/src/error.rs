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

    /// The named harness has no launch arm here.
    ///
    /// The list is passed in rather than spelled in the message: it is derived
    /// from the arms this binary actually has, so a harness gained or lost
    /// cannot leave the error advertising the wrong set.
    #[snafu(display("unsupported harness {harness:?} (supported: {supported})"))]
    UnsupportedHarness {
        /// What the user asked for.
        harness: String,
        /// The harnesses `start` can launch, comma-separated.
        supported: String,
    },

    /// `--schema` named something no upstream schema is known by.
    #[snafu(display("invalid --schema {schema:?} (valid values: anthropic, openai)"))]
    InvalidSchema {
        /// What the user asked for.
        schema: String,
    },

    /// `--schema` was passed for a harness that speaks exactly one schema.
    ///
    /// Refused rather than ignored: a flag that silently does nothing reads,
    /// from the outside, exactly like a flag that worked — and the user would
    /// have every reason to believe the capture was routed somewhere it was not.
    #[snafu(display(
        "--schema does not apply to {harness}, which speaks {provider} only \
         (it is for a harness that redirects several providers to one endpoint, \
         such as pi)"
    ))]
    SchemaNotApplicable {
        /// The harness being launched.
        harness: &'static str,
        /// The one schema that harness speaks.
        provider: &'static str,
    },

    /// A harness whose capture needs an in-harness plugin was launched before
    /// that plugin was installed.
    ///
    /// Fatal rather than a warning, for the same reason a missing tapes URL is:
    /// the session would run to completion and capture nothing, and the user
    /// would discover it from an empty session list rather than from here.
    #[snafu(display(
        "{harness} cannot be captured until its capture plugin is installed: \
         no plugin at {}. Run `tapesctl plugin install {harness}` first.",
        path.display()
    ))]
    PluginNotInstalled {
        /// The harness being launched.
        harness: &'static str,
        /// Where the missing artifact was looked for.
        path: PathBuf,
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

    /// The HTTP client itself could not be constructed. Requests error out
    /// rather than fall back to a client with different (redirect-following)
    /// behavior.
    #[snafu(display("could not initialize the HTTP client"))]
    ClientInit,

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

    /// The vendored contract embedded in this binary did not parse. Only
    /// reachable from a build whose vendored document is corrupt — the
    /// contract tests fail before such a build ships.
    #[snafu(display("the vendored {surface} contract embedded in this build did not parse"))]
    VendoredContract {
        /// Which contract failed.
        surface: &'static str,
    },

    /// A core command named an operation the vendored contract does not have.
    /// Like [`Error::VendoredContract`], a build defect: the operation
    /// coverage tests pin every id the client uses to the vendored document.
    #[snafu(display("the vendored tapes-api contract has no operation {operation:?}"))]
    ContractOperation {
        /// The operation id that failed to resolve.
        operation: String,
    },

    /// A core command tried to send a parameter the vendored contract does not
    /// declare on that operation. Refused rather than sent: an undeclared
    /// parameter is exactly the drift the vendored contract exists to catch.
    #[snafu(display(
        "the vendored tapes-api contract does not declare parameter {parameter:?} on {operation:?}"
    ))]
    ContractParameter {
        /// The operation being called.
        operation: String,
        /// The undeclared wire name.
        parameter: String,
    },

    /// A core command had no value for a path parameter the operation
    /// requires, so no URL can be built.
    #[snafu(display(
        "operation {operation:?} requires path parameter {parameter:?} and none was supplied"
    ))]
    ContractPathParameter {
        /// The operation being called.
        operation: String,
        /// The missing path parameter.
        parameter: String,
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

    /// The skills destination resolves outside the selected tree.
    #[snafu(display(
        "refusing the skills destination {}: it resolves outside the selected \
         directory (a symlinked skills path is not followed)",
        path.display()
    ))]
    SkillDestination {
        /// The refused destination path.
        path: PathBuf,
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

    /// The named harness is not in the shared registry, so there is nothing
    /// that could be installed for it.
    ///
    /// Distinct from [`Error::UnsupportedHarness`], which is about launching:
    /// `plugin install` accepts every registered harness, including ones this
    /// binary cannot yet `start`.
    #[snafu(display("unknown harness {harness:?} (known: {known})"))]
    UnknownHarness {
        /// What the user asked for.
        harness: String,
        /// The names the registry answers to, comma-separated.
        known: String,
    },

    /// The plugin destination resolves outside the user's home.
    #[snafu(display(
        "refusing the plugin destination {}: it resolves outside the home \
         directory (a symlinked extension path is not followed)",
        path.display()
    ))]
    PluginDestination {
        /// The refused destination path.
        path: PathBuf,
    },

    /// The plugin artifact could not be written to its destination.
    #[snafu(display("could not install the plugin artifact to {}", path.display()))]
    PluginWrite {
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

    // --- skill generation ---------------------------------------------------
    /// `--type` was not a skill type the format defines.
    #[snafu(display("invalid --type {value:?} (valid types: {valid})"))]
    InvalidSkillType {
        /// What the user asked for.
        value: String,
        /// The accepted values.
        valid: String,
    },

    /// `--since` or `--until` was not a time.
    #[snafu(display("invalid {flag} {value:?} (expected RFC 3339 or YYYY-MM-DD)"))]
    InvalidSkillTime {
        /// Which flag carried it.
        flag: &'static str,
        /// What the user asked for.
        value: String,
    },

    /// Neither session ids nor a search query were given.
    #[snafu(display(
        "no session ids provided and no --search query; name a session or pass --search"
    ))]
    NoSessionsNamed,

    /// A `--search` query matched no sessions.
    #[snafu(display("no sessions found for search {query:?}"))]
    NoSearchResults {
        /// The query that matched nothing.
        query: String,
    },

    /// A session contributed no turns to extract from.
    #[snafu(display(
        "no turns in session {session}{}",
        if *filtered { " after applying --since/--until" } else { "" }
    ))]
    NoTurnsInSession {
        /// The session that came back empty.
        session: String,
        /// Whether a time window was in play, which is usually the cause.
        filtered: bool,
    },

    /// The extraction model's response was not a skill document.
    #[snafu(display("could not read a skill from the model's response"))]
    SkillJson {
        /// Underlying JSON failure.
        source: serde_json::Error,
    },

    /// The model never returned parseable JSON.
    #[snafu(display("the model did not return valid JSON in {attempts} attempts"))]
    SkillNotExtracted {
        /// How many times it was asked.
        attempts: u32,
    },

    // --- the extraction provider --------------------------------------------
    /// `--provider` names something this client cannot call.
    #[snafu(display("unsupported provider {provider:?} (supported: openai, anthropic, ollama)"))]
    LlmProvider {
        /// What the user asked for.
        provider: String,
    },

    /// No API key resolved for a provider that requires one.
    #[snafu(display("no API key for {provider}: set {env_var} or pass --api-key"))]
    LlmNoApiKey {
        /// The provider that needs a key.
        provider: &'static str,
        /// The environment variable consulted.
        env_var: &'static str,
    },

    /// The provider's base URL could not be built.
    #[snafu(display("could not build the LLM provider endpoint"))]
    LlmUrl {
        /// Underlying parse failure.
        source: url::ParseError,
    },

    /// The extraction call could not be delivered.
    #[snafu(display("could not reach the LLM provider"))]
    LlmSend {
        /// Underlying transport failure.
        source: reqwest::Error,
    },

    /// The provider answered with a non-success status. The body is carried
    /// because it is where a provider names the offending model or key.
    #[snafu(display("{provider} returned {status}: {body}"))]
    LlmStatus {
        /// Which provider answered.
        provider: &'static str,
        /// HTTP status returned.
        status: u16,
        /// Response body, verbatim.
        body: String,
    },

    /// The provider's response was not the JSON its API documents.
    #[snafu(display("could not decode the LLM provider's response"))]
    LlmDecode {
        /// Underlying JSON failure.
        source: serde_json::Error,
    },

    /// The provider returned a success status carrying an error document.
    #[snafu(display("{provider} error: {message}"))]
    LlmRefused {
        /// Which provider answered.
        provider: &'static str,
        /// The provider's message.
        message: String,
    },

    /// The provider returned a success status carrying no content.
    #[snafu(display("{provider} returned no content"))]
    LlmEmpty {
        /// Which provider answered.
        provider: &'static str,
    },

    /// The extraction call did not finish inside its deadline.
    #[snafu(display("the {provider} extraction call timed out"))]
    LlmTimeout {
        /// Which provider was being called.
        provider: &'static str,
    },
}

/// Map the shared cassette machinery's errors onto the variants this CLI
/// surfaced before the PCC-1104 extraction, one to one, so every user-facing
/// message is byte-identical to what the in-tree implementation printed.
impl From<tapes_cassette_client::Error> for Error {
    fn from(error: tapes_cassette_client::Error) -> Self {
        use tapes_cassette_client::Error as Cassette;
        match error {
            Cassette::Url { source } => Self::ApiUrl { source },
            Cassette::NotABase => Self::NotABase,
            Cassette::ClientInit => Self::ClientInit,
            Cassette::Send { source } => Self::ApiSend { source },
            Cassette::Status {
                status,
                endpoint,
                body,
            } => Self::ApiStatus {
                status,
                endpoint,
                body,
            },
            Cassette::Decode { source } => Self::ApiDecode { source },
            Cassette::Contract { detail } => Self::ApiContract { detail },
            Cassette::SpecPath { path } => Self::CassetteSpecPath { path },
            Cassette::Method { method } => Self::CassetteMethod { method },
            Cassette::UnknownCassette { name } => Self::UnknownCassette { name },
            Cassette::UnknownMethod { cassette, method } => {
                Self::UnknownCassetteMethod { cassette, method }
            }
            Cassette::BodyFile { path, source } => Self::BodyFile { path, source },
            Cassette::InvalidBody { source } => Self::InvalidBody { source },
            // `read_body`'s re-render used the shared RenderJson variant
            // before the split; keep that message for the same failure.
            Cassette::RenderBody { source } => Self::RenderJson { source },
        }
    }
}

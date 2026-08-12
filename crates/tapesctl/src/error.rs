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
    ///
    /// The message names all three sources rather than only the flag, because
    /// the answer a user most often wants is the one they only have to give
    /// once. A guessed `http://localhost:8080` is still refused: guessing is
    /// how a capture ends up pointed at whatever happens to be listening.
    #[snafu(display(
        "no tapes server URL: pass --tapes-url, set TAPES_URL, or configure a \
         default with `tapesctl config set tapes-url <url>`"
    ))]
    MissingTapesUrl,

    /// `tapesctl config` was given a key it does not have.
    ///
    /// The known keys are listed rather than described: the set is small and a
    /// typo is the overwhelmingly likely cause.
    #[snafu(display("unknown config key {key:?} (known keys: {known})"))]
    UnknownConfigKey {
        /// What the user asked for.
        key: String,
        /// The keys this build knows, comma-separated.
        known: String,
    },

    /// The configuration file exists but is not readable.
    #[snafu(display("could not read the config at {}", path.display()))]
    ConfigRead {
        /// Where the read was attempted.
        path: PathBuf,
        /// Underlying IO failure.
        source: std::io::Error,
    },

    /// The configuration file is not valid TOML, or has a value of the wrong
    /// shape.
    #[snafu(display("the config at {} is not valid: {source}", path.display()))]
    ConfigParse {
        /// The file that would not parse.
        path: PathBuf,
        /// Underlying parse failure.
        source: toml::de::Error,
    },

    /// The configuration file could not be parsed for editing.
    ///
    /// A separate failure from [`Error::ConfigParse`] because a separate parser
    /// produces it: writing edits the document in place rather than
    /// re-serializing a model, so the write path parses for structure where the
    /// read path parses for meaning. Reported rather than treated as an empty
    /// document, because a `config set` that started from "empty" would replace
    /// the file and take every key it could not read down with it.
    #[snafu(display("the config at {} could not be edited: {source}", path.display()))]
    ConfigEdit {
        /// The file that would not parse.
        path: PathBuf,
        /// Underlying parse failure.
        source: toml_edit::TomlError,
    },

    /// A configured URL names a scheme nothing here can dial.
    ///
    /// Refused at the point it is set rather than at the point it is used: the
    /// value is stored once and read by every later command, so a bad scheme
    /// accepted here fails everything afterwards, in a way that reads like the
    /// server being down rather than like the typo it is.
    #[snafu(display(
        "{key} must be an http or https URL; {scheme:?} is not a scheme this client can call"
    ))]
    ConfigUrlScheme {
        /// The configuration key being set.
        key: String,
        /// The scheme that was given.
        scheme: String,
    },

    /// The configuration file could not be written.
    #[snafu(display("could not write the config at {}", path.display()))]
    ConfigWrite {
        /// Where the write was attempted.
        path: PathBuf,
        /// Underlying IO failure.
        source: std::io::Error,
    },

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

    /// A request labelled itself with a provider this capture has no upstream
    /// for, so there is no host it could be forwarded to.
    ///
    /// Refused rather than sent to the launch's default upstream: that upstream
    /// speaks one provider's API, and a request for another arrives there as a
    /// route it has never heard of. The harness then reports a failure that
    /// looks like the model's, and the 404 body is captured and rejected by
    /// ingest as a malformed turn. Naming the provider is the whole diagnosis.
    #[snafu(display(
        "no upstream for the provider {provider:?} this request is labelled with \
         (this capture routes: {known})"
    ))]
    UnroutableProvider {
        /// The provider label carried in the request path.
        provider: String,
        /// The provider labels this capture can route, comma-separated.
        known: String,
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
        source: tapes_capture::envelope::HeaderError,
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
    ///
    /// The source is rendered inline because a transport's refusal is often
    /// the whole diagnosis — "the server answered with a redirect" is
    /// actionable, "could not reach the tapes API" alone is not — and the
    /// source is opaque, so a caller cannot recover the detail by matching on
    /// it. It is a transport error rather than a `reqwest::Error` because the
    /// client crate's seam admits transports that have never heard of HTTP.
    #[snafu(display("could not reach the tapes API: {source}"))]
    ApiSend {
        /// Underlying transport failure.
        source: tapes_client::TransportError,
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

    /// A `plugin install` flag that only shapes a hook-plugin install was
    /// passed for a harness installed by copying files.
    ///
    /// Refused rather than ignored, for the same reason `--schema` is on
    /// `start`: a flag that silently does nothing reads, from the outside,
    /// exactly like a flag that worked.
    #[snafu(display("{flag} does not apply to {harness}, whose capture plugin is a file copy"))]
    PluginFlagNotApplicable {
        /// The flag that was passed.
        flag: &'static str,
        /// The harness it was passed for.
        harness: &'static str,
    },

    // --- the Codex desktop app ----------------------------------------------
    /// A `plugin` subcommand that only applies to a harness captured through
    /// lifecycle hooks was aimed at one that is not.
    #[snafu(display(
        "{harness} is not captured through lifecycle hooks (hook harnesses: {hook_harnesses})"
    ))]
    NotAHookHarness {
        /// What the user asked for.
        harness: &'static str,
        /// The harnesses the registry declares hook-attributed.
        hook_harnesses: String,
    },

    /// `--codex-auth` named a credential mode codex does not have.
    #[snafu(display("invalid --codex-auth {value:?} (valid values: chatgpt, api-key)"))]
    InvalidCodexAuth {
        /// What the user asked for.
        value: String,
    },

    /// This binary's own path could not be resolved, so no hook command line
    /// can be written. Fatal rather than falling back to the bare name: a hook
    /// runs under the desktop app's `PATH`, which generally cannot find it.
    #[snafu(display("could not resolve this executable's path for the hook command"))]
    CurrentExe {
        /// Underlying IO failure.
        source: std::io::Error,
    },

    /// The handoff file could not be read.
    ///
    /// Every handoff failure names the installer, because re-running it is the
    /// only fix and it is non-destructive — a user should never have to reason
    /// about which of these variants they hit.
    #[snafu(display(
        "could not read the codex-app handoff at {}: run `tapesctl plugin install codex-app`",
        path.display()
    ))]
    CodexAppHandoffRead {
        /// Where the read was attempted.
        path: PathBuf,
        /// Underlying IO failure.
        source: std::io::Error,
    },

    /// The handoff file is not the document this build writes.
    #[snafu(display(
        "the codex-app handoff at {} is unreadable: run `tapesctl plugin install codex-app`",
        path.display()
    ))]
    CodexAppHandoffParse {
        /// Where the read was attempted.
        path: PathBuf,
        /// Underlying JSON failure.
        source: serde_json::Error,
    },

    /// The handoff file was written by a different version of this schema.
    #[snafu(display(
        "the codex-app handoff at {} is version {found}, not {expected}: \
         run `tapesctl plugin install codex-app`",
        path.display()
    ))]
    CodexAppHandoffVersion {
        /// Where the read was attempted.
        path: PathBuf,
        /// The version found.
        found: u32,
        /// The version this build writes.
        expected: u32,
    },

    /// The handoff file describes some other harness.
    #[snafu(display(
        "the handoff at {} configures {found:?}, not codex-app: \
         run `tapesctl plugin install codex-app`",
        path.display()
    ))]
    CodexAppHandoffHarness {
        /// Where the read was attempted.
        path: PathBuf,
        /// The harness the file names.
        found: String,
    },

    /// The handoff carries no secret, so it can authenticate nothing.
    #[snafu(display(
        "the codex-app handoff at {} carries no secret, so no lifecycle report \
         could be authenticated: run `tapesctl plugin install codex-app`",
        path.display()
    ))]
    CodexAppHandoffSecret {
        /// Where the read was attempted.
        path: PathBuf,
    },

    /// The handoff document could not be rendered. Only reachable from a
    /// serializer defect: every field is a plain string or a socket address.
    #[snafu(display("could not render the codex-app handoff for {}", path.display()))]
    CodexAppHandoffWrite {
        /// Where the write was attempted.
        path: PathBuf,
        /// Underlying JSON failure.
        source: serde_json::Error,
    },

    /// The harness's own config does not declare this installation's provider,
    /// so a capture would bind an address it never talks to.
    #[snafu(display(
        "{} declares no {provider_id:?} provider, so the Codex app is not routed \
         through tapesctl: run `tapesctl plugin install codex-app`",
        path.display()
    ))]
    CodexAppNotConfigured {
        /// The config that was checked.
        path: PathBuf,
        /// The provider that should have been there.
        provider_id: String,
    },

    /// The harness's config points at a different address than the handoff.
    ///
    /// Refused rather than warned: a capture bound to the handoff's address
    /// while the app talks to another would run perfectly and record nothing,
    /// which is the one failure a user cannot diagnose from the outside.
    #[snafu(display(
        "{} routes the Codex app to {found:?}, not {expected:?}: \
         run `tapesctl plugin install codex-app` to bring the two back together",
        path.display()
    ))]
    CodexAppConfigDrift {
        /// The config that was checked.
        path: PathBuf,
        /// What this handoff's address implies.
        expected: String,
        /// What the config actually says.
        found: String,
    },

    /// Codex's `config.toml` could not be read.
    #[snafu(display("could not read the codex config at {}", path.display()))]
    CodexConfigRead {
        /// Where the read was attempted.
        path: PathBuf,
        /// Underlying IO failure.
        source: std::io::Error,
    },

    /// An install failed, and putting `config.toml` back failed too.
    ///
    /// Distinct from the failure that started it because the machine is now in
    /// a state neither error describes: the config names a capture address
    /// whose secret was never written, so the app will dial a port nothing is
    /// serving. The original cause is recoverable by retrying; this is not
    /// self-evident from it, which is why it replaces rather than nests it.
    #[snafu(display(
        "the install failed and {} could not be put back: it now names a capture \
         address that was never activated, so run `tapesctl plugin install \
         codex-app` to complete it (the install failed because: {cause})",
        path.display(),
    ))]
    CodexAppInstallNotRolledBack {
        /// The config left naming the new address.
        path: PathBuf,
        /// What the install failed with before the rollback was attempted.
        cause: String,
    },

    /// Codex's `config.toml` could not be written.
    #[snafu(display("could not write the codex config at {}", path.display()))]
    CodexConfigWrite {
        /// Where the write was attempted.
        path: PathBuf,
        /// Underlying IO failure.
        source: std::io::Error,
    },

    /// The provider patch could not be applied to `config.toml`.
    ///
    /// Surfaced rather than worked around: the grammar refuses a document it
    /// cannot parse, and rewriting a config file whose contents were not
    /// understood is how a user loses settings.
    #[snafu(display("could not update the codex config at {}", path.display()))]
    CodexConfig {
        /// Which file was being patched.
        path: PathBuf,
        /// Underlying grammar failure.
        source: tapes_harnesses::config::codex::CodexConfigError,
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

/// Map the shared client's errors onto the variants this CLI surfaced when the
/// read surface was in-tree, one to one, so every user-facing message is
/// byte-identical to what that implementation printed.
///
/// # Why there is one of these and not two
///
/// There were two, because the sealed contract and the discovered cassette
/// surface arrived as two crates with an error type each — and the two
/// disagreed in ways nothing checked: a URL failure had two spellings, and a
/// non-success status was a rich variant on one side and absent from the
/// other. This CLI paid for that twice, in two conversions whose overlapping
/// arms had to be kept in step by hand. Upstream is one taxonomy now, so this
/// is one conversion.
///
/// The variants moved but their wording did not: a user who runs a command
/// naming an operation the contract does not have reads the same sentence as
/// before, and the tests that assert on those sentences did not have to move
/// with the code.
///
/// # Why the match ends in a wildcard
///
/// The shared enum is `#[non_exhaustive]`, and deliberately: it is the
/// vocabulary of a growing surface, and a consumer must say what it does with
/// a condition its build predates rather than fail to compile when one
/// appears. The arms below are the conditions this build knows how to phrase;
/// anything else is reported as what it is — the client layer saying something
/// this binary is older than — rather than folded into an unrelated message.
impl From<tapes_client::Error> for Error {
    fn from(error: tapes_client::Error) -> Self {
        use tapes_client::Error as Client;
        match error {
            // --- refusals: the call disagreed with the contract, nothing sent
            Client::VendoredContract { surface } => Self::VendoredContract { surface },
            Client::ContractOperation { operation } => Self::ContractOperation { operation },
            Client::ContractParameter {
                operation,
                parameter,
            } => Self::ContractParameter {
                operation,
                parameter,
            },
            Client::ContractPathParameter {
                operation,
                parameter,
            } => Self::ContractPathParameter {
                operation,
                parameter,
            },
            // --- addressing
            Client::Url { source } => Self::ApiUrl { source },
            Client::NotABase => Self::NotABase,
            // --- the wire
            Client::ClientInit => Self::ClientInit,
            Client::Transport { source } => Self::ApiSend { source },
            Client::ApiStatus {
                status,
                endpoint,
                body,
            } => Self::ApiStatus {
                status,
                endpoint,
                body,
            },
            Client::Decode { source } => Self::ApiDecode { source },
            Client::Contract { detail } => Self::ApiContract { detail },
            // --- the discovered cassette surface
            Client::SpecPath { path } => Self::CassetteSpecPath { path },
            Client::Method { method } => Self::CassetteMethod { method },
            Client::UnknownCassette { name } => Self::UnknownCassette { name },
            Client::UnknownMethod { cassette, method } => {
                Self::UnknownCassetteMethod { cassette, method }
            }
            // --- request bodies a user supplied
            Client::BodyFile { path, source } => Self::BodyFile { path, source },
            Client::InvalidBody { source } => Self::InvalidBody { source },
            // `read_body`'s re-render used this CLI's RenderJson variant
            // before the surface moved out; keep that message for the same
            // failure.
            Client::RenderBody { source } => Self::RenderJson { source },
            other => {
                tracing::warn!(error = %other, "unmapped tapes-client error");
                Self::ApiContract {
                    detail: "the shared tapes client reported a condition this build predates",
                }
            }
        }
    }
}

//! `tapesctl start <harness>` — launch a harness under a just-in-time proxy.
//!
//! The proxy is *just in time* in the literal sense: it is bound when the
//! command starts, it exists only to serve one harness process, and it dies
//! with that process. There is no daemon, no port to reserve, and no state that
//! outlives the session — the listener takes an ephemeral port, the harness is
//! pointed at it by a launch recipe, and when the harness exits the server is
//! shut down and any config the recipe asked for is removed.
//!
//! # Where each piece of knowledge lives
//!
//! Almost nothing here is tapesctl's own. Directing a harness at a proxy comes
//! from `tapes_harnesses::launch`; deciding who sent a request comes from
//! `tapes_harnesses::attribution`; the header contract comes from
//! `tapes_capture::envelope`. What is genuinely local is deployment
//! knowledge: which upstream to forward to, which ingest server to post to,
//! what this client calls its own Codex provider, and what identity to stamp.
//! That split is deliberate — it is the same split the daemon client makes, which is what
//! keeps the two capture paths producing identical rows.
//!
//! # Two ways a harness reaches the proxy
//!
//! Claude and codex are *redirected*: they have a base-URL knob, a recipe sets
//! it, and nothing has to be installed. pi and opencode are captured from the
//! inside by an extension instead — `tapes-harnesses` owns the assets,
//! [`crate::plugin`] installs them, and this module's job is only to point an
//! already-installed extension at this proxy through the crate's environment
//! contract. That split is why `start pi` refuses to launch when the extension
//! is absent instead of running an uncaptured session.
//!
//! pi is that way by necessity — it has no base-URL knob at all. opencode is
//! that way by choice: the crate does ship an `OpenCodeRecipe` that redirects it
//! through a config document, but a config document cannot name the session it
//! belongs to, and opencode publishes no PID-indexed session file for the peer
//! lookup to read. Taking the plugin road gets the redirect and the identity
//! from one artifact, and makes this arm identical to pi's.
//!
//! `start opencode` is nonetheless refused today: the arm is intact and the
//! harness is registered, but the verb is withdrawn pending rework — see
//! [`WITHDRAWN`].
//!
//! The consequence for attribution is the interesting one. A redirected harness
//! is identified from the outside, by peer PID; these two cannot be, so they
//! stamp their own `X-Tapes-*` envelope from within. For those harnesses the
//! request's own headers are the better evidence, and the proxy files the turn
//! under them rather than under its failure to recognise the peer — but only
//! once the peer socket is shown to belong to the harness this process launched
//! *and* the request echoes the per-launch nonce this process generated. An
//! envelope is a claim, and a loopback port is reachable by everything on the
//! machine; see [`tapes_capture::peer_trust`] for the ancestry
//! half of what makes the claim trustworthy, and
//! [`tapes_harnesses::plugin::GATEWAY_NONCE_ENV`] for the nonce half — which is
//! what excludes the harness's own subprocesses, descendants that the ancestry
//! walk alone would vouch for.
//!
//! # The terminal is not ours
//!
//! Unlike the daemon client, this runs in the foreground of the terminal it hands to a
//! harness TUI. Between [`spawn_harness`] and the harness's exit, this process
//! must write nothing to stdout or stderr — a log line or a status print lands
//! inside someone's half-rendered frame. Diagnostics go to a file for the whole
//! of that window (see [`crate::logging`]), and the two things worth saying are
//! said on either side of it: the log path before the launch, the session link
//! after the exit.

pub mod content_encoding;
pub mod ingest;
pub mod peek;
pub mod proxy;
pub mod tally;
pub mod turn_policy;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

use snafu::{OptionExt, ResultExt};
use tapes_capture::envelope::{
    HARNESS_ID_CLAUDE, HARNESS_ID_CODEX, HARNESS_ID_OPENCODE, HARNESS_ID_PI,
};
use tapes_harnesses::attribution::{
    AttributionConfig, AttributionState, CodexProviderFilter, claude::session as claude_session,
    codex::session as codex_session, spawn_codex_watcher, spawn_watcher,
};
use tapes_harnesses::harness as registry;
use tapes_harnesses::harness::RegistryUserAgents;
use tapes_harnesses::launch::{
    ClaudeRecipe, CodexAuth, CodexRecipe, LaunchPlan, LaunchRecipe, ProxyEndpoint,
    resolve_codex_auth,
};
use tapes_harnesses::plugin::pi::GATEWAY_LABEL_SUFFIX_ENV;
use tapes_harnesses::plugin::{
    GATEWAY_NONCE_ENV, GATEWAY_PROVIDER_ROUTES_ENV, GATEWAY_PROVIDER_ROUTES_ON, GATEWAY_SCHEMA_ENV,
    GATEWAY_URL_ENV,
};
use tracing::{info, warn};
use url::Url;

use crate::cli::StartArgs;
use crate::error::{Result, error};
use crate::logging;
use crate::transcript::client::TranscriptClient;
use crate::transcript::codex_anchors;
use crate::transcript::tailer::{self, SessionTracker};
use ingest::IngestClient;
use proxy::ProxyState;
use tally::{CaptureSnapshot, CaptureTally};

/// Header this client asks Codex to stamp so concurrent Codex processes on one
/// loopback endpoint can be told apart.
///
/// The name is deliberately tapesctl's own rather than the crate's: it is a
/// private channel between this client and its own proxy, and a shared name
/// would collide with the daemon client's when both are capturing.
pub const CODEX_MARKER_HEADER: &str = "X-Tapesctl-Codex-Attribution";

/// Stable prefix of the Codex provider id this client declares. The launched
/// provider id is this plus a per-process suffix, which is exactly the
/// exact-or-suffixed shape [`CodexProviderFilter`] matches.
pub const CODEX_PROVIDER_PREFIX: &str = "tapesctl-openai";

/// Default upstream for Claude traffic when the caller names none.
pub const DEFAULT_ANTHROPIC_UPSTREAM: &str = "https://api.anthropic.com";

/// Default upstream for Codex traffic authenticated with an API key.
pub const DEFAULT_OPENAI_UPSTREAM: &str = "https://api.openai.com";

/// Default upstream for Codex traffic authenticated with a ChatGPT-plan login.
///
/// A separate host because OpenAI honours plan OAuth tokens only on the
/// ChatGPT backend, never on `api.openai.com` — the credential decides the
/// route, not a preference.
pub const DEFAULT_CHATGPT_UPSTREAM: &str = "https://chatgpt.com/backend-api/codex";

/// Where a labelled `openai-codex` request is forwarded: the ChatGPT backend
/// root, without the `/codex` segment.
///
/// The segment is absent here because pi's plan client carries it in the path
/// it composes — a labelled request arrives as `/codex/responses`, not
/// `/responses`. Codex the harness is the opposite: it appends a bare
/// `/responses`, which is why [`DEFAULT_CHATGPT_UPSTREAM`] keeps the segment.
/// One host, two clients, and the split between base and path drawn in two
/// different places; reusing the Codex constant on this route doubles the
/// segment into `/codex/codex/responses`, which the backend answers with a
/// 404 the harness reports as a model failure.
const PI_CHATGPT_UPSTREAM: &str = "https://chatgpt.com/backend-api";

/// Every provider label this capture can route, with the upstream its traffic
/// goes to and the ingest `provider` its turns file under.
///
/// The labels are the harness's spellings — what the pi extension calls its
/// providers — and they are not all schema names: `openai-codex` is pi's
/// ChatGPT-plan provider, which speaks the OpenAI schema to a different host.
/// That is exactly why the table has two columns rather than one. Collapsing
/// them would either route plan traffic to `api.openai.com`, which rejects the
/// credential, or file it under a provider ingest has no reducer for.
///
/// The ChatGPT backend appears here for the same reason it does in Codex's
/// default: the credential decides the host. It appears as
/// [`PI_CHATGPT_UPSTREAM`] rather than [`DEFAULT_CHATGPT_UPSTREAM`] because
/// the routed client also decides where base ends and path begins.
const PROVIDER_ROUTES: &[(&str, &str, &str)] = &[
    ("anthropic", DEFAULT_ANTHROPIC_UPSTREAM, "anthropic"),
    ("openai", DEFAULT_OPENAI_UPSTREAM, "openai"),
    ("openai-codex", PI_CHATGPT_UPSTREAM, "openai"),
];

/// Appended to the pi status entry when this capture routes every provider.
///
/// The entry reads `<label>:<schema><suffix>`, and the schema in it is the one
/// this launch fronts *by default* — which, once every provider has a route, is
/// no longer the only thing being captured. Without this the status would name
/// one provider for a session carrying three, and a user debugging a missing
/// turn would rule out the right explanation on the strength of it.
const PROVIDER_ROUTES_LABEL_SUFFIX: &str = "+all";

/// Where one labelled provider's traffic goes.
#[derive(Debug, Clone)]
pub struct ProviderRoute {
    /// The upstream this provider's requests are forwarded to.
    pub upstream: Url,
    /// The ingest `provider` its turns file under.
    pub provider: &'static str,
}

/// The provider labels one capture can route.
///
/// A resolved value carried by the proxy rather than a lookup into
/// [`PROVIDER_ROUTES`], because the upstreams are the thing a test has to be
/// able to substitute: a routing rule that can only be exercised against
/// `api.anthropic.com` is a routing rule nothing checks.
#[derive(Debug, Clone)]
pub struct ProviderRoutes {
    routes: Vec<(String, ProviderRoute)>,
}

impl ProviderRoutes {
    /// The routes a real launch uses — [`PROVIDER_ROUTES`], parsed.
    pub fn defaults() -> Result<Self> {
        Self::from_pairs(
            PROVIDER_ROUTES
                .iter()
                .map(|(label, upstream, provider)| (*label, *upstream, *provider)),
        )
    }

    /// Build a route set from `(label, upstream, ingest provider)` triples.
    pub fn from_pairs<'a>(
        pairs: impl IntoIterator<Item = (&'a str, &'a str, &'static str)>,
    ) -> Result<Self> {
        let routes = pairs
            .into_iter()
            .map(|(label, upstream, provider)| {
                Ok((
                    label.to_owned(),
                    ProviderRoute {
                        upstream: Url::parse(upstream).context(error::UpstreamUrlSnafu)?,
                        provider,
                    },
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self { routes })
    }

    /// Resolve a provider label.
    ///
    /// `None` for a label no route names, which the proxy refuses rather than
    /// guesses at — see [`crate::error::Error::UnroutableProvider`].
    #[must_use]
    pub fn resolve(&self, label: &str) -> Option<&ProviderRoute> {
        self.routes
            .iter()
            .find(|(name, _)| name == label)
            .map(|(_, route)| route)
    }

    /// The labels this set resolves, comma-separated, for error messages.
    #[must_use]
    pub fn known(&self) -> String {
        self.routes
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Which upstream API schema the proxy fronts.
///
/// One proxy forwards to one upstream, so a harness that speaks several schemas
/// has to be told which one this capture is for. The spellings are the crate's:
/// they are simultaneously the values the pi extension recognises (its
/// `SCHEMA_PROVIDERS`) and the `provider` names ingest keys its reducer on. That
/// those two sets coincide is a contract, not a coincidence — a schema whose
/// extension spelling differed from its ingest spelling would capture turns the
/// server could not reduce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamSchema {
    /// The Anthropic Messages API.
    Anthropic,
    /// The OpenAI API.
    OpenAi,
}

impl UpstreamSchema {
    /// Resolve a user-typed schema name.
    pub fn parse(name: &str) -> Result<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "anthropic" => Ok(Self::Anthropic),
            "openai" => Ok(Self::OpenAi),
            other => error::InvalidSchemaSnafu {
                schema: other.to_owned(),
            }
            .fail(),
        }
    }

    /// The wire name — both the ingest provider and the extension's schema hint.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
        }
    }

    /// Default upstream for traffic in this schema.
    #[must_use]
    pub fn default_upstream(self) -> &'static str {
        match self {
            Self::Anthropic => DEFAULT_ANTHROPIC_UPSTREAM,
            Self::OpenAi => DEFAULT_OPENAI_UPSTREAM,
        }
    }
}

/// The schema a pi capture fronts when the user names none.
///
/// Anthropic because that is the provider pi ships selected, so the default
/// captures the default session.
pub const DEFAULT_PI_SCHEMA: UpstreamSchema = UpstreamSchema::Anthropic;

/// The schema an opencode capture fronts when the user names none.
///
/// Anthropic for the same reason as pi's, and a separate constant rather than a
/// shared one because it is a separate judgement about a separate harness: which
/// provider opencode ships selected can move without pi's having moved.
pub const DEFAULT_OPENCODE_SCHEMA: UpstreamSchema = UpstreamSchema::Anthropic;

/// Which harness is being launched, and everything that differs between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Harness {
    /// Claude Code, over the Anthropic Messages API.
    Claude,
    /// Codex, over the OpenAI Responses API.
    Codex,
    /// opencode, captured from inside by the crate's gateway plugin.
    OpenCode,
    /// pi, captured from inside by the crate's gateway extension.
    Pi,
}

/// Every harness `start` will launch, in the order they should be offered.
///
/// The registry is deliberately the wider set: it lists every harness the shared
/// crate knows, including ones no arm here launches (the Codex desktop app,
/// which is configured rather than launched at all). This is the narrower claim
/// — what this binary can actually do — and the error message for an unsupported
/// name is derived from it so the two cannot drift.
///
/// It is also what [`Harness::parse`] gates on, so this list is the surface
/// itself rather than a description of it: a harness [`Harness`] has an arm for
/// but that is absent here cannot be launched at all.
pub const SUPPORTED: &[Harness] = &[Harness::Claude, Harness::Codex, Harness::Pi];

/// Harnesses this binary has a working arm for and deliberately does not offer.
///
/// A withdrawal, not a deletion. opencode keeps everything it had — its registry
/// entry, its capture plugin, its arms below, `tapesctl plugin install
/// opencode` — and reinstating the verb is moving one entry back into
/// [`SUPPORTED`]. What is withdrawn is the first-class `start` verb, because on
/// the OAuth path the plugin captures nothing, and a `start` that
/// runs an agent while silently recording none of it is worse than a `start`
/// that refuses.
///
/// The marker lives here rather than in the registry because the registry's
/// launchability axis states a *capability* — whether a launch can be planned
/// at all — and opencode's can. This is a product judgement about one client's
/// surface, which is the kind of thing that differs between clients: the
/// sibling CLI carries its own list, by the same name and for the same reason.
///
/// Naming the withdrawn set explicitly, rather than just dropping the entry
/// above, is what keeps the arms below from reading as dead code somebody
/// tidies away — they are the work this is pending a rework of.
pub const WITHDRAWN: &[Harness] = &[Harness::OpenCode];

impl Harness {
    /// Resolve a user-typed harness name.
    ///
    /// Resolution goes through the shared registry rather than a local match, so
    /// the names `start` accepts are the names every other command accepts —
    /// aliases and casing included. A harness the registry knows but this binary
    /// has no arm for fails here, with the same message as a name nobody knows:
    /// from the user's side both mean "not something `tapesctl start` launches".
    ///
    /// A [`WITHDRAWN`] harness fails identically, and that is the point: it has
    /// an arm, and the refusal is deliberate rather than a gap. Gating on
    /// [`SUPPORTED`] rather than on the match below is what makes the withdrawal
    /// one line of data instead of a special case threaded through the arms.
    pub fn parse(name: &str) -> Result<Self> {
        let resolved = registry::find(name)
            .and_then(|harness| match harness.id() {
                HARNESS_ID_CLAUDE => Some(Self::Claude),
                HARNESS_ID_CODEX => Some(Self::Codex),
                HARNESS_ID_OPENCODE => Some(Self::OpenCode),
                HARNESS_ID_PI => Some(Self::Pi),
                _ => None,
            })
            .filter(|harness| SUPPORTED.contains(harness));
        resolved.context(error::UnsupportedHarnessSnafu {
            harness: name.trim().to_owned(),
            supported: supported_names(),
        })
    }

    /// The canonical harness id — the registry's, and the envelope's.
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Self::Claude => HARNESS_ID_CLAUDE,
            Self::Codex => HARNESS_ID_CODEX,
            Self::OpenCode => HARNESS_ID_OPENCODE,
            Self::Pi => HARNESS_ID_PI,
        }
    }

    /// The binary to execute.
    #[must_use]
    pub fn program(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::OpenCode => "opencode",
            Self::Pi => "pi",
        }
    }

    /// The schema this capture fronts when the user names none, for a harness
    /// that speaks more than one — and `None` for a harness whose schema follows
    /// from the harness itself.
    ///
    /// That `None` is load-bearing twice over: it is what makes `--schema`
    /// refusable for Claude and Codex, and it is what
    /// [`StartConfig::schema`] carries as "this question does not arise".
    #[must_use]
    pub fn default_schema(self) -> Option<UpstreamSchema> {
        match self {
            Self::Claude | Self::Codex => None,
            Self::OpenCode => Some(DEFAULT_OPENCODE_SCHEMA),
            Self::Pi => Some(DEFAULT_PI_SCHEMA),
        }
    }

    /// The ingest `provider` family this harness's traffic is in. Ingest keys
    /// its server-side reducer on this, so it must name the wire format of the
    /// bytes actually captured — not the vendor of the harness.
    ///
    /// pi and opencode speak whichever schema this capture fronts, so they are
    /// the harnesses whose provider is not fixed by the harness.
    #[must_use]
    pub fn provider(self, schema: Option<UpstreamSchema>) -> &'static str {
        match self {
            Self::Claude => "anthropic",
            Self::Codex => "openai",
            Self::OpenCode => schema.unwrap_or(DEFAULT_OPENCODE_SCHEMA).as_str(),
            Self::Pi => schema.unwrap_or(DEFAULT_PI_SCHEMA).as_str(),
        }
    }

    /// Default upstream when none is supplied.
    ///
    /// Codex's default depends on how it will authenticate, because the two
    /// credential kinds are accepted by different hosts; pi's and opencode's
    /// depend on the schema being captured.
    #[must_use]
    pub fn default_upstream(
        self,
        auth: Option<CodexAuth>,
        schema: Option<UpstreamSchema>,
    ) -> &'static str {
        match (self, auth) {
            (Self::Claude, _) => DEFAULT_ANTHROPIC_UPSTREAM,
            (Self::Codex, Some(CodexAuth::ChatGpt)) => DEFAULT_CHATGPT_UPSTREAM,
            (Self::Codex, _) => DEFAULT_OPENAI_UPSTREAM,
            (Self::OpenCode, _) => schema.unwrap_or(DEFAULT_OPENCODE_SCHEMA).default_upstream(),
            (Self::Pi, _) => schema.unwrap_or(DEFAULT_PI_SCHEMA).default_upstream(),
        }
    }

    /// Whether requests take the Codex attribution lane.
    #[must_use]
    pub fn is_codex(self) -> bool {
        matches!(self, Self::Codex)
    }

    /// Whether this harness's capture extension points *several* providers at
    /// the proxy at once, rather than redirecting the one being captured.
    ///
    /// pi is the case: its extension registers `anthropic`, `openai`, and
    /// `openai-codex` together, because it has to register before a model is
    /// chosen. A proxy behind one upstream can serve one of them, so the other
    /// two arrive as requests it must either route elsewhere or refuse — which
    /// is what [`PROVIDER_ROUTES`] and the label on the request are for.
    ///
    /// Not the same question as [`Self::default_schema`]. That asks which
    /// schema a launch fronts *by default*; this asks whether one upstream can
    /// be the whole answer at all.
    #[must_use]
    pub fn registers_several_providers(self) -> bool {
        matches!(self, Self::Pi)
    }

    /// Whether this harness stamps its own `X-Tapes-*` envelope from inside.
    ///
    /// Read from the registry rather than matched on here: which harnesses
    /// attribute themselves is harness knowledge, and the crate states it. A
    /// harness that gains an in-harness extension therefore takes this lane
    /// without this file changing.
    #[must_use]
    pub fn is_self_attributing(self) -> bool {
        registry::find(self.id()).is_some_and(|harness| {
            harness.attribution() == registry::AttributionStrategy::SelfAttributing
        })
    }

    /// The plugin artifacts that must already be installed for this harness's
    /// traffic to be captured at all. Empty for a harness captured by redirect.
    #[must_use]
    pub fn required_plugin_artifacts(self) -> &'static [tapes_harnesses::plugin::PluginArtifact] {
        registry::find(self.id()).map_or(&[], |harness| harness.plugin_artifacts())
    }

    /// Build the endpoint the harness should be pointed at.
    ///
    /// The path suffix is deployment knowledge, so it is decided here rather
    /// than in the recipe. Codex appends `/responses` to whatever it is given,
    /// and the two auth modes reach different paths: OpenAI's responses route
    /// is `/v1/responses`, so an API-key endpoint ends at a `/v1` segment,
    /// while the ChatGPT backend has no `/v1` component and its endpoint ends
    /// at the backend segment. Claude appends `/v1/messages` itself and needs
    /// no suffix at all, and pi's providers each append their own full path.
    ///
    /// opencode is bare for a subtler reason than pi's. Its AI SDK adapters do
    /// *not* append a version component — they append only `/messages` or
    /// `/responses` — so something has to supply the `/v1`. That something is
    /// the plugin, not this function: it is knowledge about opencode's HTTP
    /// client rather than about this deployment's routes, and putting it here
    /// would make [`tapes_harnesses::plugin::GATEWAY_URL_ENV`] mean a different
    /// thing for opencode than it means for pi. The variable stays "the proxy's
    /// origin" for every harness that reads it.
    #[must_use]
    pub fn endpoint_for(self, addr: SocketAddr, auth: Option<CodexAuth>) -> ProxyEndpoint {
        match (self, auth) {
            (Self::Claude | Self::OpenCode | Self::Pi, _)
            | (Self::Codex, Some(CodexAuth::ChatGpt)) => {
                ProxyEndpoint::new(&format!("http://{addr}"))
            }
            (Self::Codex, _) => ProxyEndpoint::new(&format!("http://{addr}/v1")),
        }
    }

    /// Build the launch plan that points this harness at `endpoint`.
    ///
    /// `nonce` is the per-launch capture secret. Only a self-attributing
    /// harness's plan carries it — a redirected harness never names its own
    /// session, so it has nothing to prove — but it is a parameter rather than
    /// generated inside so the same value ends up in the launched environment
    /// and in the proxy that validates the echo.
    pub fn plan(
        self,
        endpoint: ProxyEndpoint,
        provider_id: &str,
        auth: Option<CodexAuth>,
        schema: Option<UpstreamSchema>,
        nonce: &str,
        provider_routes: bool,
    ) -> Result<LaunchPlan> {
        match self {
            Self::Claude => ClaudeRecipe::new(endpoint).plan(),
            Self::Codex => {
                CodexRecipe::new(endpoint, auth.unwrap_or(CodexAuth::ChatGpt), provider_id)
                    .with_display_name("tapesctl capture")
                    .with_attribution_header(CODEX_MARKER_HEADER)
                    .plan()
            }
            // No recipe is used for either of these: what points them at the
            // proxy is an installed extension reading the environment. See
            // [`gateway_plan`].
            Self::OpenCode => {
                return Ok(gateway_plan(
                    &endpoint,
                    schema.unwrap_or(DEFAULT_OPENCODE_SCHEMA),
                    nonce,
                    provider_routes,
                ));
            }
            Self::Pi => {
                return Ok(gateway_plan(
                    &endpoint,
                    schema.unwrap_or(DEFAULT_PI_SCHEMA),
                    nonce,
                    provider_routes,
                ));
            }
        }
        .context(error::LaunchPlanSnafu)
    }

    /// How Codex will authenticate, resolved from the environment the harness
    /// will inherit. `None` for harnesses where the question does not arise.
    ///
    /// Delegated to the shared crate rather than re-tested here: "a blank
    /// `OPENAI_API_KEY` means absent, not empty" is exactly the kind of detail
    /// two clients would otherwise get subtly different.
    #[must_use]
    pub fn codex_auth(self) -> Option<CodexAuth> {
        if !self.is_codex() {
            return None;
        }
        let env: std::collections::HashMap<std::ffi::OsString, std::ffi::OsString> =
            std::env::vars_os().collect();
        Some(resolve_codex_auth(&env))
    }
}

/// The harnesses `start` can launch, comma-separated, for error messages.
fn supported_names() -> String {
    SUPPORTED
        .iter()
        .map(|harness| harness.id())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The launch plan for a harness captured by an installed extension: no
/// arguments, three environment variables.
///
/// Shared by pi and opencode, and identical for both — which is the point of the
/// crate's environment contract. There is no argv here at all: the extension is
/// already loaded for every session of that harness on this machine, because
/// `tapesctl plugin install <harness>` wrote it into the harness's own
/// auto-discovery directory. What a launch adds is only the environment that
/// wakes it up.
///
/// For pi this is also what [`registry::LaunchSupport::ConsumerOwned`] means in
/// practice: the crate ships the asset but no recipe, because the two consumers
/// point pi at it differently — one materialises an ephemeral copy and passes
/// `--extension`, while tapesctl relies on the installed one. opencode does have
/// a crate recipe ([`tapes_harnesses::launch::OpenCodeRecipe`]) and this arm
/// deliberately does not use it: the recipe relocates `XDG_CONFIG_HOME` for the
/// whole process tree to place one config file, and it cannot attribute the
/// session it redirects. The plugin does both jobs and touches nothing outside
/// the harness.
///
/// Both names come from [`tapes_harnesses::plugin`], which is also where the
/// asset reads them — the constant and the TypeScript literal are two spellings
/// of one contract, and the crate's tests pin them against each other. Spelling
/// either one here would be the drift the shared crate exists to prevent.
///
/// The schema variable is a hint, not a switch: the extension redirects
/// regardless, and uses this only to warn a user who picks a model the proxy is
/// not fronting. It is still always set, because a warning that never fires is
/// indistinguishable from one that cannot.
///
/// The nonce variable is the opposite of a hint: the extension echoes it in
/// [`tapes_harnesses::plugin::GATEWAY_NONCE_HEADER`] on every captured request,
/// and the proxy refuses any inbound envelope that does not carry the echo.
/// This is what turns "is the peer below the launched PID" — which every
/// subprocess the harness runs satisfies — into "does the peer hold this
/// launch's secret". The crate's extension reads the variable once at load and
/// deletes it from its process environment before any tool can run, so
/// subprocesses the harness later spawns do not inherit it; the crate pins
/// that delete in the asset. The residual exposure is exactly two channels:
/// a same-UID process reading the harness's *original* environment via
/// `/proc/<pid>/environ` on Linux (a snapshot taken at `exec`, unaffected by
/// the deletion), and anything the harness itself passes along explicitly.
/// What the nonce guarantees unconditionally is that a *complete* forgery
/// needs the secret, not just two headers and a loopback port.
/// The routes variable is a switch, and the only one here that changes what the
/// extension puts on the wire. Set, each captured provider registers under its
/// own label and this proxy routes by it; unset, every provider registers at
/// the endpoint directly and the schema above is the only thing that can be
/// captured. It is set only when this launch can actually honour a label — see
/// [`StartConfig::provider_routes`] — because an extension labelling requests a
/// proxy would refuse is strictly worse than one that labels nothing.
fn gateway_plan(
    endpoint: &ProxyEndpoint,
    schema: UpstreamSchema,
    nonce: &str,
    provider_routes: bool,
) -> LaunchPlan {
    let mut env = vec![
        (GATEWAY_URL_ENV.to_owned(), endpoint.as_str().to_owned()),
        (GATEWAY_SCHEMA_ENV.to_owned(), schema.as_str().to_owned()),
        (GATEWAY_NONCE_ENV.to_owned(), nonce.to_owned()),
    ];
    if provider_routes {
        env.push((
            GATEWAY_PROVIDER_ROUTES_ENV.to_owned(),
            GATEWAY_PROVIDER_ROUTES_ON.to_owned(),
        ));
        // The status entry would otherwise read `tapesctl:anthropic`, which
        // names one schema for a capture that is carrying all of them.
        env.push((
            GATEWAY_LABEL_SUFFIX_ENV.to_owned(),
            PROVIDER_ROUTES_LABEL_SUFFIX.to_owned(),
        ));
    }
    LaunchPlan {
        args: Vec::new(),
        env,
        config_files: Vec::new(),
    }
}

/// Refuse to launch a harness whose capture plugin is not installed.
///
/// The check is against the artifacts the *registry* declares, not a path
/// spelled here, so a harness that gains an artifact is covered without this
/// function changing — and so this and `plugin install` can only ever disagree
/// if the crate contradicts itself.
///
/// `home` is a parameter for the same reason it is one in [`crate::plugin`]:
/// the behaviour worth testing is what happens for a home that does and does not
/// have the file, and neither is safe to assert against a developer's own.
fn ensure_plugin_installed(harness: Harness, home: &Path) -> Result<()> {
    for artifact in harness.required_plugin_artifacts() {
        let path = artifact.install_path(home);
        // `exists()` follows symlinks, which is the right question here: a
        // symlinked extension that resolves to a real file is one pi will load.
        // Whether writing *through* such a link is safe is the installer's
        // problem, and it refuses; this only asks whether pi has something to
        // load.
        snafu::ensure!(
            path.exists(),
            error::PluginNotInstalledSnafu {
                harness: harness.id(),
                path,
            }
        );
    }
    Ok(())
}

/// Resolved configuration for one `tapesctl start` invocation.
#[derive(Debug, Clone)]
pub struct StartConfig {
    /// Harness to launch.
    pub harness: Harness,
    /// How Codex will authenticate, when the harness is Codex.
    pub codex_auth: Option<CodexAuth>,
    /// Which upstream schema this capture fronts, for a harness that speaks
    /// more than one. `None` for harnesses whose schema follows from the
    /// harness itself.
    pub schema: Option<UpstreamSchema>,
    /// Arguments passed through to the harness verbatim.
    pub harness_args: Vec<String>,
    /// Where forwarded LLM traffic goes.
    pub upstream: Url,
    /// Whether the launched extension labels each registration with its
    /// provider, so the proxy routes each one to its own upstream.
    ///
    /// On for a harness that registers several providers at once — and only
    /// when the upstream was left to default. An explicit `--upstream` names
    /// one host and means it: a user pointing a capture at a local gateway is
    /// asking for everything to go *there*, and a per-provider table would send
    /// two thirds of the session somewhere they did not ask for.
    pub provider_routes: bool,
    /// Base URL of the tapes ingest server.
    pub ingest_url: Url,
    /// Base URL of the web console, for the printed session link.
    pub web_url: Option<Url>,
    /// Org id stamped on captured turns.
    pub org_id: String,
    /// Acting subject stamped on captured turns.
    pub auth_subject: String,
    /// Whether to tail this session's transcripts alongside the wire lane.
    pub transcripts: bool,
}

impl StartConfig {
    /// Resolve CLI arguments and the environment into a config.
    pub fn resolve(args: StartArgs) -> Result<Self> {
        let harness = Harness::parse(&args.harness)?;
        let codex_auth = harness.codex_auth();
        let schema = resolve_schema(harness, args.schema.as_deref())?;
        let ingest_url = args
            .ingest_url
            .as_deref()
            .context(error::MissingTapesUrlSnafu)?;
        let ingest_url = Url::parse(ingest_url).context(error::TapesUrlSnafu)?;
        let upstream = match args.upstream.as_deref() {
            Some(upstream) => upstream,
            None => harness.default_upstream(codex_auth, schema),
        };
        let web_url = match args.web_url.as_deref() {
            Some(raw) => Some(Url::parse(raw).context(error::WebUrlSnafu)?),
            None => None,
        };

        Ok(Self {
            harness,
            codex_auth,
            schema,
            harness_args: args.harness_args,
            provider_routes: harness.registers_several_providers() && args.upstream.is_none(),
            upstream: Url::parse(upstream).context(error::UpstreamUrlSnafu)?,
            ingest_url,
            web_url,
            org_id: args.org_id.unwrap_or_default(),
            // A standalone client has no gateway to stamp validated claims, so
            // it names the local user. Nothing parses the prefix — it is an
            // opaque attribution string in both worlds.
            auth_subject: args
                .auth_subject
                .unwrap_or_else(|| format!("local:{}", local_username())),
            // Transcripts are the only source of a session's fork skeleton, so
            // the lane is on unless the user explicitly says another client is
            // already tailing the same tree.
            transcripts: !args.no_transcripts,
        })
    }
}

/// Resolve `--schema` against the harness being launched.
///
/// A harness that speaks exactly one schema gets `None`, and naming one anyway
/// is refused: the flag would otherwise appear to route a capture it has no
/// power over.
fn resolve_schema(harness: Harness, schema: Option<&str>) -> Result<Option<UpstreamSchema>> {
    let Some(schema) = schema else {
        return Ok(harness.default_schema());
    };
    // A harness with no default schema is one that speaks exactly one, so
    // `--schema` has nothing to choose between and is refused rather than
    // silently ignored.
    snafu::ensure!(
        harness.default_schema().is_some(),
        error::SchemaNotApplicableSnafu {
            harness: harness.id(),
            provider: harness.provider(None),
        }
    );
    Ok(Some(UpstreamSchema::parse(schema)?))
}

/// The local OS username, or `unknown`. Shared with `tapesctl sync`, which
/// stamps the same default subject.
#[must_use]
pub fn local_username() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".to_owned())
}

/// Run one capture session: bind, launch, forward, and exit with the harness.
pub async fn run(args: StartArgs) -> Result<()> {
    let config = StartConfig::resolve(args)?;

    // Before anything is bound or spawned: a harness whose capture depends on an
    // installed extension cannot be captured without it, and the session would
    // otherwise run to completion and record nothing.
    if !config.harness.required_plugin_artifacts().is_empty() {
        let home = dirs::home_dir().context(error::NoHomeDirSnafu)?;
        ensure_plugin_installed(config.harness, &home)?;
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .context(error::BindSnafu)?;
    let addr = listener.local_addr().context(error::BindSnafu)?;

    // A per-process provider id: it is echoed back in the marker header, which
    // is how the attribution pipeline tells two concurrent Codex processes
    // apart on one loopback endpoint.
    let provider_id = format!("{CODEX_PROVIDER_PREFIX}-{}", uuid::Uuid::new_v4());
    // The per-launch capture secret. Generated for every launch and handed only
    // to a self-attributing harness's environment; the proxy refuses any
    // inbound envelope whose request does not echo it. A v4 UUID is 122 bits
    // from the OS RNG — a secret, not a tag, so it must never be logged and
    // never leave this process except through the launched environment.
    let gateway_nonce = uuid::Uuid::new_v4().to_string();
    let endpoint = config.harness.endpoint_for(addr, config.codex_auth);
    let plan = config.harness.plan(
        endpoint.clone(),
        &provider_id,
        config.codex_auth,
        config.schema,
        &gateway_nonce,
        config.provider_routes,
    )?;

    // Published once and read by two consumers: the attribution pipeline, and
    // the Codex anchor lane. A second watcher would be a second answer to
    // "which rollouts exist right now", and the two could disagree about a
    // file that appeared mid-tick.
    let codex_snapshot = spawn_codex_watcher(codex_sessions_dir()?);
    let attribution = AttributionState::new(
        spawn_watcher(claude_sessions_dir()?),
        codex_snapshot.clone(),
    );

    // Written once, immediately after the harness is spawned, and read on every
    // request that arrives carrying an envelope. It starts at the "no harness
    // yet" sentinel so a request that beats the spawn is refused rather than
    // trusted — the listener is open from `bind`, which is strictly earlier.
    let launched_pid = Arc::new(AtomicI32::new(NO_LAUNCHED_PID));

    let tracker = SessionTracker::new();
    // Held here as well as in the state: the capture tasks write it, and this
    // function drains and reads it after shutdown to decide what the exit
    // summary may say.
    let tally = Arc::new(CaptureTally::new());
    let state = ProxyState {
        tally: Arc::clone(&tally),
        upstream: config.upstream.clone(),
        ingest: IngestClient::new(&config.ingest_url)?,
        transcript_tracker: tracker.clone(),
        attribution: Arc::new(attribution),
        attribution_config: Arc::new(AttributionConfig::new(
            CodexProviderFilter::new(CODEX_PROVIDER_PREFIX),
            // Which harness a `User-Agent` names is registry knowledge, so the
            // registry answers it. A local table here would be a second set of
            // rules for the same question.
            RegistryUserAgents::default(),
        )),
        provider: config.harness.provider(config.schema),
        provider_routes: config
            .provider_routes
            .then(ProviderRoutes::defaults)
            .transpose()?
            .map(Arc::new),
        codex_marker_header: Arc::new(CODEX_MARKER_HEADER.to_ascii_lowercase()),
        codex_lane: config.harness.is_codex(),
        self_attributing: config.harness.is_self_attributing(),
        launched_pid: Arc::clone(&launched_pid),
        gateway_nonce: Arc::new(gateway_nonce),
        org_id: Arc::new(config.org_id.clone()),
        auth_subject: Arc::new(config.auth_subject.clone()),
        // Every harness `start` launches names its session either through the
        // pipeline or through its own envelope. The desktop registry is filled
        // by a route this command does not serve, so leaving it empty here
        // would only add a lane that always misses.
        desktop_sessions: None,
    };

    let app = axum::Router::new()
        .fallback(proxy::forward_handler)
        .with_state(state);
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let served = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async {
            let _ = shutdown_rx.await;
        });
        if let Err(err) = served.await {
            warn!(error = %err, "proxy server stopped");
        }
    });

    info!(
        harness = config.harness.program(),
        proxy = %addr,
        upstream = %config.upstream,
        ingest = %config.ingest_url,
        "capture proxy listening",
    );

    let tailer = spawn_tailer(&config, tracker)?;
    let anchors = spawn_codex_anchor_lane(&config, codex_snapshot)?;

    let written = materialise_config_files(&plan)?;

    // The last thing this process writes to the terminal until the harness
    // gives it back.
    announce_capture();

    let status = spawn_harness(&config, &plan, &launched_pid).await;

    // Dying with the session: stop accepting, then clean up whatever the recipe
    // asked to have on disk. Cleanup is the consumer's job precisely because
    // recipes are pure.
    let _ = shutdown_tx.send(());
    let _ = server.await;
    remove_config_files(&written);

    // The tailer is *awaited*, not aborted. Its shutdown pass is the
    // `PushReason::Exit` push that delivers the completed transcript set —
    // including the subagent files that carry the fork skeleton — and it can
    // only run after the harness has finished writing them. Aborting here would
    // drop exactly the data the transcript lane exists to capture.
    if let Some((shutdown, handle)) = tailer {
        let _ = shutdown.send(());
        if let Err(err) = handle.await {
            warn!(error = %err, "transcript tailer did not finish cleanly");
        }
    }

    // Same contract for Codex, and the same reason: a subagent spawned in the
    // last seconds of a session has its anchor pushed only by the final pass.
    if let Some((shutdown, handle)) = anchors {
        let _ = shutdown.send(());
        if let Err(err) = handle.await {
            warn!(error = %err, "codex anchor lane did not finish cleanly");
        }
    }

    // A capture finishes on a task detached from the exchange that produced it,
    // so the server being shut down says nothing about whether the last turn
    // has reached ingest. A harness that exits the moment its final response
    // lands leaves that turn's POST in flight, and a summary taken here would
    // be short by it — in a one-turn session, short by all of it, printing "no
    // turns were captured" over a turn that was about to land.
    //
    // Bounded, and deliberately not a join: ingest sets no request timeout, so
    // waiting for a wedged server would hold a terminal the harness has already
    // handed back. What has not landed by the deadline is reported as still in
    // flight instead.
    tally.drain(tally::CAPTURE_DRAIN_DEADLINE).await;

    // The terminal is the caller's again, so the session can finally be named.
    // The id only becomes known once an attributed turn is *accepted*, which is
    // mid-session — the one moment printing is forbidden — so it is read back
    // off the tally here rather than announced when it resolved.
    print_exit_summary(config.web_url.as_ref(), &tally.snapshot());

    let status = status?;
    if !status.success() {
        warn!(code = ?status.code(), "harness exited with a non-zero status");
    }
    Ok(())
}

/// Start the transcript tailer for this session, when the harness has one.
///
/// `None` covers the two cases where the lane cannot or should not run: the user
/// opted out, or the harness is not Claude — no other harness writes into the
/// Claude project tree the shared crate's discovery walks (Codex keeps rollout
/// files of its own, carried by [`spawn_codex_anchor_lane`]; opencode keeps
/// sessions in a SQLite database, which is not a tree a transcript sweep can
/// walk at all). Returning `None` rather than failing is deliberate: those are
/// still good wire captures, and refusing to start one over a lane that does
/// not apply would be a regression against PR 5.
fn spawn_tailer(
    config: &StartConfig,
    tracker: SessionTracker,
) -> Result<
    Option<(
        tokio::sync::oneshot::Sender<()>,
        tokio::task::JoinHandle<()>,
    )>,
> {
    if !config.transcripts || config.harness != Harness::Claude {
        return Ok(None);
    }
    let projects_root = tailer::default_projects_root().context(error::NoHomeDirSnafu)?;
    let tailer_config = tailer::TailerConfig::new(
        projects_root,
        claude_sessions_dir()?,
        config.auth_subject.clone(),
    );
    let client = TranscriptClient::new(&config.ingest_url)?;
    Ok(Some(tailer::spawn(client, tracker, tailer_config)))
}

/// Start the Codex spawn-anchor lane, which is Codex's whole transcript lane.
///
/// Codex writes no per-session transcript tree, so [`spawn_tailer`] has nothing
/// to walk for it. What it does write is one `sub_agent_activity` record per
/// spawn, in the parent thread's rollout — the only place the
/// (spawn call_id ↔ child thread id) join exists, since `spawn_agent`'s
/// arguments are encrypted on the wire. Without this lane a `tapesctl`-captured
/// Codex session reconstructs into a flatter tree than the same session
/// captured through the daemon client, which ships these anchors.
///
/// `None` when the user opted out of transcripts or the harness is not Codex.
fn spawn_codex_anchor_lane(
    config: &StartConfig,
    snapshot: tapes_harnesses::attribution::CodexWatcherSnapshotHandle,
) -> Result<
    Option<(
        tokio::sync::oneshot::Sender<()>,
        tokio::task::JoinHandle<()>,
    )>,
> {
    if !config.transcripts || !config.harness.is_codex() {
        return Ok(None);
    }
    let client = TranscriptClient::new(&config.ingest_url)?;
    Ok(Some(codex_anchors::spawn(
        client,
        snapshot,
        codex_anchors::AnchorLaneConfig::new(CODEX_PROVIDER_PREFIX),
    )))
}

/// The last line printed before the harness takes the terminal.
///
/// The log path has to be handed over *before* that window opens, because from
/// the spawn until the harness exits this process may not write to the terminal
/// at all. Silent when tracing is streaming to stderr — then the user is already
/// watching it, and there is no file to point at.
fn announce_capture() {
    if let Some(path) = logging::active_log_file() {
        println!("tapesctl: capturing; logs at {}", path.display());
    }
}

/// What this session was, printed once the terminal belongs to the caller again.
///
/// Three outcomes, and they are deliberately not collapsed:
///
/// * nothing captured — benign, and the only case that may say so;
/// * captured and attributed — the session link, as before;
/// * captured and unattributed — turns landed under `unknown`, which is a bug
///   in attribution and is reported as one rather than as silence.
///
/// The mixed case gets the link *and* the warning: the link is true, and it is
/// incomplete, because some of this session's turns are not filed under it.
fn print_exit_summary(web_url: Option<&Url>, snapshot: &CaptureSnapshot) {
    match tally::exit_summary_for(snapshot) {
        // Not an error: a harness can be launched and quit without ever calling
        // a model. Saying so beats printing nothing and leaving the user to
        // wonder whether capture was ever on.
        tally::ExitSummary::NothingCaptured => println!("tapesctl: no turns were captured"),
        tally::ExitSummary::Session(id) => print_session_link(web_url, &id),
        tally::ExitSummary::Unattributed(counts) => {
            println!("{}", tally::unattributed_line(counts));
        }
    }

    // stderr, and unconditional on the count rather than on the branch above:
    // a launch that files any turn under `unknown` has lost data the caller
    // cannot find by name, and that is worth surfacing every time.
    if snapshot.counts.has_unattributed() {
        eprintln!("{}", tally::unattributed_warning(snapshot.counts));
    }

    // Only after a drain gave up. Every line above it is then a floor rather
    // than a total, and a summary that let that pass silently would be making
    // the same claim-more-than-you-know mistake in a new place.
    if snapshot.in_flight > 0 {
        eprintln!("{}", tally::in_flight_note(snapshot.in_flight));
    }

    if let Some(path) = logging::active_log_file() {
        println!("tapesctl: logs at {}", path.display());
    }
}

fn print_session_link(web_url: Option<&Url>, session_id: &str) {
    match web_url.and_then(|base| base.join(&format!("/sessions/{session_id}")).ok()) {
        Some(url) => println!("tapesctl: captured session {session_id} — {url}"),
        // Without a console base URL there is no link to print; naming the flag
        // beats printing a guessed host that 404s.
        None => {
            println!("tapesctl: captured session {session_id} (pass --web-url for a console link)",)
        }
    }
}

/// Sentinel for "no harness has been spawned yet". PIDs are positive, and 0 is
/// not a PID any process can have.
pub const NO_LAUNCHED_PID: i32 = 0;

/// Launch the harness, publishing its PID before waiting on it.
///
/// Spawned and awaited in two steps rather than through `status()`, which never
/// exposes the child. The PID is what
/// [`tapes_capture::peer_trust::peer_is_launched_harness`]
/// compares a request's peer against, so it has to be published while the
/// process is running, not returned after it exits. Stdio is inherited either
/// way, which is what keeps the harness's TUI attached to the terminal.
async fn spawn_harness(
    config: &StartConfig,
    plan: &LaunchPlan,
    launched_pid: &AtomicI32,
) -> Result<std::process::ExitStatus> {
    let mut command = tokio::process::Command::new(config.harness.program());
    command.args(&plan.args);
    command.args(&config.harness_args);
    for (key, value) in &plan.env {
        command.env(key, value);
    }
    let mut child = command.spawn().context(error::SpawnHarnessSnafu {
        harness: config.harness.program(),
    })?;
    // Published before the first await, so the harness cannot have issued a
    // request that races the store.
    if let Some(pid) = child.id().and_then(|pid| i32::try_from(pid).ok()) {
        launched_pid.store(pid, Ordering::Relaxed);
    }
    child.wait().await.context(error::SpawnHarnessSnafu {
        harness: config.harness.program(),
    })
}

fn materialise_config_files(plan: &LaunchPlan) -> Result<Vec<PathBuf>> {
    let mut written = Vec::new();
    for file in &plan.config_files {
        if let Some(parent) = file.path.parent() {
            std::fs::create_dir_all(parent).context(error::ConfigFileSnafu {
                path: file.path.clone(),
            })?;
        }
        std::fs::write(&file.path, &file.contents).context(error::ConfigFileSnafu {
            path: file.path.clone(),
        })?;
        written.push(file.path.clone());
    }
    Ok(written)
}

fn remove_config_files(paths: &[PathBuf]) {
    for path in paths {
        if let Err(err) = std::fs::remove_file(path) {
            warn!(path = %path.display(), error = %err, "could not remove launch config");
        }
    }
}

fn claude_sessions_dir() -> Result<PathBuf> {
    claude_session::default_sessions_dir().context(error::NoHomeDirSnafu)
}

fn codex_sessions_dir() -> Result<PathBuf> {
    codex_session::default_sessions_dir().context(error::NoHomeDirSnafu)
}

/// Environment overlay a plan applies, as a map — for tests and diagnostics.
#[must_use]
pub fn plan_env(plan: &LaunchPlan) -> HashMap<String, String> {
    plan.env.iter().cloned().collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn args(harness: &str) -> StartArgs {
        StartArgs {
            harness: harness.to_owned(),
            harness_args: Vec::new(),
            ingest_url: Some("http://127.0.0.1:8090".to_owned()),
            upstream: None,
            schema: None,
            web_url: None,
            org_id: None,
            auth_subject: None,
            no_transcripts: false,
        }
    }

    fn addr() -> SocketAddr {
        "127.0.0.1:51000".parse().unwrap()
    }

    #[test]
    fn harness_names_are_case_insensitive_and_trimmed() {
        assert_eq!(Harness::parse("claude").unwrap(), Harness::Claude);
        assert_eq!(Harness::parse("  CODEX ").unwrap(), Harness::Codex);
    }

    #[test]
    fn an_unsupported_harness_is_rejected_by_name() {
        let err = Harness::parse("gemini").unwrap_err();
        assert!(format!("{err}").contains("gemini"), "got: {err}");
    }

    #[test]
    fn the_provider_names_the_captured_wire_format_not_the_vendor() {
        // Codex speaks the OpenAI Responses API, and ingest picks its
        // server-side reducer by this name — calling it "codex" would leave the
        // turn unreduced.
        assert_eq!(Harness::Codex.provider(None), "openai");
        assert_eq!(Harness::Claude.provider(None), "anthropic");
        // And pi, which speaks whichever schema the capture fronts, is named by
        // the schema rather than by itself — "pi" is not a wire format.
        assert_eq!(Harness::Pi.provider(Some(UpstreamSchema::OpenAi)), "openai");
        assert_eq!(
            Harness::Pi.provider(Some(UpstreamSchema::Anthropic)),
            "anthropic"
        );
    }

    #[test]
    fn claude_gets_a_bare_endpoint_and_api_key_codex_gets_a_v1_suffix() {
        // Codex appends `/responses`, so an API-key endpoint must already end
        // at the `/v1` segment for OpenAI's `/v1/responses` route to resolve.
        assert_eq!(
            Harness::Claude.endpoint_for(addr(), None).as_str(),
            "http://127.0.0.1:51000",
        );
        assert_eq!(
            Harness::Codex
                .endpoint_for(addr(), Some(CodexAuth::ApiKey))
                .as_str(),
            "http://127.0.0.1:51000/v1",
        );
    }

    #[test]
    fn chatgpt_codex_gets_no_v1_suffix() {
        // The ChatGPT backend's path has no `/v1` component; adding one sends
        // codex to a route that does not exist.
        assert_eq!(
            Harness::Codex
                .endpoint_for(addr(), Some(CodexAuth::ChatGpt))
                .as_str(),
            "http://127.0.0.1:51000",
        );
    }

    #[test]
    fn the_codex_upstream_follows_the_credential_not_a_preference() {
        // Plan OAuth tokens are honoured only by the ChatGPT backend, and API
        // keys only by api.openai.com — so the credential picks the host.
        assert_eq!(
            Harness::Codex.default_upstream(Some(CodexAuth::ChatGpt), None),
            DEFAULT_CHATGPT_UPSTREAM,
        );
        assert_eq!(
            Harness::Codex.default_upstream(Some(CodexAuth::ApiKey), None),
            DEFAULT_OPENAI_UPSTREAM,
        );
    }

    #[test]
    fn the_claude_plan_points_the_harness_at_the_proxy() {
        let plan = Harness::Claude
            .plan(
                Harness::Claude.endpoint_for(addr(), None),
                "unused",
                None,
                None,
                "unused-nonce",
                false,
            )
            .unwrap();
        let env = plan_env(&plan);
        assert_eq!(
            env.get("ANTHROPIC_BASE_URL").map(String::as_str),
            Some("http://127.0.0.1:51000"),
        );
        // The nonce is the self-attribution secret, and Claude never names its
        // own session — handing the value to a harness that has no use for it
        // would only widen where the secret lives.
        assert!(
            !env.contains_key(GATEWAY_NONCE_ENV),
            "a redirected harness's environment must not carry the capture nonce",
        );
    }

    #[test]
    fn the_codex_plan_declares_the_marker_header_and_provider() {
        let provider_id = format!("{CODEX_PROVIDER_PREFIX}-abc");
        let plan = Harness::Codex
            .plan(
                Harness::Codex.endpoint_for(addr(), Some(CodexAuth::ApiKey)),
                &provider_id,
                Some(CodexAuth::ApiKey),
                None,
                "unused-nonce",
                false,
            )
            .unwrap();
        let joined = plan.args.join(" ");
        assert!(joined.contains(&provider_id), "got: {joined}");
        assert!(
            joined.contains(CODEX_MARKER_HEADER),
            "the proxy cannot tell two codex processes apart without it: {joined}",
        );
    }

    #[test]
    fn the_launched_provider_id_matches_this_clients_filter() {
        // The suffixed id and the filter must agree, or every Codex turn is
        // attributed to nothing.
        let provider_id = format!("{CODEX_PROVIDER_PREFIX}-{}", uuid::Uuid::new_v4());
        let filter = CodexProviderFilter::new(CODEX_PROVIDER_PREFIX);
        assert!(filter.matches(Some(&provider_id)));
    }

    #[test]
    fn a_missing_tapes_url_is_an_error_rather_than_a_silent_no_capture() {
        let mut args = args("claude");
        args.ingest_url = None;
        assert!(StartConfig::resolve(args).is_err());
    }

    #[test]
    fn upstream_defaults_per_harness() {
        // Only Claude is asserted through `resolve` here. Codex's default
        // depends on which credential the *ambient* environment offers, so
        // pinning it in a test that reads the real environment would pass or
        // fail according to whether the machine happens to export
        // `OPENAI_API_KEY`. The credential-to-host mapping is asserted
        // directly, without the environment, in
        // `the_codex_upstream_follows_the_credential_not_a_preference`.
        assert_eq!(
            StartConfig::resolve(args("claude"))
                .unwrap()
                .upstream
                .as_str(),
            "https://api.anthropic.com/",
        );
    }

    #[test]
    fn an_explicit_upstream_wins() {
        let mut args = args("claude");
        args.upstream = Some("http://127.0.0.1:52292".to_owned());
        assert_eq!(
            StartConfig::resolve(args).unwrap().upstream.as_str(),
            "http://127.0.0.1:52292/",
        );
    }

    #[test]
    fn the_default_subject_names_the_local_user() {
        let subject = StartConfig::resolve(args("claude")).unwrap().auth_subject;
        assert!(subject.starts_with("local:"), "got: {subject}");
    }

    #[test]
    fn an_explicit_subject_wins_so_ci_can_name_itself() {
        let mut args = args("claude");
        args.auth_subject = Some("gardener-ci".to_owned());
        assert_eq!(
            StartConfig::resolve(args).unwrap().auth_subject,
            "gardener-ci",
        );
    }

    #[test]
    fn the_org_id_defaults_to_empty_for_the_local_sentinel() {
        assert_eq!(StartConfig::resolve(args("claude")).unwrap().org_id, "");
    }

    // --- pi ------------------------------------------------------------------

    #[test]
    fn pi_resolves_through_the_shared_registry() {
        assert_eq!(Harness::parse("pi").unwrap(), Harness::Pi);
        assert_eq!(Harness::parse(" PI ").unwrap(), Harness::Pi);
        // Resolution goes through the registry, so an alias declared there works
        // here without this file listing it.
        assert_eq!(Harness::parse("claude-code").unwrap(), Harness::Claude);
    }

    #[test]
    fn a_registered_harness_with_no_arm_here_is_still_unsupported() {
        // The Codex desktop app is in the shared registry — it is capturable,
        // and the crate ships its hook templates — but it is configured rather
        // than launched, so this binary has no arm for it. Resolving through the
        // registry must not make it look launchable.
        let err = Harness::parse("codex-app").unwrap_err();
        let message = format!("{err}");
        assert!(message.contains("codex-app"), "got: {message}");
        // And the message advertises exactly what `start` will launch.
        assert!(message.contains("claude, codex, pi"), "got: {message}");
    }

    #[test]
    fn the_pi_plan_carries_the_crates_environment_contract_and_no_argv() {
        // The whole launch: pi has no base-URL flag, so what points it at this
        // proxy is the environment its already-installed extension reads.
        let plan = Harness::Pi
            .plan(
                Harness::Pi.endpoint_for(addr(), None),
                "unused",
                None,
                Some(UpstreamSchema::Anthropic),
                "per-launch-secret",
                false,
            )
            .unwrap();
        let env = plan_env(&plan);
        assert_eq!(
            env.get(GATEWAY_URL_ENV).map(String::as_str),
            Some("http://127.0.0.1:51000"),
        );
        assert_eq!(
            env.get(GATEWAY_SCHEMA_ENV).map(String::as_str),
            Some("anthropic"),
        );
        // The nonce the proxy will demand echoed. Without it in the launched
        // environment, every pi envelope would be refused and every turn would
        // file under `unknown`.
        assert_eq!(
            env.get(GATEWAY_NONCE_ENV).map(String::as_str),
            Some("per-launch-secret"),
        );
        assert!(
            plan.args.is_empty(),
            "pi loads the extension from its own auto-discovery directory; \
             argv here would be a second, competing copy: {:?}",
            plan.args,
        );
        assert!(plan.config_files.is_empty());
    }

    #[test]
    fn the_pi_endpoint_has_no_path_suffix() {
        // pi's providers each append their own full path, so a `/v1` here would
        // land every request one segment deep.
        assert_eq!(
            Harness::Pi.endpoint_for(addr(), None).as_str(),
            "http://127.0.0.1:51000",
        );
    }

    #[test]
    fn the_pi_schema_selects_the_upstream_the_provider_and_the_hint_together() {
        // These three must move as one: forwarding to OpenAI while telling
        // ingest "anthropic" would capture turns no reducer can read.
        let mut args = args("pi");
        args.schema = Some("openai".to_owned());
        let config = StartConfig::resolve(args).unwrap();
        assert_eq!(config.schema, Some(UpstreamSchema::OpenAi));
        assert_eq!(config.upstream.as_str(), "https://api.openai.com/");
        assert_eq!(config.harness.provider(config.schema), "openai");
    }

    #[test]
    fn pi_defaults_to_the_anthropic_schema() {
        let config = StartConfig::resolve(args("pi")).unwrap();
        assert_eq!(config.schema, Some(DEFAULT_PI_SCHEMA));
        assert_eq!(config.upstream.as_str(), "https://api.anthropic.com/");
        assert_eq!(config.harness.provider(config.schema), "anthropic");
    }

    /// pi registers three providers, so a pi launch that did not route them
    /// would send two thirds of the session to a host that cannot serve it.
    /// The schema still picks the *default* upstream, which is what an
    /// unlabelled request falls back to.
    #[test]
    fn a_pi_launch_routes_every_provider_it_registers() {
        let config = StartConfig::resolve(args("pi")).unwrap();
        assert!(config.provider_routes);
        assert!(Harness::Pi.registers_several_providers());
    }

    /// A harness whose extension redirects exactly one provider has nothing to
    /// route between, and labelling would only invent a path its upstream has
    /// no route for.
    #[test]
    fn a_harness_that_registers_one_provider_never_labels() {
        for harness in ["claude", "codex"] {
            let config = StartConfig::resolve(args(harness)).unwrap();
            assert!(
                !config.provider_routes,
                "{harness} asked its extension to label registrations"
            );
        }
    }

    /// An explicit `--upstream` names one host and means it. A user pointing a
    /// capture at a local gateway is asking for the whole session to go there;
    /// a per-provider table would quietly send two thirds of it to the public
    /// APIs instead.
    #[test]
    fn an_explicit_upstream_turns_provider_routing_off() {
        let mut args = args("pi");
        args.upstream = Some("http://127.0.0.1:18832/local-gw".to_owned());
        let config = StartConfig::resolve(args).unwrap();
        assert!(!config.provider_routes);
        assert_eq!(config.upstream.as_str(), "http://127.0.0.1:18832/local-gw");
    }

    /// Every label the extension can send resolves to a host and an ingest
    /// provider. A registered provider with no route would be refused at the
    /// proxy, which is a working session broken by its own launch.
    #[test]
    fn every_provider_the_extension_registers_has_a_route() {
        let routes = ProviderRoutes::defaults().unwrap();
        for provider in ["anthropic", "openai", "openai-codex"] {
            assert!(
                routes.resolve(provider).is_some(),
                "the extension registers {provider} and this capture cannot route it"
            );
        }
        // The plan credential rides its own host, and files under the schema it
        // speaks rather than under its own name. The base is the backend ROOT:
        // pi's client supplies the `/codex` segment in the path it composes,
        // and a base that repeats it produces `/codex/codex/responses` — a 404
        // the harness reports as a model failure.
        let plan = routes.resolve("openai-codex").unwrap();
        assert_eq!(plan.upstream.as_str(), "https://chatgpt.com/backend-api");
        assert_eq!(plan.provider, "openai");
        let composed =
            super::proxy::build_upstream_url(&plan.upstream, "/codex/responses", None).unwrap();
        assert_eq!(
            composed.as_str(),
            "https://chatgpt.com/backend-api/codex/responses",
            "the path pi actually sends must land on the endpoint the backend serves",
        );
    }

    /// The launched environment carries the switch exactly when the proxy can
    /// honour it. An extension told to label a proxy that refuses labels would
    /// break every turn of the session.
    #[test]
    fn the_pi_plan_asks_the_extension_to_label_only_when_this_launch_routes() {
        let plan = Harness::Pi
            .plan(
                Harness::Pi.endpoint_for(addr(), None),
                "unused",
                None,
                Some(UpstreamSchema::Anthropic),
                "per-launch-secret",
                true,
            )
            .unwrap();
        let env = plan_env(&plan);
        assert_eq!(
            env.get(GATEWAY_PROVIDER_ROUTES_ENV).map(String::as_str),
            Some(GATEWAY_PROVIDER_ROUTES_ON),
        );
        // The status entry would otherwise name one schema for a capture
        // carrying all of them.
        assert_eq!(
            env.get(GATEWAY_LABEL_SUFFIX_ENV).map(String::as_str),
            Some(PROVIDER_ROUTES_LABEL_SUFFIX),
        );

        let unrouted = Harness::Pi
            .plan(
                Harness::Pi.endpoint_for(addr(), None),
                "unused",
                None,
                Some(UpstreamSchema::Anthropic),
                "per-launch-secret",
                false,
            )
            .unwrap();
        assert!(!plan_env(&unrouted).contains_key(GATEWAY_PROVIDER_ROUTES_ENV));
    }

    #[test]
    fn the_schema_spellings_are_ones_the_extension_recognises() {
        // The asset switches on these strings. A spelling this client invented
        // would leave the extension unable to tell which schema is fronted, and
        // its mismatch warning silently dead.
        let asset = tapes_harnesses::plugin::PI_GATEWAY_EXTENSION.contents();
        for schema in [UpstreamSchema::Anthropic, UpstreamSchema::OpenAi] {
            assert!(
                asset.contains(&format!("\"{}\"", schema.as_str())),
                "the pi extension does not know the schema {:?}",
                schema.as_str(),
            );
        }
    }

    #[test]
    fn a_schema_is_refused_for_a_harness_that_speaks_only_one() {
        let mut args = args("claude");
        args.schema = Some("openai".to_owned());
        let err = StartConfig::resolve(args).unwrap_err();
        let message = format!("{err}");
        assert!(message.contains("claude"), "got: {message}");
        assert!(message.contains("anthropic"), "got: {message}");
    }

    #[test]
    fn an_unknown_schema_is_refused_with_the_valid_values() {
        let mut args = args("pi");
        args.schema = Some("gemini".to_owned());
        let err = StartConfig::resolve(args).unwrap_err();
        let message = format!("{err}");
        assert!(message.contains("gemini"), "got: {message}");
        assert!(message.contains("anthropic"), "got: {message}");
    }

    #[test]
    fn the_extension_captured_harnesses_self_attribute_and_the_others_do_not() {
        // Drives the proxy's choice of whose session id to file a turn under.
        // Neither arm spells this out: it is read from the shared registry, so a
        // harness that gains an in-harness extension takes the lane without this
        // file changing — which is exactly how opencode joined it.
        assert!(Harness::Pi.is_self_attributing());
        assert!(Harness::OpenCode.is_self_attributing());
        assert!(!Harness::Claude.is_self_attributing());
        assert!(!Harness::Codex.is_self_attributing());
    }

    #[test]
    fn launching_pi_without_its_extension_names_the_installer() {
        // The failure a user will actually hit: `start pi` before
        // `plugin install pi`. It must say what to run, not just what is
        // missing.
        let home = tempfile::tempdir().unwrap();
        let err = ensure_plugin_installed(Harness::Pi, home.path()).unwrap_err();
        let message = format!("{err}");
        assert!(
            message.contains("tapesctl plugin install pi"),
            "got: {message}",
        );
        assert!(message.contains("tapes-gateway.ts"), "got: {message}");
    }

    #[test]
    fn launching_pi_with_its_extension_installed_proceeds() {
        // The same check must pass once the installer has run, or `start pi`
        // could never launch at all.
        let home = tempfile::tempdir().unwrap();
        for artifact in Harness::Pi.required_plugin_artifacts() {
            let path = artifact.install_path(home.path());
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, artifact.contents()).unwrap();
        }
        ensure_plugin_installed(Harness::Pi, home.path()).unwrap();
    }

    #[test]
    fn a_harness_captured_by_redirect_needs_nothing_installed() {
        // Claude has no artifacts, so an empty home must not block its launch.
        let home = tempfile::tempdir().unwrap();
        assert!(Harness::Claude.required_plugin_artifacts().is_empty());
        ensure_plugin_installed(Harness::Claude, home.path()).unwrap();
    }

    // --- opencode -------------------------------------------------------------
    //
    // opencode takes pi's lane, so these mirror pi's tests rather than inventing
    // a shape. What is asserted below is precisely the places the two harnesses
    // are *not* interchangeable — and, first, that none of it is reachable
    // through `start` today.

    /// **The withdrawal.** `start opencode` is refused exactly the way an
    /// unknown name is: from a user's side there is no difference between "we
    /// never had this" and "we are not offering this", and inventing a third
    /// answer would only invite them to work around it.
    #[test]
    fn a_withdrawn_harness_is_refused_the_way_an_unrecognised_one_is() {
        for name in ["opencode", "  OpenCode "] {
            let message = format!("{}", Harness::parse(name).unwrap_err());
            assert!(message.contains(name.trim()), "got: {message}");
            assert!(!message.contains(", opencode"), "still offered: {message}");
        }
    }

    /// …and it is a withdrawal, not a deletion: the registry still knows the
    /// harness, `tapesctl plugin install opencode` still installs its capture
    /// plugin, and the arms below still work. Reinstating the verb is moving one
    /// entry from [`WITHDRAWN`] to [`SUPPORTED`], which is why the two lists are
    /// asserted disjoint rather than one being derived from the other.
    #[test]
    fn a_withdrawn_harness_keeps_everything_except_the_verb() {
        for harness in WITHDRAWN {
            assert!(!SUPPORTED.contains(harness), "{harness:?} is still offered");
            let registered = registry::find(harness.id())
                .unwrap_or_else(|| panic!("{harness:?} withdrawn from a registry that lacks it"));
            assert!(
                registered.is_launchable(),
                "{harness:?} needed no withdrawing"
            );
            assert!(
                !harness.required_plugin_artifacts().is_empty(),
                "{harness:?}'s capture plugin went with the verb",
            );
        }
    }

    #[test]
    fn opencode_still_resolves_to_its_registry_identity() {
        assert_eq!(Harness::OpenCode.program(), "opencode");
        assert_eq!(Harness::OpenCode.id(), "opencode");
    }

    #[test]
    fn the_opencode_plan_carries_the_crates_environment_contract_and_no_argv() {
        // Byte-for-byte the pi contract: the crate's plugin reads these three
        // names, and nothing else points opencode at this proxy.
        let plan = Harness::OpenCode
            .plan(
                Harness::OpenCode.endpoint_for(addr(), None),
                "unused",
                None,
                Some(UpstreamSchema::Anthropic),
                "per-launch-secret",
                false,
            )
            .unwrap();
        let env = plan_env(&plan);
        assert_eq!(
            env.get(GATEWAY_URL_ENV).map(String::as_str),
            Some("http://127.0.0.1:51000"),
        );
        assert_eq!(
            env.get(GATEWAY_SCHEMA_ENV).map(String::as_str),
            Some("anthropic"),
        );
        assert_eq!(
            env.get(GATEWAY_NONCE_ENV).map(String::as_str),
            Some("per-launch-secret"),
        );
        assert!(plan.args.is_empty(), "got: {:?}", plan.args);
        // And in particular: no config document. The crate's OpenCodeRecipe
        // would emit one and relocate XDG_CONFIG_HOME for the whole process
        // tree; this arm deliberately takes the plugin road instead, so there
        // is nothing to materialise and nothing to clean up.
        assert!(plan.config_files.is_empty(), "got: {:?}", plan.config_files,);
    }

    #[test]
    fn the_opencode_endpoint_is_the_bare_proxy_origin() {
        // The `/v1` opencode's AI SDK adapters need is added by the plugin, not
        // here — so GATEWAY_URL_ENV means the same thing for opencode as for pi.
        // A suffix here would double it.
        assert_eq!(
            Harness::OpenCode.endpoint_for(addr(), None).as_str(),
            "http://127.0.0.1:51000",
        );
    }

    /// Asserted on the arm rather than through `StartConfig::resolve`, which no
    /// longer accepts the name. The schema/upstream/provider triple is the work
    /// the withdrawal is pending a rework of, and it should keep being held to
    /// its shape while it waits.
    #[test]
    fn opencode_defaults_to_the_anthropic_schema() {
        let harness = Harness::OpenCode;
        assert_eq!(harness.default_schema(), Some(DEFAULT_OPENCODE_SCHEMA));
        assert_eq!(
            harness.default_upstream(None, Some(DEFAULT_OPENCODE_SCHEMA)),
            "https://api.anthropic.com",
        );
        assert_eq!(harness.provider(Some(DEFAULT_OPENCODE_SCHEMA)), "anthropic");
    }

    #[test]
    fn the_opencode_schema_selects_the_upstream_and_the_provider_together() {
        let harness = Harness::OpenCode;
        let schema = Some(UpstreamSchema::OpenAi);
        assert_eq!(
            harness.default_upstream(None, schema),
            "https://api.openai.com",
        );
        assert_eq!(harness.provider(schema), "openai");
    }

    #[test]
    fn the_schema_spellings_are_ones_the_opencode_plugin_recognises() {
        // Same pin as the pi asset's, against opencode's own copy: these
        // strings are simultaneously this client's schema names, opencode's
        // provider ids, and the names ingest keys its reducer on. A spelling
        // this client invented would redirect a provider opencode does not have.
        let asset = tapes_harnesses::plugin::OPENCODE_GATEWAY_EXTENSION.contents();
        for schema in [UpstreamSchema::Anthropic, UpstreamSchema::OpenAi] {
            assert!(
                asset.contains(&format!("\"{}\"", schema.as_str())),
                "the opencode plugin does not know the schema {:?}",
                schema.as_str(),
            );
        }
    }

    #[test]
    fn launching_opencode_without_its_plugin_names_the_installer() {
        let home = tempfile::tempdir().unwrap();
        let err = ensure_plugin_installed(Harness::OpenCode, home.path()).unwrap_err();
        let message = format!("{err}");
        assert!(
            message.contains("tapesctl plugin install opencode"),
            "got: {message}",
        );
        assert!(message.contains("tapes-gateway.ts"), "got: {message}");
    }

    #[test]
    fn launching_opencode_with_its_plugin_installed_proceeds() {
        let home = tempfile::tempdir().unwrap();
        for artifact in Harness::OpenCode.required_plugin_artifacts() {
            let path = artifact.install_path(home.path());
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, artifact.contents()).unwrap();
        }
        ensure_plugin_installed(Harness::OpenCode, home.path()).unwrap();
    }

    #[test]
    fn opencode_and_pi_install_to_different_places() {
        // Two harnesses on one lane, but each auto-discovers from its own
        // directory. A shared destination would mean installing one silently
        // disabled the other.
        let home = Path::new("/home/u");
        let paths = |harness: Harness| {
            harness
                .required_plugin_artifacts()
                .iter()
                .map(|artifact| artifact.install_path(home))
                .collect::<Vec<_>>()
        };
        let opencode = paths(Harness::OpenCode);
        let pi = paths(Harness::Pi);
        assert_eq!(opencode.len(), 1);
        assert_eq!(pi.len(), 1);
        assert_ne!(opencode, pi);
        assert!(
            opencode[0].starts_with("/home/u/.config/opencode"),
            "got: {:?}",
            opencode[0],
        );
    }

    #[test]
    fn opencode_gets_no_transcript_tailer() {
        // opencode keeps sessions in a SQLite database, not a tree the shared
        // crate's discovery can walk, so the wire lane is the whole capture.
        // Asserted through the registry rather than the tailer, which needs a
        // home directory to start.
        assert_eq!(
            registry::find("opencode").map(tapes_harnesses::harness::Harness::transcripts),
            Some(registry::TranscriptSource::None),
        );
    }
}

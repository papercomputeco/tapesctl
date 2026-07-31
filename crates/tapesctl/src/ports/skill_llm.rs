//! The extraction call: one prompt in, one JSON document out.
//!
//! Three providers, reproduced from the Go implementation wire-shape for
//! wire-shape — same routes, same headers, same request bodies, same defaults.
//! This is a port, so the shapes are the contract: a user who pointed the Go
//! command at a local Ollama or an OpenAI-compatible gateway must be able to
//! point this one at the same place and have it work.
//!
//! Note this is the *only* place tapesctl talks to something other than a tapes
//! server. It is a client-side call — the tapes API has no LLM endpoint to
//! proxy it through — so the key is the user's and never leaves the machine
//! except to the provider they named.
//!
//! # Credentials
//!
//! Resolution is `--api-key`, then the provider's environment variable. The Go
//! command had a third source ahead of the environment — the credential store
//! `tapes auth` writes — which tapesctl has no equivalent of. A user who
//! authenticated through `tapes auth` must pass `--api-key` or export the
//! variable instead.

use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use snafu::ResultExt;
use url::Url;

use crate::error::{Error, Result, error};

/// One deadline spanning every attempt, so a retry cannot extend the budget.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Additional attempts after a transient provider failure. One is enough to
/// ride out a brief rate-limit or upstream blip without risking the deadline.
const CALL_RETRIES: u32 = 1;

/// Pause between attempts.
const RETRY_BACKOFF: Duration = Duration::from_millis(500);

/// Which provider performs the extraction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    /// OpenAI chat completions.
    OpenAi,
    /// Anthropic messages.
    Anthropic,
    /// A local Ollama daemon.
    Ollama,
}

impl Provider {
    /// Resolve a user-typed provider name. An empty value is OpenAI, matching
    /// the Go switch that treated `""` as the default provider.
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "openai" | "" => Ok(Self::OpenAi),
            "anthropic" => Ok(Self::Anthropic),
            "ollama" => Ok(Self::Ollama),
            other => error::LlmProviderSnafu {
                provider: other.to_owned(),
            }
            .fail(),
        }
    }

    /// How the provider names itself in errors.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
            Self::Ollama => "ollama",
        }
    }

    /// The model used when the user names none.
    #[must_use]
    pub const fn default_model(self) -> &'static str {
        match self {
            Self::OpenAi => "gpt-4o-mini",
            // Reproduced verbatim from the command this ports. Newer Anthropic
            // model ids are undated; changing it here would silently move every
            // existing user to a different model, which is a decision for the
            // command's owner and not for a port.
            Self::Anthropic => "claude-haiku-4-5-20251001",
            Self::Ollama => "llama3.2",
        }
    }

    /// The host used when the user names none.
    #[must_use]
    pub const fn default_base_url(self) -> &'static str {
        match self {
            Self::OpenAi => "https://api.openai.com",
            Self::Anthropic => "https://api.anthropic.com",
            Self::Ollama => "http://localhost:11434",
        }
    }

    /// The environment variable consulted when no key is passed.
    #[must_use]
    pub const fn env_var(self) -> &'static str {
        match self {
            Self::Anthropic => "ANTHROPIC_API_KEY",
            Self::OpenAi => "OPENAI_API_KEY",
            Self::Ollama => "OPENAI_API_KEY or ANTHROPIC_API_KEY",
        }
    }

    /// Whether a key is required. Ollama is local and typically unauthenticated.
    #[must_use]
    pub const fn requires_key(self) -> bool {
        !matches!(self, Self::Ollama)
    }
}

/// How to reach the extraction model.
#[derive(Debug, Clone, Default)]
pub struct LlmConfig {
    /// Provider name as the user typed it.
    pub provider: String,
    /// Model override.
    pub model: Option<String>,
    /// Explicit key, ahead of the environment.
    pub api_key: Option<String>,
    /// Host override. Not a CLI flag — the Go command did not expose one
    /// either — but the seam the tests point at a mock provider.
    pub base_url: Option<String>,
}

/// Read the provider's key out of the environment.
///
/// Ollama's branch tries both keys, mirroring the Go default case: a
/// user pointing `--provider ollama` at an authenticated gateway gets whichever
/// key they have exported.
fn key_from_env(provider: Provider) -> Option<String> {
    let named = |name: &str| std::env::var(name).ok().filter(|key| !key.is_empty());
    match provider {
        Provider::Anthropic => named("ANTHROPIC_API_KEY"),
        Provider::OpenAi => named("OPENAI_API_KEY"),
        Provider::Ollama => named("OPENAI_API_KEY").or_else(|| named("ANTHROPIC_API_KEY")),
    }
}

/// A configured extraction caller.
#[derive(Debug, Clone)]
pub struct LlmCaller {
    http: reqwest::Client,
    provider: Provider,
    model: String,
    api_key: String,
    base: Url,
}

impl LlmCaller {
    /// Resolve provider, model, key, and host.
    pub fn new(config: &LlmConfig) -> Result<Self> {
        let provider = Provider::parse(&config.provider)?;

        let api_key = config
            .api_key
            .clone()
            .filter(|key| !key.is_empty())
            .or_else(|| key_from_env(provider))
            .unwrap_or_default();
        snafu::ensure!(
            !api_key.is_empty() || !provider.requires_key(),
            error::LlmNoApiKeySnafu {
                provider: provider.as_str(),
                env_var: provider.env_var(),
            }
        );

        let base = config
            .base_url
            .clone()
            .filter(|raw| !raw.is_empty())
            .unwrap_or_else(|| provider.default_base_url().to_owned());

        Ok(Self {
            http: reqwest::Client::new(),
            provider,
            model: config
                .model
                .clone()
                .filter(|model| !model.is_empty())
                .unwrap_or_else(|| provider.default_model().to_owned()),
            api_key,
            base: Url::parse(&base).context(error::LlmUrlSnafu)?,
        })
    }

    /// The resolved model, for the progress line.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The resolved provider.
    #[must_use]
    pub const fn provider(&self) -> Provider {
        self.provider
    }

    /// Run one extraction call, returning the model's raw text.
    pub async fn call(&self, prompt: &str) -> Result<String> {
        // The deadline wraps the retry loop rather than each attempt, so a
        // retry never extends the budget past CALL_TIMEOUT.
        tokio::time::timeout(CALL_TIMEOUT, self.call_inner(prompt))
            .await
            .unwrap_or(Err(Error::LlmTimeout {
                provider: self.provider.as_str(),
            }))
    }

    async fn call_inner(&self, prompt: &str) -> Result<String> {
        let (path, headers, body) = self.request(prompt);
        let url = self.base.join(path).context(error::LlmUrlSnafu)?;

        let mut last: Option<Error> = None;
        for attempt in 0..=CALL_RETRIES {
            if attempt > 0 {
                tokio::time::sleep(RETRY_BACKOFF).await;
            }

            let mut request = self
                .http
                .post(url.clone())
                .header(http::header::CONTENT_TYPE, "application/json");
            for (name, value) in &headers {
                request = request.header(*name, value);
            }

            let response = match request.json(&body).send().await {
                Ok(response) => response,
                Err(source) => {
                    // A transport blip is worth one more attempt.
                    last = Some(Error::LlmSend { source });
                    continue;
                }
            };

            let status = response.status();
            let payload = response.bytes().await.context(error::LlmSendSnafu)?;
            if status.is_success() {
                return self.extract(&payload);
            }

            last = Some(Error::LlmStatus {
                provider: self.provider.as_str(),
                status: status.as_u16(),
                body: String::from_utf8_lossy(&payload).into_owned(),
            });
            if !is_retryable(status.as_u16()) {
                break;
            }
        }
        Err(last.unwrap_or(Error::LlmTimeout {
            provider: self.provider.as_str(),
        }))
    }

    /// The route, extra headers, and body for this provider.
    fn request(&self, prompt: &str) -> (&'static str, Vec<(&'static str, String)>, Value) {
        match self.provider {
            Provider::OpenAi => (
                "/v1/chat/completions",
                vec![("authorization", format!("Bearer {}", self.api_key))],
                json!({
                    "model": self.model,
                    "messages": [{"role": "user", "content": prompt}],
                    "response_format": {"type": "json_object"},
                }),
            ),
            Provider::Anthropic => (
                "/v1/messages",
                vec![
                    ("x-api-key", self.api_key.clone()),
                    ("anthropic-version", "2023-06-01".to_owned()),
                ],
                json!({
                    "model": self.model,
                    "max_tokens": 1024,
                    "messages": [{
                        "role": "user",
                        "content": format!(
                            "{prompt}\n\nReturn ONLY valid JSON, no markdown or extra text.",
                        ),
                    }],
                }),
            ),
            Provider::Ollama => (
                "/api/chat",
                Vec::new(),
                json!({
                    "model": self.model,
                    "messages": [{"role": "user", "content": prompt}],
                    "stream": false,
                    "format": "json",
                }),
            ),
        }
    }

    /// Pull the text out of a provider's success body.
    fn extract(&self, payload: &[u8]) -> Result<String> {
        let empty = || {
            error::LlmEmptySnafu {
                provider: self.provider.as_str(),
            }
            .fail()
        };
        match self.provider {
            Provider::OpenAi => {
                let parsed: OpenAiResponse =
                    serde_json::from_slice(payload).context(error::LlmDecodeSnafu)?;
                if let Some(err) = parsed.error {
                    return error::LlmRefusedSnafu {
                        provider: self.provider.as_str(),
                        message: err.message,
                    }
                    .fail();
                }
                parsed
                    .choices
                    .into_iter()
                    .next()
                    .map_or_else(empty, |choice| Ok(choice.message.content))
            }
            Provider::Anthropic => {
                let parsed: AnthropicResponse =
                    serde_json::from_slice(payload).context(error::LlmDecodeSnafu)?;
                if let Some(err) = parsed.error {
                    return error::LlmRefusedSnafu {
                        provider: self.provider.as_str(),
                        message: err.message,
                    }
                    .fail();
                }
                parsed
                    .content
                    .into_iter()
                    .next()
                    .map_or_else(empty, |block| Ok(block.text))
            }
            Provider::Ollama => {
                let parsed: OllamaResponse =
                    serde_json::from_slice(payload).context(error::LlmDecodeSnafu)?;
                Ok(parsed.message.content)
            }
        }
    }
}

/// Statuses worth one more attempt: a transient provider condition.
const fn is_retryable(status: u16) -> bool {
    matches!(status, 429 | 502 | 503 | 504)
}

#[derive(Debug, Deserialize)]
struct ProviderError {
    #[serde(default)]
    message: String,
}

#[derive(Debug, Deserialize)]
struct OpenAiResponse {
    #[serde(default)]
    choices: Vec<OpenAiChoice>,
    #[serde(default)]
    error: Option<ProviderError>,
}

#[derive(Debug, Deserialize)]
struct OpenAiChoice {
    #[serde(default)]
    message: OpenAiMessage,
}

#[derive(Debug, Default, Deserialize)]
struct OpenAiMessage {
    #[serde(default)]
    content: String,
}

#[derive(Debug, Deserialize)]
struct AnthropicResponse {
    #[serde(default)]
    content: Vec<AnthropicBlock>,
    #[serde(default)]
    error: Option<ProviderError>,
}

#[derive(Debug, Deserialize)]
struct AnthropicBlock {
    #[serde(default)]
    text: String,
}

#[derive(Debug, Deserialize)]
struct OllamaResponse {
    #[serde(default)]
    message: OllamaMessage,
}

#[derive(Debug, Default, Deserialize)]
struct OllamaMessage {
    #[serde(default)]
    content: String,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_json_string, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn caller(server: &MockServer, provider: &str) -> LlmCaller {
        LlmCaller::new(&LlmConfig {
            provider: provider.to_owned(),
            api_key: Some("test-key".to_owned()),
            base_url: Some(server.uri()),
            model: None,
        })
        .unwrap()
    }

    #[test]
    fn provider_names_are_case_insensitive_and_empty_means_openai() {
        assert_eq!(Provider::parse("OpenAI").unwrap(), Provider::OpenAi);
        assert_eq!(Provider::parse(" anthropic ").unwrap(), Provider::Anthropic);
        assert_eq!(Provider::parse("ollama").unwrap(), Provider::Ollama);
        assert_eq!(Provider::parse("").unwrap(), Provider::OpenAi);
    }

    #[test]
    fn an_unknown_provider_is_refused_before_any_request() {
        let err = Provider::parse("gemini").unwrap_err();
        assert!(format!("{err}").contains("gemini"), "got: {err}");
    }

    #[test]
    fn each_provider_keeps_the_defaults_of_the_command_it_ports() {
        assert_eq!(Provider::OpenAi.default_model(), "gpt-4o-mini");
        assert_eq!(
            Provider::Anthropic.default_model(),
            "claude-haiku-4-5-20251001",
        );
        assert_eq!(Provider::Ollama.default_model(), "llama3.2");
        assert_eq!(
            Provider::OpenAi.default_base_url(),
            "https://api.openai.com"
        );
        assert_eq!(
            Provider::Anthropic.default_base_url(),
            "https://api.anthropic.com",
        );
        assert_eq!(
            Provider::Ollama.default_base_url(),
            "http://localhost:11434",
        );
    }

    #[test]
    fn an_explicit_key_is_not_required_for_a_local_ollama() {
        let built = LlmCaller::new(&LlmConfig {
            provider: "ollama".to_owned(),
            base_url: Some("http://127.0.0.1:11434".to_owned()),
            ..LlmConfig::default()
        });
        assert!(built.is_ok(), "got: {built:?}");
    }

    #[test]
    fn a_named_model_overrides_the_default() {
        let built = LlmCaller::new(&LlmConfig {
            provider: "openai".to_owned(),
            model: Some("gpt-4o".to_owned()),
            api_key: Some("k".to_owned()),
            base_url: None,
        })
        .unwrap();
        assert_eq!(built.model(), "gpt-4o");
    }

    #[tokio::test]
    async fn the_openai_request_is_the_shape_the_go_command_sent() {
        // A user pointing this at an OpenAI-compatible gateway must get the
        // same request the tool it replaces sent.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", "Bearer test-key"))
            .and(header("content-type", "application/json"))
            .and(body_json_string(
                r#"{"model":"gpt-4o-mini","messages":[{"role":"user","content":"extract"}],"response_format":{"type":"json_object"}}"#,
            ))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"choices":[{"message":{"content":"{\"description\":\"d\"}"}}]}"#,
            ))
            .mount(&server)
            .await;

        let text = caller(&server, "openai").call("extract").await.unwrap();
        assert_eq!(text, r#"{"description":"d"}"#);
    }

    #[tokio::test]
    async fn the_anthropic_request_carries_the_version_header_and_json_nudge() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "test-key"))
            .and(header("anthropic-version", "2023-06-01"))
            .and(body_json_string(
                "{\"model\":\"claude-haiku-4-5-20251001\",\"max_tokens\":1024,\"messages\":[{\"role\":\"user\",\"content\":\"extract\\n\\nReturn ONLY valid JSON, no markdown or extra text.\"}]}",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"content":[{"type":"text","text":"{\"description\":\"d\"}"}]}"#,
            ))
            .mount(&server)
            .await;

        let text = caller(&server, "anthropic").call("extract").await.unwrap();
        assert_eq!(text, r#"{"description":"d"}"#);
    }

    #[tokio::test]
    async fn the_ollama_request_is_unauthenticated_and_non_streaming() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .and(body_json_string(
                r#"{"model":"llama3.2","messages":[{"role":"user","content":"extract"}],"stream":false,"format":"json"}"#,
            ))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"message":{"content":"{}"},"done":true}"#),
            )
            .mount(&server)
            .await;

        let text = caller(&server, "ollama").call("extract").await.unwrap();
        assert_eq!(text, "{}");
    }

    #[tokio::test]
    async fn a_provider_error_body_is_surfaced_rather_than_read_as_content() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"error":{"message":"model not found"}}"#),
            )
            .mount(&server)
            .await;

        let err = caller(&server, "openai").call("extract").await.unwrap_err();
        assert!(format!("{err}").contains("model not found"), "got: {err}");
    }

    #[tokio::test]
    async fn a_rate_limit_is_retried_once_and_then_succeeds() {
        let server = MockServer::start().await;
        // Mounted most-specific-first: wiremock matches in mount order, so the
        // one-shot 429 answers the first call and the success answers the retry.
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(429).set_body_string("slow down"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"choices":[{"message":{"content":"ok"}}]}"#),
            )
            .mount(&server)
            .await;

        let text = caller(&server, "openai").call("extract").await.unwrap();
        assert_eq!(text, "ok");
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_permanent_failure_is_not_retried() {
        // Retrying a 401 just spends another round trip to be told the same
        // thing, and doubles the delay before the user sees the real problem.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(401).set_body_string("bad key"))
            .mount(&server)
            .await;

        let err = caller(&server, "openai").call("extract").await.unwrap_err();

        assert!(format!("{err}").contains("401"), "got: {err}");
        assert!(format!("{err}").contains("bad key"), "got: {err}");
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "a 401 must not be retried",
        );
    }

    #[tokio::test]
    async fn a_response_with_no_choices_is_an_error_not_an_empty_skill() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"choices":[]}"#))
            .mount(&server)
            .await;

        assert!(caller(&server, "openai").call("extract").await.is_err());
    }
}

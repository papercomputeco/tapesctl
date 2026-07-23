//! Launch recipes.
//!
//! Ported from paper's `start.rs` (per-agent env/config) and the Go
//! opencode/codex config injection during Track 1. A launch recipe knows how to
//! run one harness so its LLM traffic is directed through a capture proxy
//! endpoint — parameterized by the proxy endpoint, never Paper-specific.

/// How to launch a specific harness under a capture proxy.
pub trait LaunchRecipe {
    /// The harness identifier this recipe handles (e.g. `"claude"`, `"codex"`).
    fn harness(&self) -> &str;

    /// Environment variables to inject so the harness routes its LLM traffic
    /// through `proxy_endpoint`. Returned as `(key, value)` pairs.
    fn env(&self, proxy_endpoint: &str) -> Vec<(String, String)>;
}

//! Concrete `orchestrator::Agent` implementations.
//!
//! Each module wraps one external CLI/runtime: [`ClaudeCodeAgent`] for the
//! `claude` CLI, `CodexAgent` for `codex`, `OllamaAgent` for local models,
//! etc. Adding a new backend is a one-line `pub mod` declaration here plus a
//! sibling module file.

#![deny(missing_docs)]

pub mod claude_code;
pub mod codex;
pub mod foundation_models;
#[cfg(not(target_family = "wasm"))]
#[doc(hidden)]
pub mod gateway;
pub mod ollama;
pub mod remote;

pub use claude_code::{ClaudeCodeAgent, ClaudeModel};
pub use codex::{CodexAgent, ReasoningEffort, ServiceTier};
pub use foundation_models::FoundationModelsAgent;
pub use ollama::OllamaAgent;
pub use remote::RemoteAgent;

/// Predicate matching env var names that could leak Linear credentials into
/// an agent subprocess. PDX-112 §10.5: the daemon-mediated `linear_graphql`
/// tool is the only sanctioned path to Linear, so any inherited
/// `LINEAR_*` env var is scrubbed before the agent CLI is exec'd.
///
/// Exposed for unit tests that audit the env-leak invariant directly.
pub fn is_linear_secret_env(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper == "LINEAR_API_KEY"
        || upper == "LINEAR_API_TOKEN"
        || upper == "LINEAR_TOKEN"
        || upper.starts_with("LINEAR_")
}

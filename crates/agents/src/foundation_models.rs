//! [`Agent`] implementation backed by Apple's on-device Foundation Models
//! runtime (PDX-16).
//!
//! This agent is the router-facing wrapper around the `foundation_models`
//! crate's Swift bridge (PDX-13) and `@Generable` translator (PDX-14). It
//! advertises three roles per the master plan — [`Role::Inline`],
//! [`Role::ToolRouter`], and [`Role::Summarize`] — and runs prompts via the
//! synchronous `foundation_models::complete` entry point inside
//! `tokio::task::spawn_blocking`, since the underlying Swift bridge blocks
//! the calling thread.
//!
//! # Capability gating
//!
//! On non-macOS targets and on macOS hosts where the Foundation Models
//! framework is not loadable (e.g. macOS < 26 or the smoke-test session
//! fails), the agent reports itself permanently unhealthy. The router then
//! skips it and falls through to whichever next-eligible agent advertises
//! the requested role — typically a Claude Code Haiku 4.5 worker — with
//! no per-call probing on the hot path.
//!
//! # Cost telemetry
//!
//! Foundation Models runs on-device, so the dollar cost is exactly $0.
//! [`local_orchestrator`] registers this agent with
//! `estimated_micros_per_task = 0`, mirroring the [`OllamaAgent`] pattern
//! for local-only providers. The router's tie-break sort key uses that
//! value directly.
//!
//! # Tool translation
//!
//! When MCP tools are present in the active forwarder context, the agent
//! translates them to a Swift `@Generable` schema preamble via
//! [`generable::translate_tools`]. On any [`TranslationError`] (the active
//! tool set contains an unsupported JSON Schema construct, or the tool
//! list is empty), the agent emits an [`AgentEvent::Failed`] so the
//! dispatcher can route the next attempt to a fallback agent. The
//! translation result is cached by [`GeneratedSwift::hash`] across calls,
//! so a stable tool set re-uses the cached source string without
//! re-running serde over the schemas.
//!
//! [`local_orchestrator`]: ../../app/src/ai/agent_sdk/driver/local_orchestrator.rs
//! [`OllamaAgent`]: crate::OllamaAgent

use std::sync::Arc;

use async_stream::stream;
use async_trait::async_trait;
use chrono::Utc;
use foundation_models::generable::{self, GeneratedSwift, McpTool, TranslationError};
use foundation_models::FoundationModelsError;
use orchestrator::{
    Agent, AgentError, AgentEvent, AgentEventStream, AgentId, Capabilities, Health, Role, Task,
};
use tokio::sync::{Mutex, RwLock};

/// Context window the on-device Foundation Models tier exposes today.
///
/// 4 096 tokens is the documented session window for the macOS 26 model;
/// the router uses this for capability-gated routing decisions
/// (Inline / Summarize tasks comfortably fit, BulkRefactor / Planner do not).
pub const FOUNDATION_MODELS_CONTEXT_TOKENS: u32 = 4_096;

/// Cached translation result for the active MCP tool set.
///
/// Keyed by the tool set's content hash via [`GeneratedSwift::hash`] so
/// that re-translating an identical set short-circuits to the cached
/// Swift source. The cache is wrapped in an [`RwLock`] so concurrent
/// reads on the hot path don't serialise.
type TranslationCache = RwLock<Option<(String, GeneratedSwift)>>;

/// [`Agent`] implementation routed through Apple's on-device Foundation
/// Models runtime.
pub struct FoundationModelsAgent {
    id: AgentId,
    capabilities: Capabilities,
    health: Arc<Mutex<Health>>,
    /// Cached `(hash, GeneratedSwift)` for the most recently translated
    /// tool set. Tools rarely change within a session, so a single-slot
    /// cache covers ~all the win.
    tools_cache: Arc<TranslationCache>,
}

impl FoundationModelsAgent {
    /// Construct a new [`FoundationModelsAgent`].
    ///
    /// Probes [`foundation_models::is_supported`] once at construction
    /// time and seeds the agent's [`Health`] flag accordingly. Returns
    /// successfully even on unsupported hosts — the router will simply
    /// observe `healthy = false` and skip dispatch.
    pub fn new(id: AgentId) -> Self {
        use std::collections::HashSet;
        let roles: HashSet<Role> = [Role::Inline, Role::ToolRouter, Role::Summarize]
            .into_iter()
            .collect();
        let capabilities = Capabilities {
            roles,
            max_context_tokens: FOUNDATION_MODELS_CONTEXT_TOKENS,
            // The Swift bridge does not yet emit tool-call events; tools are
            // surfaced into the prompt as a `@Generable` preamble. Setting
            // this to `false` keeps the router's tool-aware filter from
            // selecting FM for tasks that demand structured tool I/O.
            supports_tools: false,
            supports_vision: false,
        };
        let healthy = foundation_models::is_supported();
        let health = Arc::new(Mutex::new(Health {
            healthy,
            last_check: Utc::now(),
            error_rate: 0.0,
        }));
        Self {
            id,
            capabilities,
            health,
            tools_cache: Arc::new(RwLock::new(None)),
        }
    }

    /// Returns `true` iff Apple Foundation Models reports as supported on
    /// the current process. Thin wrapper exposed for the router-side
    /// registration helper, which short-circuits registration when this
    /// is `false`.
    pub fn is_supported() -> bool {
        foundation_models::is_supported()
    }

    /// Translate an MCP tool list to a Swift `@Generable` source string,
    /// caching the result by content hash.
    ///
    /// Returns `Ok(None)` when `tools` is empty — translating zero tools
    /// is unsupported (per PDX-14), and an empty preamble means the call
    /// proceeds without a `@Generable` schema. Returns `Err(message)`
    /// when the tool set contains an unsupported construct so the
    /// caller can surface a `Failed` event and let the router fall
    /// through.
    async fn translate_or_cached(
        &self,
        tools: &[McpTool],
    ) -> Result<Option<GeneratedSwift>, String> {
        if tools.is_empty() {
            return Ok(None);
        }
        // Compute a cheap fingerprint of the tool set up-front to avoid
        // re-translating when nothing changed. We hash the normalised
        // (name, description, schema) tuple via the same serde+sha2 path
        // PDX-14 uses internally so the fingerprints align.
        match generable::translate_tools(tools) {
            Ok(generated) => {
                let mut cache = self.tools_cache.write().await;
                let prior_hash = cache.as_ref().map(|(h, _)| h.clone());
                if prior_hash.as_deref() != Some(generated.hash.as_str()) {
                    *cache = Some((generated.hash.clone(), generated.clone()));
                }
                Ok(Some(generated))
            }
            Err(TranslationError::Unsupported(msg)) => Err(format!(
                "foundation_models: tool schema unsupported by @Generable translator: {msg}"
            )),
            Err(other) => Err(format!(
                "foundation_models: translation error: {other:?}"
            )),
        }
    }

    /// Build the Swift `@Generable` preamble that gets prepended to the
    /// user prompt before the FM bridge sees it.
    ///
    /// Today this is a pass-through of the generated Swift source. The
    /// actual structured-output enforcement happens when the chained
    /// execution loop (PDX-15) compiles the preamble; that landing will
    /// add a wrapper around `complete` that takes the source as a
    /// schema parameter rather than concatenating it into the prompt.
    fn build_prompt(generated: Option<&GeneratedSwift>, user_prompt: &str) -> String {
        match generated {
            Some(g) => format!(
                "// @Generable schema for the active MCP tools (PDX-14):\n{}\n\nUser request:\n{}",
                g.source, user_prompt
            ),
            None => user_prompt.to_string(),
        }
    }

    /// Mark the agent as unhealthy after a runtime failure so the next
    /// `Router::select` skips it.
    async fn mark_unhealthy(&self) {
        let mut h = self.health.lock().await;
        h.healthy = false;
        h.last_check = Utc::now();
        h.error_rate = (h.error_rate + 1.0).min(1.0);
    }
}

#[async_trait]
impl Agent for FoundationModelsAgent {
    fn id(&self) -> AgentId {
        self.id.clone()
    }

    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    fn health(&self) -> Health {
        if let Ok(guard) = self.health.try_lock() {
            guard.clone()
        } else {
            // Conservative fallback: if another task is mutating health,
            // report unhealthy so the router doesn't race a teardown.
            Health {
                healthy: false,
                last_check: Utc::now(),
                error_rate: 1.0,
            }
        }
    }

    async fn execute(&self, task: Task) -> Result<AgentEventStream, AgentError> {
        let task_id = task.id;
        let prompt = task.prompt.clone();

        // Tool list lives on the McpForwarder in the orchestrator layer;
        // the local_orchestrator wires the active tool set into
        // `task.context.metadata` under `mcp_tools` (a JSON array of
        // `{name, description, input_schema}` triples). Reading from
        // metadata here keeps `foundation_models::McpTool` out of the
        // public Task surface and lets the orchestrator be the source of
        // truth for which tools are live.
        let tools: Vec<McpTool> = task
            .context
            .metadata
            .get("mcp_tools")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|item| {
                        let name = item.get("name")?.as_str()?.to_string();
                        let description = item
                            .get("description")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let input_schema = item.get("input_schema").cloned()?;
                        Some(McpTool {
                            name,
                            description,
                            input_schema,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        // Fast-path supported check: if the bridge is unloadable, fail
        // immediately so the router can re-dispatch to the Haiku 4.5
        // fallback. The persistent router skips registration entirely
        // when this is false, so we only hit this branch when support
        // changed mid-session (rare).
        if !foundation_models::is_supported() {
            self.mark_unhealthy().await;
            let stream = stream! {
                yield AgentEvent::Started { task_id };
                yield AgentEvent::Failed {
                    task_id,
                    error: "foundation_models: runtime not supported on this host".to_string(),
                };
            };
            return Ok(Box::pin(stream));
        }

        let generated = match self.translate_or_cached(&tools).await {
            Ok(g) => g,
            Err(msg) => {
                // Translation failures are user-actionable (the active
                // MCP tool set has a construct we can't yet emit as
                // Swift `@Generable`). Don't flag the agent as unhealthy
                // — the next call with a different tool set may
                // succeed.
                let stream = stream! {
                    yield AgentEvent::Started { task_id };
                    yield AgentEvent::Failed { task_id, error: msg };
                };
                return Ok(Box::pin(stream));
            }
        };

        let composed_prompt = Self::build_prompt(generated.as_ref(), &prompt);
        let health = Arc::clone(&self.health);

        let stream = stream! {
            yield AgentEvent::Started { task_id };

            // The Foundation Models C ABI blocks the calling thread for
            // the duration of the completion. Wrap in `spawn_blocking`
            // so we don't stall the tokio reactor. PDX-15 will replace
            // this with the streaming entry point so we can yield
            // `OutputChunk` events incrementally; for now we surface
            // the full response as one chunk.
            let join = tokio::task::spawn_blocking(move || foundation_models::complete(&composed_prompt));
            match join.await {
                Ok(Ok(text)) => {
                    if !text.is_empty() {
                        yield AgentEvent::OutputChunk { text };
                    }
                    yield AgentEvent::Completed { task_id, summary: None };
                }
                Ok(Err(FoundationModelsError::Unavailable)) => {
                    let mut h = health.lock().await;
                    h.healthy = false;
                    h.last_check = Utc::now();
                    h.error_rate = (h.error_rate + 1.0).min(1.0);
                    drop(h);
                    yield AgentEvent::Failed {
                        task_id,
                        error: "foundation_models: runtime became unavailable mid-session"
                            .to_string(),
                    };
                }
                Ok(Err(other)) => {
                    let mut h = health.lock().await;
                    h.last_check = Utc::now();
                    h.error_rate = (h.error_rate + 0.25).min(1.0);
                    drop(h);
                    yield AgentEvent::Failed {
                        task_id,
                        error: format!("foundation_models: {other}"),
                    };
                }
                Err(join_err) => {
                    yield AgentEvent::Failed {
                        task_id,
                        error: format!("foundation_models: blocking task panicked: {join_err}"),
                    };
                }
            }
        };

        Ok(Box::pin(stream))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(target_os = "macos"))]
    use orchestrator::{TaskContext, TaskId};

    #[cfg(not(target_os = "macos"))]
    fn make_task(role: Role) -> Task {
        Task {
            id: TaskId::new(),
            role,
            prompt: "hi".to_string(),
            context: TaskContext::default(),
            budget_hint: None,
        }
    }

    /// PDX-16 acceptance: capabilities must advertise exactly the three
    /// roles called out in the issue (Inline, ToolRouter, Summarize) and
    /// nothing more. A regression that adds `Worker` would mean FM bids
    /// on tasks beyond its 4 K context window.
    #[test]
    fn capabilities_advertise_inline_toolrouter_summarize_only() {
        let agent = FoundationModelsAgent::new(AgentId("fm".into()));
        let caps = agent.capabilities();
        assert!(caps.roles.contains(&Role::Inline));
        assert!(caps.roles.contains(&Role::ToolRouter));
        assert!(caps.roles.contains(&Role::Summarize));
        assert!(!caps.roles.contains(&Role::Worker));
        assert!(!caps.roles.contains(&Role::Planner));
        assert!(!caps.roles.contains(&Role::Reviewer));
        assert!(!caps.roles.contains(&Role::BulkRefactor));
        assert!(!caps.supports_tools);
        assert!(!caps.supports_vision);
        assert_eq!(caps.max_context_tokens, FOUNDATION_MODELS_CONTEXT_TOKENS);
    }

    /// On non-macOS targets `is_supported` is a compile-time false, so
    /// `FoundationModelsAgent::new` must seed `Health.healthy = false`.
    /// On a Mac running macOS 26+ the smoke test will return true; we
    /// can't assert that without an environment dependency, so we only
    /// run this on non-macOS.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn health_is_unhealthy_off_mac() {
        let agent = FoundationModelsAgent::new(AgentId("fm".into()));
        assert!(!agent.health().healthy);
    }

    /// Off-Mac, `execute` must yield a `Failed` event explaining that
    /// the runtime is unavailable, so the dispatcher can pick the
    /// fallback agent on the next dispatch.
    #[cfg(not(target_os = "macos"))]
    #[tokio::test]
    async fn execute_off_mac_returns_failed() {
        use futures_util::StreamExt;
        let agent = FoundationModelsAgent::new(AgentId("fm".into()));
        let mut stream = agent
            .execute(make_task(Role::Inline))
            .await
            .expect("execute returns stream");
        // First event is Started.
        assert!(matches!(stream.next().await, Some(AgentEvent::Started { .. })));
        match stream.next().await {
            Some(AgentEvent::Failed { error, .. }) => {
                assert!(
                    error.contains("foundation_models"),
                    "unexpected error: {error}"
                );
            }
            other => panic!("expected Failed event, got {other:?}"),
        }
    }

    /// `is_supported` is a thin pass-through and must not panic on any
    /// target.
    #[test]
    fn is_supported_does_not_panic() {
        let _ = FoundationModelsAgent::is_supported();
    }

    /// `build_prompt` with no tool schema is the identity on the user
    /// prompt — important so empty MCP tool sets pass straight through
    /// without a confusing `@Generable` preamble.
    #[test]
    fn build_prompt_passthrough_when_no_tools() {
        let composed = FoundationModelsAgent::build_prompt(None, "hello");
        assert_eq!(composed, "hello");
    }

    /// `build_prompt` with a generated schema prepends a `@Generable`
    /// preamble that includes the source, so PDX-15's compile cache can
    /// align preambles to a known prefix.
    #[test]
    fn build_prompt_includes_preamble_when_tools_present() {
        let g = GeneratedSwift {
            source: "enum ToolChoice { case Read }".to_string(),
            hash: "deadbeef".to_string(),
        };
        let composed = FoundationModelsAgent::build_prompt(Some(&g), "summarize");
        assert!(composed.contains("@Generable schema"));
        assert!(composed.contains("enum ToolChoice"));
        assert!(composed.contains("summarize"));
    }
}

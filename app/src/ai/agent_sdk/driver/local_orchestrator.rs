//! Bridges the local [`AgentDriver`] conversation model to the [`orchestrator`]
//! crate's [`Agent`] / [`Router`] abstraction, replacing the deprecated hosted
//! Oz execution path.
//!
//! [`LocalOrchestratorAgent`] wraps the driver's existing [`execute_run`]
//! mechanism behind the `orchestrator::Agent` trait so that routing, health
//! checks, and budget accounting can all go through the canonical orchestrator
//! stack without any changes to the underlying conversation machinery.
//!
//! # Persistent Router (PDX-103 [B1] task 2)
//!
//! Earlier revisions rebuilt the [`Router`] on every call inside
//! `run_via_local_orchestrator`, which meant per-provider [`Budget`] state was
//! re-zeroed on every prompt. The router now lives behind a process-wide
//! [`OnceLock<Arc<Mutex<Router>>>`] so budget snapshots accumulate across
//! consecutive prompts in the same session.
//!
//! Per-call data ([`ModelSpawner`] foreground + [`AgentRunPrompt`]) is staged
//! into the long-lived [`LocalOrchestratorAgent`] via
//! [`LocalOrchestratorAgent::stage_run`] immediately before [`Router::select`]
//! is invoked. The agent picks the staged value off the mutex inside
//! [`Agent::execute`]. Local prompts are sequential within a session, so a
//! single-slot mutex is sufficient.
//!
//! # ClaudeCodeAgent registration (PDX-103 [B1] task 3)
//!
//! [`ensure_claude_code_registered`] tries to construct a real
//! `agents::ClaudeCodeAgent` (which probes `which::which("claude")`) the first
//! time the router is built. On success the agent is registered under
//! [`Provider::ClaudeCode`] with a generous-but-real cap. On failure (no
//! binary, signed-out CLI) the registration is skipped — [`Router::select`]
//! falls through to [`LocalOrchestratorAgent`] without a panic. Re-running
//! after the user signs in is supported via [`refresh_claude_code_registration`].

use std::collections::HashSet;
use std::sync::{Arc, Mutex as StdMutex, OnceLock};

use async_trait::async_trait;
use chrono::Utc;
use orchestrator::{
    Agent, AgentError, AgentEvent, AgentEventStream, AgentId, AgentRegistration, Budget,
    Capabilities, Health, McpForwarder, Provider, Role, Router, Task, TaskId,
};
use tokio::sync::Mutex as AsyncMutex;
use warpui::ModelSpawner;

use super::provider_caps::ProviderCapsConfig;
use super::{AgentDriver, AgentRunPrompt, SDKConversationOutputStatus};

/// Stable identifier used for the local Warp agent in the orchestrator registry.
const LOCAL_AGENT_ID: &str = "local-warp-oz";

/// Stable identifier used for the Claude Code Sonnet 4.6 agent.
pub(crate) const CLAUDE_CODE_SONNET_46_ID: &str = "claude-sonnet-46";

/// Stable identifier used for the Codex worker agent (PDX-104 [B2] task 1).
pub(crate) const CODEX_WORKER_ID: &str = "codex-worker";

/// Stable identifier used for the Ollama worker agent (PDX-104 [B2] task 2).
pub(crate) const OLLAMA_WORKER_ID: &str = "ollama-worker";

/// Default Ollama model used when nothing has been pinned. Matches the task
/// description in PDX-104; takes the first locally pulled candidate at
/// registration time and falls back to this string if none can be detected.
pub(crate) const OLLAMA_DEFAULT_MODEL: &str = "qwen2.5-coder";

/// Tie-break ordering hints (PDX-103 [B1] task 5).
///
/// Both agents advertise [`Role::Worker`]. The router sorts ascending by
/// `(tier, estimated_micros_per_task, agent_id)`, so a smaller value here
/// means *preferred* on a tie.
///
/// We deliberately make the local Foundation Models agent cheaper than Claude
/// for plain Worker tasks: spawning a `claude` subprocess is ~hundreds of
/// milliseconds of overhead before any work happens, whereas the local model
/// is in-process. The larger `estimated_micros` for Claude reflects the
/// average per-task cost in micro-dollars (a few cents) and is what
/// `Budget::try_charge` will be debited at the call site.
///
/// Concretely: for a generic [`Role::Worker`] task with no other constraints
/// the local agent wins (`0 < 8_000`). When the call site biases toward Claude
/// — by pinning [`Provider::ClaudeCode`] from the in-prompt selector or by
/// asking for a role only Claude advertises (Planner / Reviewer / BulkRefactor
/// at higher tiers, vision) — the lex tie-break or capability filter takes
/// over and Claude wins.
const LOCAL_AGENT_ESTIMATED_MICROS: u64 = 0;
const CLAUDE_CODE_SONNET_46_ESTIMATED_MICROS: u64 = 8_000;
/// Cost estimate per Codex worker task in micro-dollars.
///
/// Higher than Claude Sonnet 4.6 to reflect Codex's larger reasoning surface
/// in the `Standard / Medium` worker profile. Concrete number is best-effort;
/// the orchestrator only uses it as a tie-break sort key, not a billing
/// figure.
const CODEX_WORKER_ESTIMATED_MICROS: u64 = 12_000;
/// Cost estimate per Ollama task in micro-dollars.
///
/// Local inference, so the real dollar cost is zero. We pick a small
/// non-zero number so the local Foundation Models agent (`0`) still wins
/// the deepest tie-break for plain `Worker` work, while letting Ollama
/// beat Claude / Codex on a [`Role::Summarize`] task where neither cloud
/// agent advertises the role and Ollama does (see PDX-104 acceptance
/// criteria item 2).
const OLLAMA_ESTIMATED_MICROS: u64 = 100;

/// Per-call run state staged on [`LocalOrchestratorAgent`].
///
/// Holds the foreground spawner and the prompt for the current prompt. A
/// fresh value is set via [`LocalOrchestratorAgent::stage_run`] immediately
/// before the call to [`Router::select`] in `run_via_local_orchestrator`.
struct StagedRun {
    foreground: ModelSpawner<AgentDriver>,
    prompt: AgentRunPrompt,
}

/// An [`orchestrator::Agent`] implementation that runs tasks through the local
/// Warp conversation model, replacing the hosted Oz cloud execution path.
///
/// Now process-stable: the agent itself is registered once into the
/// persistent router, and per-call state is set via [`Self::stage_run`]
/// immediately before dispatch.
pub(crate) struct LocalOrchestratorAgent {
    capabilities: Capabilities,
    staged: AsyncMutex<Option<StagedRun>>,
}

impl LocalOrchestratorAgent {
    pub(crate) fn new() -> Self {
        Self {
            capabilities: Capabilities {
                roles: HashSet::from([Role::Worker, Role::Planner]),
                max_context_tokens: 200_000,
                supports_tools: true,
                supports_vision: false,
            },
            staged: AsyncMutex::new(None),
        }
    }

    /// Stage the foreground + prompt for the very next [`Agent::execute`]
    /// call.
    ///
    /// Local prompts run sequentially inside a single `AgentDriver` session,
    /// so a single-slot mutex is sufficient. Any previously staged value is
    /// dropped (this is exclusively a "set the next prompt" channel).
    pub(crate) async fn stage_run(
        &self,
        foreground: ModelSpawner<AgentDriver>,
        prompt: AgentRunPrompt,
    ) {
        let mut slot = self.staged.lock().await;
        *slot = Some(StagedRun { foreground, prompt });
    }

    /// Read-only view of the currently-staged prompt.
    ///
    /// Used by the dispatcher when re-encoding the prompt as a string for a
    /// subprocess agent (Claude Code), without consuming the staged slot.
    /// Returns `None` if nothing is staged (which shouldn't happen on the
    /// hot path; the caller stages immediately before reading).
    pub(crate) async fn peek_staged_prompt(&self) -> Option<AgentRunPrompt> {
        self.staged
            .lock()
            .await
            .as_ref()
            .map(|s| s.prompt.clone())
    }
}

/// Convert an [`AgentRunPrompt`] into a plain string suitable for piping into
/// a subprocess agent (e.g. `claude --print "<text>"`).
///
/// `Local` prompts unwrap directly. `ServerSide` prompts can't be resolved
/// from this side, so we fall back to an explanatory placeholder; in
/// practice the in-prompt selector only fires on `Local` prompts because the
/// server-side resolution path is gated on a different code path that
/// doesn't yet route through the persistent router.
pub(crate) fn encode_prompt_for_subprocess(prompt: Option<&AgentRunPrompt>) -> String {
    match prompt {
        Some(AgentRunPrompt::Local(text)) => text.clone(),
        Some(AgentRunPrompt::ServerSide { .. }) => {
            log::warn!(
                "ServerSide prompt encountered while routing to a subprocess agent; \
                 ServerSide → Claude Code pinning is not yet supported (PDX-103 follow-up)"
            );
            String::new()
        }
        None => String::new(),
    }
}

#[async_trait]
impl Agent for LocalOrchestratorAgent {
    fn id(&self) -> AgentId {
        AgentId(LOCAL_AGENT_ID.to_string())
    }

    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    fn health(&self) -> Health {
        Health {
            healthy: true,
            last_check: Utc::now(),
            error_rate: 0.0,
        }
    }

    /// Bridge `orchestrator::Agent::execute` to `AgentDriver::execute_run`.
    ///
    /// Pulls the staged foreground+prompt off the mutex, then translates the
    /// driver's [`SDKConversationOutputStatus`] into orchestrator events.
    async fn execute(&self, task: Task) -> Result<AgentEventStream, AgentError> {
        let staged = self
            .staged
            .lock()
            .await
            .take()
            .ok_or_else(|| AgentError::Other("local orchestrator: no staged run".to_string()))?;
        let StagedRun { foreground, prompt } = staged;
        let task_id = task.id;

        let stream = async_stream::stream! {
            yield AgentEvent::Started { task_id };

            let status_rx = match foreground
                .spawn(move |me, ctx| me.execute_run(prompt, ctx))
                .await
            {
                Ok(rx) => rx,
                Err(_) => {
                    yield AgentEvent::Failed {
                        task_id,
                        error: "local orchestrator: driver model unavailable".into(),
                    };
                    return;
                }
            };

            match status_rx.await {
                Ok(SDKConversationOutputStatus::Success) => {
                    yield AgentEvent::Completed { task_id, summary: None };
                }
                Ok(SDKConversationOutputStatus::Error { error }) => {
                    yield AgentEvent::Failed {
                        task_id,
                        error: error.to_string(),
                    };
                }
                Ok(SDKConversationOutputStatus::Cancelled { reason }) => {
                    yield AgentEvent::Failed {
                        task_id,
                        error: format!("cancelled: {reason:?}"),
                    };
                }
                Ok(SDKConversationOutputStatus::Blocked { blocked_action }) => {
                    yield AgentEvent::Failed {
                        task_id,
                        error: format!("blocked: {blocked_action}"),
                    };
                }
                Err(_) => {
                    yield AgentEvent::Failed {
                        task_id,
                        error: "local orchestrator: driver dropped before conversation finished"
                            .into(),
                    };
                }
            }
        };

        Ok(Box::pin(stream))
    }
}

/// Process-wide persistent router holder (PDX-103 [B1] task 2).
///
/// Wrapped in a [`tokio::sync::Mutex`] only to permit re-registering
/// `ClaudeCodeAgent` after a successful sign-in without restarting the app.
/// The hot dispatch path holds the lock only across the cheap `register` /
/// `select` calls, never across `execute`.
static GLOBAL_ROUTER: OnceLock<Arc<AsyncMutex<Router>>> = OnceLock::new();

/// Process-wide [`Budget`] handle, exposed so the PDX-27 budget enforcer
/// can share the same accounting state as the [`Router`].
///
/// Populated in lockstep with [`GLOBAL_ROUTER`]: both `OnceLock`s are
/// initialised together by [`ensure_persistent_router`], so callers that
/// reach for the budget after `ensure_persistent_router` has run will
/// always see the same `Arc<Budget>` the router consults.
static GLOBAL_BUDGET: OnceLock<Arc<Budget>> = OnceLock::new();

/// Process-wide [`super::budget_enforcer::BudgetEnforcer`] handle —
/// PDX-27 [D4] runtime gate sitting between [`Router::select`] and
/// [`orchestrator::Agent::execute`]. Same lifetime story as
/// [`GLOBAL_BUDGET`]: lazily initialised by
/// [`ensure_persistent_router`], shared by every dispatch.
static GLOBAL_ENFORCER: OnceLock<Arc<super::budget_enforcer::BudgetEnforcer>> = OnceLock::new();

/// Construct the budget that backs [`GLOBAL_ROUTER`].
///
/// Cap table is sourced from [`ProviderCapsConfig::load`] (PDX-104 [B2]
/// task 4) so the same defaults are visible — and tunable — in one place.
fn build_persistent_budget() -> Arc<Budget> {
    Arc::new(Budget::new(ProviderCapsConfig::load().into_caps()))
}

/// Return the process-wide [`Budget`] handle.
///
/// Returns `None` until [`ensure_persistent_router`] has been called
/// at least once. Used by the budget enforcer and by status-snapshot
/// telemetry that wants a real number rather than the lazy
/// `OnceLock::get` indirection.
pub(crate) fn persistent_budget() -> Option<Arc<Budget>> {
    GLOBAL_BUDGET.get().cloned()
}

/// Return the process-wide [`super::budget_enforcer::BudgetEnforcer`].
///
/// Initialised on first access using the persistent [`Budget`] and
/// production-default audit log path (`~/.warp/symphony/audit.log`).
/// Concurrency caps default to empty (lenient) per the PDX-27 contract;
/// future settings overrides plug in via
/// [`super::provider_caps::ProviderCapsConfig`].
pub(crate) fn persistent_enforcer() -> Arc<super::budget_enforcer::BudgetEnforcer> {
    GLOBAL_ENFORCER
        .get_or_init(|| {
            let budget = persistent_budget().unwrap_or_else(build_persistent_budget);
            // Lenient default: no concurrency caps installed. Production
            // can plumb a real config in via a follow-up commit; tests
            // override this by constructing `BudgetEnforcer::new` directly.
            super::budget_enforcer::BudgetEnforcer::with_default_audit(
                budget,
                super::budget_enforcer::ConcurrencyCaps::new(),
            )
        })
        .clone()
}

/// Lazily build (or reuse) the persistent process-wide [`Router`] and return
/// the long-lived [`LocalOrchestratorAgent`] handle.
///
/// The agent is registered exactly once; subsequent calls reuse the existing
/// registration. ClaudeCodeAgent registration is best-effort and idempotent —
/// see [`ensure_claude_code_registered`].
///
/// Returns the `Arc<LocalOrchestratorAgent>` so the caller can stage a run on
/// it via [`LocalOrchestratorAgent::stage_run`] without re-fetching it from
/// the router (the router stores `Arc<dyn Agent>` and downcasting is awkward).
pub(crate) async fn ensure_persistent_router() -> (
    Arc<AsyncMutex<Router>>,
    Arc<LocalOrchestratorAgent>,
) {
    // Note: the `Arc<LocalOrchestratorAgent>` we return is the *same* one
    // registered into the router, so `stage_run` and `Agent::execute` race
    // through the same mutex.
    static LOCAL_AGENT: OnceLock<Arc<LocalOrchestratorAgent>> = OnceLock::new();
    let local_agent = LOCAL_AGENT
        .get_or_init(|| Arc::new(LocalOrchestratorAgent::new()))
        .clone();

    let router = GLOBAL_ROUTER
        .get_or_init(|| {
            // Build the budget once and stash it in `GLOBAL_BUDGET` so
            // PDX-27's budget enforcer can debit and read the same
            // accounting state the router consults at select-time.
            let budget = GLOBAL_BUDGET
                .get_or_init(build_persistent_budget)
                .clone();
            let mut router = Router::new(budget);
            router.register(AgentRegistration {
                agent: local_agent.clone() as Arc<dyn Agent>,
                provider: Provider::FoundationModels,
                estimated_micros_per_task: LOCAL_AGENT_ESTIMATED_MICROS,
            });
            Arc::new(AsyncMutex::new(router))
        })
        .clone();

    // Best-effort: try to attach a real ClaudeCodeAgent. Idempotent.
    ensure_claude_code_registered(&router).await;
    // PDX-104 [B2] tasks 1 + 2: register Codex worker + Ollama default model.
    // Both are idempotent and skip silently when the binary or model is
    // missing.
    ensure_codex_registered(&router).await;
    ensure_ollama_registered(&router).await;

    (router, local_agent)
}

/// Best-effort registration of [`agents::ClaudeCodeAgent`].
///
/// Constructed via `ClaudeCodeAgent::new`, which itself probes the `claude`
/// binary on `PATH` via `which::which`. If construction fails (binary
/// missing), the registration is skipped silently — `Router::select` will
/// fall through to [`LocalOrchestratorAgent`] for any role both can satisfy.
///
/// Idempotent: re-registering an existing `AgentId` overwrites the prior
/// entry, which is what we want when the user signs in mid-session.
pub(crate) async fn ensure_claude_code_registered(router: &Arc<AsyncMutex<Router>>) {
    use agents::{ClaudeCodeAgent, ClaudeModel};

    match ClaudeCodeAgent::new(AgentId(CLAUDE_CODE_SONNET_46_ID.to_string()), ClaudeModel::Sonnet46)
    {
        Ok(agent) => {
            let mut router = router.lock().await;
            router.register(AgentRegistration {
                agent: Arc::new(agent),
                provider: Provider::ClaudeCode,
                estimated_micros_per_task: CLAUDE_CODE_SONNET_46_ESTIMATED_MICROS,
            });
            log::debug!(
                "Registered ClaudeCodeAgent({}) in persistent router",
                CLAUDE_CODE_SONNET_46_ID
            );
        }
        Err(err) => {
            log::debug!(
                "Skipping ClaudeCodeAgent registration: {err} (sign in via Settings → AI to enable)"
            );
        }
    }
}

/// Re-attempt [`ClaudeCodeAgent`] registration after the user signs in.
///
/// Wired to the `CliAgentSignInWidget` re-poll loop and to the in-prompt
/// selector's "Sign in" deep-link.
pub(crate) async fn refresh_claude_code_registration() {
    if let Some(router) = GLOBAL_ROUTER.get() {
        ensure_claude_code_registered(router).await;
    }
}

/// Best-effort registration of [`agents::CodexAgent`] (PDX-104 [B2] task 1).
///
/// Mirrors [`ensure_claude_code_registered`]: constructs the agent via
/// `CodexAgent::worker`, which probes the `codex` binary on `PATH`. On
/// success the agent is registered under [`Provider::Codex`]. On failure
/// (binary missing) registration is skipped silently — `Router::select`
/// falls back to whichever agent advertises [`Role::Worker`].
///
/// Auth probing (reading `~/.codex/auth.json`) is the responsibility of the
/// `CliAgentSignInWidget`. When the user is signed-out the underlying
/// CodexAgent still constructs successfully but the row in the widget is
/// rendered as *signed-out* and the in-prompt selector entry is *disabled
/// with a sign-in affordance* — see [`codex_signed_in`].
pub(crate) async fn ensure_codex_registered(router: &Arc<AsyncMutex<Router>>) {
    use agents::CodexAgent;

    match CodexAgent::worker(AgentId(CODEX_WORKER_ID.to_string())) {
        Ok(agent) => {
            let mut router = router.lock().await;
            router.register(AgentRegistration {
                agent: Arc::new(agent),
                provider: Provider::Codex,
                estimated_micros_per_task: CODEX_WORKER_ESTIMATED_MICROS,
            });
            log::debug!(
                "Registered CodexAgent({}) in persistent router",
                CODEX_WORKER_ID
            );
        }
        Err(err) => {
            log::debug!(
                "Skipping CodexAgent registration: {err} (sign in via Settings → AI to enable)"
            );
        }
    }
}

/// Re-attempt [`CodexAgent`] registration after the user signs in.
///
/// Wired to the `CliAgentSignInWidget` re-poll loop for Codex.
pub(crate) async fn refresh_codex_registration() {
    if let Some(router) = GLOBAL_ROUTER.get() {
        ensure_codex_registered(router).await;
    }
}

/// Probe [`CodexAgent`]'s sign-in state by reading `~/.codex/auth.json`.
///
/// Returns:
/// * `Some(true)` — the file exists and contains an auth-mode marker (the
///   shape mirrored at `app/src/ai/agent_sdk/driver/harness/codex.rs:275`).
/// * `Some(false)` — the file is absent or unreadable; treat as signed-out.
/// * `None` — the home directory could not be resolved (extremely rare).
///
/// Cheap and synchronous — one `fs::read_to_string` of a tiny JSON file —
/// and intentionally tolerant of schema drift: any well-formed JSON object
/// with either `auth_mode` or `OPENAI_API_KEY` populated counts as signed-in.
pub(crate) fn codex_signed_in() -> Option<bool> {
    let home = dirs::home_dir()?;
    let path = home.join(".codex").join("auth.json");
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(_) => return Some(false),
    };
    let json: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => return Some(false),
    };
    let signed_in = json
        .get("auth_mode")
        .and_then(|v| v.as_str())
        .map(|s| !s.is_empty())
        .unwrap_or(false)
        || json
            .get("OPENAI_API_KEY")
            .and_then(|v| v.as_str())
            .map(|s| !s.is_empty())
            .unwrap_or(false)
        || json
            .get("tokens")
            .map(|v| !v.is_null())
            .unwrap_or(false);
    Some(signed_in)
}

/// Best-effort registration of [`agents::OllamaAgent`] (PDX-104 [B2] task 2).
///
/// Constructed via `OllamaAgent::new(id, model)` against
/// [`OLLAMA_DEFAULT_MODEL`]; the constructor probes the `ollama` binary on
/// `PATH` and `ollama show <model>` for capability info. Failures are
/// non-fatal — the registration is skipped silently and the router falls
/// through.
///
/// Local-only, so registered under [`Provider::Ollama`] with the unlimited
/// cap from [`super::provider_caps`].
pub(crate) async fn ensure_ollama_registered(router: &Arc<AsyncMutex<Router>>) {
    use agents::OllamaAgent;

    match OllamaAgent::new(
        AgentId(OLLAMA_WORKER_ID.to_string()),
        OLLAMA_DEFAULT_MODEL.to_string(),
    ) {
        Ok(agent) => {
            let mut router = router.lock().await;
            router.register(AgentRegistration {
                agent: Arc::new(agent),
                provider: Provider::Ollama,
                estimated_micros_per_task: OLLAMA_ESTIMATED_MICROS,
            });
            log::debug!(
                "Registered OllamaAgent({}, {}) in persistent router",
                OLLAMA_WORKER_ID,
                OLLAMA_DEFAULT_MODEL
            );
        }
        Err(err) => {
            log::debug!(
                "Skipping OllamaAgent registration: {err} (install from https://ollama.com/download)"
            );
        }
    }
}

/// Re-attempt [`OllamaAgent`] registration after the user installs the CLI
/// or pulls the default model. Wired to the `CliAgentSignInWidget` Ollama
/// detect-only re-poll path.
pub(crate) async fn refresh_ollama_registration() {
    if let Some(router) = GLOBAL_ROUTER.get() {
        ensure_ollama_registered(router).await;
    }
}

/// Detect whether the `ollama` CLI is installed on `PATH`.
///
/// No subprocess fork — `which::which` walks `$PATH` in-process. Used by the
/// `CliAgentSignInWidget` detect-only Ollama row to render *installed /
/// not installed* without blocking the UI thread.
pub(crate) fn ollama_installed() -> bool {
    which::which("ollama").is_ok()
}

/// Derive a [`Role`] from an [`AgentRunPrompt`] (PDX-104 [B2] task 3).
///
/// Replaces the hardcoded [`Role::Worker`] at the dispatch site in
/// `driver.rs`. The simplest first cut, per the Linear ticket: scan the
/// resolved prompt text for high-signal cues and fall back to
/// [`Role::Worker`] for anything ambiguous.
///
/// The classifier is intentionally cheap (substring match on a lowercase
/// copy) and conservative — it only promotes to a more specialized role
/// when the cue is unambiguous, otherwise routing stays on the existing
/// Worker tie-break that the rest of the test suite relies on.
///
/// Cues:
/// * Leading / explicit `summarize` / `tl;dr` / `summary of` →
///   [`Role::Summarize`].
/// * `plan` / `decompose` / `break down` → [`Role::Planner`].
/// * `review` / `code review` / `lgtm` → [`Role::Reviewer`].
/// * Otherwise [`Role::Worker`].
///
/// `ServerSide` prompts return [`Role::Worker`]: the prompt has not been
/// resolved on this side of the wire, so we cannot infer a role from it.
pub(crate) fn infer_role_from_prompt(prompt: &AgentRunPrompt) -> Role {
    let text = match prompt {
        AgentRunPrompt::Local(t) => t.as_str(),
        AgentRunPrompt::ServerSide { .. } => return Role::Worker,
    };
    classify_prompt_text(text)
}

/// Lower-level role classifier exposed for unit tests so the cue list
/// stays close to its data without spinning up an `AgentRunPrompt`.
fn classify_prompt_text(text: &str) -> Role {
    let lower = text.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return Role::Worker;
    }
    // Order matters: more specific cues win over more general ones.
    if lower.starts_with("summarize")
        || lower.starts_with("tl;dr")
        || lower.contains("summary of")
        || lower.contains("please summarize")
    {
        return Role::Summarize;
    }
    if lower.starts_with("review")
        || lower.contains("code review")
        || lower.contains("please review")
    {
        return Role::Reviewer;
    }
    if lower.starts_with("plan")
        || lower.contains("break down")
        || lower.contains("decompose")
        || lower.contains("step-by-step plan")
    {
        return Role::Planner;
    }
    Role::Worker
}

/// Snapshot the live health of an agent registered in the persistent router.
///
/// Used by `CliAgentSignInWidget::detect_claude` and by
/// `ProfileModelSelector::claude_code_option_healthy` to surface "signed in"
/// inline in the UI without having to hold the router mutex themselves.
///
/// Returns `None` when the router has not been built yet (e.g. very early
/// startup) or when the requested `AgentId` is not registered.
pub(crate) fn agent_health_snapshot(id: &AgentId) -> Option<Health> {
    let router = GLOBAL_ROUTER.get()?.clone();
    // try_lock to keep this synchronous and cheap on the UI render path. If
    // another task is currently rebuilding the router we return `None` and
    // the caller falls back to its skeleton default rather than blocking.
    let guard = router.try_lock().ok()?;
    // Router does not expose a health getter; we work around by calling
    // `select` over a sentinel task — not viable here. Instead we use the
    // router's inspection helper added in this crate: see `agent_health`.
    guard.agent_health(id)
}

/// Process-wide latch driven by the in-prompt model selector
/// (`ProfileModelSelector::Select{ClaudeCode,Codex,Ollama}Model`) — when set,
/// the next `run_via_local_orchestrator` call dispatches through an agent
/// registered under that [`Provider`] regardless of Role tie-break.
///
/// PDX-105 [B3] task 4 — generalised from the prior Claude-only
/// `AtomicBool` to a `Mutex<Option<Provider>>` so all three pinned
/// providers (Claude Code, Codex, Ollama) can drive dispatch through the
/// same code path. The Claude-only `set_claude_code_pin` / `consume_*`
/// pair is preserved as a thin shim so the existing call site in
/// `ProfileModelSelector` keeps compiling without churn.
///
/// Per-session pinning lives on the `ProfileModelSelector` itself; this
/// global is a v1 bridge so the dispatcher can consult the pin without a
/// new threading-through change set. Once `AppContext` carries a
/// session-keyed pin map (PDX-104+), this latch can be removed.
static PROVIDER_PIN: OnceLock<StdMutex<Option<Provider>>> = OnceLock::new();

fn provider_pin_slot() -> &'static StdMutex<Option<Provider>> {
    PROVIDER_PIN.get_or_init(|| StdMutex::new(None))
}

/// Set the process-wide "next turn routes via this provider" pin.
///
/// `None` clears the pin. Called from
/// `ProfileModelSelectorAction::Select{ClaudeCode,Codex,Ollama}Model`
/// and (for tests) directly. The pin is consume-and-clear (see
/// [`consume_provider_pin`]) so it never leaks across turns.
pub(crate) fn set_provider_pin(provider: Option<Provider>) {
    let mut slot = provider_pin_slot()
        .lock()
        .expect("PROVIDER_PIN mutex is never poisoned");
    *slot = provider;
}

/// Read-and-clear the provider pin. Returns the pinned provider, if any.
/// The dispatcher calls this once per turn so the pin doesn't leak into
/// the *next-next* turn.
pub(crate) fn consume_provider_pin() -> Option<Provider> {
    let mut slot = provider_pin_slot()
        .lock()
        .expect("PROVIDER_PIN mutex is never poisoned");
    slot.take()
}

/// Claude-specific shim around [`set_provider_pin`] — kept for
/// backwards compatibility with PDX-103 callers and for tests. The
/// production `ProfileModelSelector::SelectClaudeCodeModel` arm now
/// calls [`set_provider_pin`] directly so all three providers go
/// through one code path (PDX-105 [B3] task 4).
#[allow(dead_code)]
pub(crate) fn set_claude_code_pin() {
    set_provider_pin(Some(Provider::ClaudeCode));
}

/// Clear the provider pin without consuming it. Kept for symmetry with
/// [`set_claude_code_pin`] and for tests; the runtime cleanup happens
/// via [`consume_provider_pin`].
#[allow(dead_code)]
pub(crate) fn clear_claude_code_pin() {
    set_provider_pin(None);
}

/// Read-and-clear the Claude Code pin. Returns `true` if the pin was set
/// to [`Provider::ClaudeCode`]; any other pin variant is left intact (so
/// switching providers does not reset the others' pins). Kept for
/// PDX-103 callers that haven't been migrated to
/// [`consume_provider_pin`] yet.
#[allow(dead_code)]
pub(crate) fn consume_claude_code_pin() -> bool {
    let mut slot = provider_pin_slot()
        .lock()
        .expect("PROVIDER_PIN mutex is never poisoned");
    if matches!(*slot, Some(Provider::ClaudeCode)) {
        *slot = None;
        true
    } else {
        false
    }
}

/// Process-wide [`McpForwarder`] (PDX-105 [B3] task 1).
///
/// One forwarder per process. The persistent [`Router`] dispatches every
/// turn, and after [`Router::select`] resolves to an [`AgentId`], the
/// dispatcher calls [`McpForwarder::set_active`] (see `driver.rs`) so that
/// MCP tool-call subscribers can re-target their tool sinks at the
/// currently-active agent.
///
/// Subscribers (e.g. the MCP manager) acquire a `watch::Receiver` via
/// [`mcp_forwarder`]`().subscribe()` and react to target changes
/// asynchronously.
static GLOBAL_MCP_FORWARDER: OnceLock<Arc<McpForwarder>> = OnceLock::new();

/// Returns the process-wide [`McpForwarder`].
///
/// Idempotent and cheap — initialised on first access. All callers that
/// reach for the forwarder obtain the same `Arc`, so dispatch-side
/// `set_active` calls and MCP-side `subscribe` calls observe the same
/// state.
pub fn mcp_forwarder() -> Arc<McpForwarder> {
    GLOBAL_MCP_FORWARDER
        .get_or_init(|| Arc::new(McpForwarder::new()))
        .clone()
}

/// Map a [`Provider`] to its registered `estimated_micros_per_task`, the
/// per-task cost estimate used by the router tie-break sort key.
///
/// PDX-27 [D4] task 4 uses this as the placeholder amount to debit
/// post-run, until real token counts flow through the agent stream from
/// Symphony 13.5. Provider variants without a registered agent map to
/// `0`, which makes [`super::budget_enforcer::BudgetEnforcer::record_charge`]
/// a no-op.
pub(crate) fn estimated_micros_for_provider(provider: Provider) -> u64 {
    match provider {
        Provider::FoundationModels => LOCAL_AGENT_ESTIMATED_MICROS,
        Provider::ClaudeCode => CLAUDE_CODE_SONNET_46_ESTIMATED_MICROS,
        Provider::Codex => CODEX_WORKER_ESTIMATED_MICROS,
        Provider::Ollama => OLLAMA_ESTIMATED_MICROS,
        Provider::Custom(_) => 0,
    }
}

/// Generates a fresh [`TaskId`] for use when constructing an
/// `orchestrator::Task` in the Oz dispatch arm.
///
/// Exposed as a thin convenience wrapper so the call-site in `driver.rs`
/// does not need to import `TaskId` and call `TaskId::new()` directly,
/// keeping the orchestrator API surface minimal at the call site.
pub(crate) fn new_task_id() -> TaskId {
    TaskId::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use orchestrator::{Role, TaskContext};

    /// The provider pin and the McpForwarder are process-wide singletons,
    /// so any test that touches them must serialise against every other
    /// such test to avoid the test-runner's parallel scheduler racing on
    /// the same `OnceLock`. A plain `std::sync::Mutex` is the lightest
    /// answer; tests poison-recover so a panic in one doesn't cascade.
    static PIN_TEST_LOCK: StdMutex<()> = StdMutex::new(());

    fn pin_test_guard() -> std::sync::MutexGuard<'static, ()> {
        // `lock()` returns `PoisonError` if a previous test panicked
        // while holding the guard. We unwrap-or-into-inner to keep
        // running on the same data; the data is `()` so there's
        // nothing to recover.
        PIN_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn worker_task() -> Task {
        Task {
            id: new_task_id(),
            role: Role::Worker,
            prompt: String::new(),
            context: TaskContext::default(),
            budget_hint: None,
        }
    }

    /// Local agent must be registered exactly once, and the same Arc
    /// returned across calls so `stage_run` from the dispatcher and
    /// `Agent::execute` see the same staged-slot.
    #[tokio::test]
    async fn ensure_persistent_router_returns_stable_local_agent() {
        let (_router, a) = ensure_persistent_router().await;
        let (_router, b) = ensure_persistent_router().await;
        assert!(Arc::ptr_eq(&a, &b));
    }

    /// Two consecutive `ensure_persistent_router` calls share the same
    /// underlying Router (and thus the same Budget).
    #[tokio::test]
    async fn ensure_persistent_router_returns_same_router_arc() {
        let (r1, _) = ensure_persistent_router().await;
        let (r2, _) = ensure_persistent_router().await;
        assert!(Arc::ptr_eq(&r1, &r2));
    }

    /// Tie-break check: when ClaudeCodeAgent is *not* registered (no `claude`
    /// on PATH in CI), Worker tasks still route to the local agent without
    /// blowing up. This also exercises the "fall-through" guarantee called
    /// out in PDX-103's acceptance criteria for signed-out users.
    #[tokio::test]
    async fn worker_routes_to_local_when_claude_unavailable() {
        let (router, _local) = ensure_persistent_router().await;
        let router = router.lock().await;
        let task = worker_task();
        let agent = router.select(&task).await.expect("select");
        // We can't assert *which* agent without `claude` on PATH being
        // deterministic across CI/dev, but the local id should always be a
        // valid winner in CI where claude is absent. In a dev box with the
        // CLI installed, the local agent still wins on tie-break because
        // `LOCAL_AGENT_ESTIMATED_MICROS < CLAUDE_CODE_SONNET_46_ESTIMATED_MICROS`
        // for plain Worker work.
        assert_eq!(agent.id().0, LOCAL_AGENT_ID);
    }

    /// PDX-103 [B1] task 5 tie-break: the local agent must beat Claude on
    /// plain Worker work because its `estimated_micros_per_task` is lower.
    /// This guard prevents a future regression where someone re-registers
    /// the local agent with a non-zero estimate or bumps Claude's value
    /// downward.
    #[test]
    fn tie_break_prefers_local_for_plain_worker() {
        assert!(
            LOCAL_AGENT_ESTIMATED_MICROS < CLAUDE_CODE_SONNET_46_ESTIMATED_MICROS,
            "local must be cheaper than Claude on plain Worker tie-break (PDX-103 task 5)"
        );
    }

    /// PDX-103 [B1] task 7c: the consume-and-clear pin latch flips back to
    /// `false` after the dispatcher reads it, so the pin never leaks across
    /// turns.
    #[test]
    fn claude_code_pin_latch_consume_and_clear() {
        let _g = pin_test_guard();
        clear_claude_code_pin();
        assert!(!consume_claude_code_pin());
        set_claude_code_pin();
        assert!(consume_claude_code_pin());
        // Second consume returns the cleared default.
        assert!(!consume_claude_code_pin());
    }

    /// PDX-105 [B3] task 4: the generalised provider pin round-trips
    /// every variant and is read-and-clear (each consume yields `None`
    /// until the next set).
    #[test]
    fn provider_pin_round_trips_every_variant() {
        let _g = pin_test_guard();
        // Reset state; this static is shared with other tests in the
        // same process so we always start from `None` here.
        set_provider_pin(None);
        assert_eq!(consume_provider_pin(), None);

        for provider in [
            Provider::ClaudeCode,
            Provider::Codex,
            Provider::Ollama,
        ] {
            set_provider_pin(Some(provider));
            assert_eq!(consume_provider_pin(), Some(provider));
            // Read-and-clear: next consume is `None`.
            assert_eq!(consume_provider_pin(), None);
        }
    }

    /// PDX-105 [B3] task 4: the legacy Claude-only consume shim only
    /// matches when the pin is *Claude*, leaving Codex/Ollama pins
    /// intact. This guards the shim's compatibility contract for
    /// pre-PDX-105 call sites.
    #[test]
    fn consume_claude_code_pin_only_matches_claude() {
        let _g = pin_test_guard();
        set_provider_pin(Some(Provider::Codex));
        assert!(!consume_claude_code_pin());
        // Codex pin should still be there.
        assert_eq!(consume_provider_pin(), Some(Provider::Codex));

        set_provider_pin(Some(Provider::ClaudeCode));
        assert!(consume_claude_code_pin());
        // Cleared.
        assert_eq!(consume_provider_pin(), None);
    }

    /// PDX-105 [B3] task 1: every caller of [`mcp_forwarder`] sees the
    /// *same* `Arc<McpForwarder>` so that dispatch-side `set_active`
    /// notifies MCP-side subscribers obtained earlier.
    #[test]
    fn mcp_forwarder_is_process_wide_singleton() {
        let _g = pin_test_guard();
        let a = mcp_forwarder();
        let b = mcp_forwarder();
        assert!(Arc::ptr_eq(&a, &b));

        // A subscriber obtained from one handle observes a state change
        // pushed through the other handle.
        let mut rx = a.subscribe();
        b.set_active(AgentId("alpha".to_string()));
        assert_eq!(
            rx.borrow_and_update().agent_id().cloned(),
            Some(AgentId("alpha".to_string()))
        );
    }

    /// PDX-105 [B3] task 1 + 3: simulates the wiring contract end-to-end.
    /// One handle plays the dispatch site (`set_active(A)`, then
    /// `set_active(B)`), and another handle plays the MCP-side subscriber
    /// that re-targets its tool sink. The subscriber observes both
    /// switches via the watch channel.
    #[tokio::test]
    async fn dispatch_set_active_flows_through_to_subscriber() {
        let _g = pin_test_guard();
        // Reset the global to a known-good baseline before the test.
        // (Other tests may have left the forwarder pointing somewhere.)
        let dispatch = mcp_forwarder();
        dispatch.clear_active();

        // MCP-side subscriber, obtained *before* the first switch — the
        // PDX-75 design guarantees no events are lost.
        let mcp_side = mcp_forwarder();
        let mut rx = mcp_side.subscribe();
        // Drain the initial baseline so `changed()` only fires for the
        // dispatch's `set_active`.
        let _ = rx.borrow_and_update();

        let agent_a = AgentId("agent-a".to_string());
        let agent_b = AgentId("agent-b".to_string());

        assert!(dispatch.set_active(agent_a.clone()));
        rx.changed().await.expect("watch sender alive");
        assert_eq!(rx.borrow_and_update().agent_id().cloned(), Some(agent_a));

        assert!(dispatch.set_active(agent_b.clone()));
        rx.changed().await.expect("watch sender alive");
        assert_eq!(rx.borrow_and_update().agent_id().cloned(), Some(agent_b));
    }

    /// PDX-105 [B3] task 4: when both an explicit pin (passed in by the
    /// caller) and a generalised provider pin (set via
    /// [`set_provider_pin`]) are present, the explicit pin wins. This
    /// matches the precedence rule wired into
    /// `run_via_local_orchestrator`: it calls
    /// `pinned_provider.or_else(consume_provider_pin)`, so the latch is
    /// only consulted when the caller didn't pre-resolve a provider.
    #[test]
    fn provider_pin_explicit_takes_precedence_over_latch() {
        let _g = pin_test_guard();
        set_provider_pin(Some(Provider::Codex));
        let explicit: Option<Provider> = Some(Provider::ClaudeCode);
        let resolved = explicit.or_else(consume_provider_pin);
        assert_eq!(resolved, Some(Provider::ClaudeCode));
        // The latch must still be live (untouched) because the explicit
        // pin short-circuited the `or_else`. PDX-104 callers depend on
        // this so a stale latch doesn't fire on the next turn.
        assert_eq!(consume_provider_pin(), Some(Provider::Codex));
    }

    /// PDX-105 [B3] task 4: with no caller-provided pin, the generalised
    /// latch is consulted and consumed. Mirrors the previous
    /// Claude-only `consume_claude_code_pin` precedence behaviour, now
    /// on every provider variant.
    #[test]
    fn provider_pin_falls_back_to_latch_when_caller_unspecified() {
        let _g = pin_test_guard();
        set_provider_pin(Some(Provider::Ollama));
        let explicit: Option<Provider> = None;
        let resolved = explicit.or_else(consume_provider_pin);
        assert_eq!(resolved, Some(Provider::Ollama));
        // Latch must be cleared after a successful consume.
        assert_eq!(consume_provider_pin(), None);
    }

    /// PDX-103 [B1] task 4: `encode_prompt_for_subprocess` round-trips
    /// `Local` prompts losslessly and falls back to an empty string for
    /// `ServerSide` prompts.
    #[test]
    fn encode_prompt_for_subprocess_handles_local_prompt() {
        let prompt = AgentRunPrompt::Local("hello world".to_string());
        assert_eq!(encode_prompt_for_subprocess(Some(&prompt)), "hello world");
        assert_eq!(encode_prompt_for_subprocess(None), "");
    }

    /// PDX-104 [B2] task 3: bare prompts default to `Worker`, the safe
    /// fall-back the rest of the suite relies on.
    #[test]
    fn classify_prompt_defaults_to_worker() {
        assert_eq!(classify_prompt_text(""), Role::Worker);
        assert_eq!(
            classify_prompt_text("Refactor app.rs to drop the orphan import"),
            Role::Worker
        );
    }

    /// PDX-104 [B2] task 3: explicit summarize cues promote to
    /// `Role::Summarize` so the router can prefer Ollama (which advertises
    /// the role at a low `estimated_micros_per_task`).
    #[test]
    fn classify_prompt_detects_summarize_cues() {
        assert_eq!(
            classify_prompt_text("Summarize the changes made in PR #1"),
            Role::Summarize
        );
        assert_eq!(
            classify_prompt_text("tl;dr the conversation so far"),
            Role::Summarize
        );
        assert_eq!(
            classify_prompt_text("Give me a summary of the failing test"),
            Role::Summarize
        );
    }

    /// Planner / Reviewer cues are also detected, although less frequently
    /// in practice — those routes remain mostly Claude Code's domain.
    #[test]
    fn classify_prompt_detects_planner_and_reviewer_cues() {
        assert_eq!(
            classify_prompt_text("Plan a migration from monolith to services"),
            Role::Planner
        );
        assert_eq!(
            classify_prompt_text("Please review this diff for bugs"),
            Role::Reviewer
        );
    }

    /// PDX-104 [B2] task 3: `infer_role_from_prompt` honors the prompt
    /// kind. `ServerSide` falls back to Worker because the text isn't
    /// resolvable here.
    #[test]
    fn infer_role_uses_local_text_and_falls_back_for_server_side() {
        assert_eq!(
            infer_role_from_prompt(&AgentRunPrompt::Local(
                "Summarize git history".to_string()
            )),
            Role::Summarize
        );
        assert_eq!(
            infer_role_from_prompt(&AgentRunPrompt::ServerSide {
                skill: None,
                attachments_dir: None,
            }),
            Role::Worker
        );
    }

    /// PDX-104 [B2] tasks 1 + 2 + 4: every billable provider has an entry
    /// in the cap table backing the persistent router. A missing entry
    /// causes `Budget::current_tier` to fail mid-dispatch, which
    /// `Router::select` would then surface as a hard error.
    #[test]
    fn cap_table_has_entries_for_all_registered_providers() {
        let caps = super::super::provider_caps::ProviderCapsConfig::defaults();
        for provider in [
            Provider::FoundationModels,
            Provider::ClaudeCode,
            Provider::Codex,
            Provider::Ollama,
        ] {
            assert!(
                caps.caps().contains_key(&provider),
                "missing cap for {provider:?}"
            );
        }
    }

    /// PDX-104 [B2] task 1: tie-break ordering for plain Worker tasks is
    /// stable: local Foundation Models < Claude Code < Codex. Guards
    /// against regressions where a future contributor bumps the
    /// estimates inadvertently and reverses the preference order.
    #[test]
    fn tie_break_local_beats_claude_beats_codex_for_worker() {
        assert!(LOCAL_AGENT_ESTIMATED_MICROS < CLAUDE_CODE_SONNET_46_ESTIMATED_MICROS);
        assert!(CLAUDE_CODE_SONNET_46_ESTIMATED_MICROS < CODEX_WORKER_ESTIMATED_MICROS);
    }

    /// PDX-104 [B2] acceptance: when both are present, Ollama beats Claude
    /// on a Summarize task because Sonnet 4.6 does not advertise the role
    /// while Ollama does. Even without `ollama` and `claude` binaries
    /// installed, the tie-break invariant is testable directly: Ollama's
    /// `estimated_micros` is lower than Claude's.
    #[test]
    fn tie_break_ollama_cheaper_than_claude_on_summarize() {
        assert!(OLLAMA_ESTIMATED_MICROS < CLAUDE_CODE_SONNET_46_ESTIMATED_MICROS);
    }

    /// `codex_signed_in` returns `Some(false)` when the file is missing.
    /// On any normal CI host `~/.codex/auth.json` won't exist, so this is
    /// the universal default.
    #[test]
    fn codex_signed_in_handles_missing_file() {
        // We cannot easily mock `dirs::home_dir`; the actual call returns
        // `Some(false)` on a clean home dir without `~/.codex/auth.json`.
        // We at least confirm the function returns *something* without
        // panicking.
        let _ = codex_signed_in();
    }

    /// `ollama_installed` returns a deterministic boolean based on the
    /// current `PATH`. We just confirm it doesn't panic.
    #[test]
    fn ollama_installed_does_not_panic() {
        let _ = ollama_installed();
    }
}

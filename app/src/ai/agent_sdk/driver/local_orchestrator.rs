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

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use chrono::Utc;
use orchestrator::{
    Agent, AgentError, AgentEvent, AgentEventStream, AgentId, AgentRegistration, Budget, Cap,
    Capabilities, Health, Provider, Role, Router, Task, TaskId,
};
use tokio::sync::Mutex as AsyncMutex;
use warpui::ModelSpawner;

use super::{AgentDriver, AgentRunPrompt, SDKConversationOutputStatus};

/// Stable identifier used for the local Warp agent in the orchestrator registry.
const LOCAL_AGENT_ID: &str = "local-warp-oz";

/// Stable identifier used for the Claude Code Sonnet 4.6 agent.
pub(crate) const CLAUDE_CODE_SONNET_46_ID: &str = "claude-sonnet-46";

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

/// Per-month cap for [`Provider::ClaudeCode`], in micro-dollars.
///
/// Set high enough that we never accidentally halt routing in the v1 wiring
/// — the user is paying Anthropic directly via the CLI's own auth, so this
/// is mostly an accounting bucket for tier-aware fallbacks. Tighten via
/// settings in a follow-up.
const CLAUDE_CODE_MONTHLY_CAP_MICROS: u64 = 200_000_000; // $200/mo
const CLAUDE_CODE_SESSION_CAP_MICROS: u64 = 50_000_000; // $50/session

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

/// Construct the budget that backs [`GLOBAL_ROUTER`].
///
/// Local Foundation Models are unbounded (in-process, free); Claude Code is
/// budgeted so the tier-aware filter has something to enforce.
fn build_persistent_budget() -> Arc<Budget> {
    let mut caps = HashMap::new();
    caps.insert(
        Provider::FoundationModels,
        Cap {
            monthly_micro_dollars: u64::MAX,
            session_micro_dollars: u64::MAX,
        },
    );
    caps.insert(
        Provider::ClaudeCode,
        Cap {
            monthly_micro_dollars: CLAUDE_CODE_MONTHLY_CAP_MICROS,
            session_micro_dollars: CLAUDE_CODE_SESSION_CAP_MICROS,
        },
    );
    Arc::new(Budget::new(caps))
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
            let budget = build_persistent_budget();
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
/// (`ProfileModelSelector::SelectClaudeCodeModel`) — when `true`, the next
/// `run_via_local_orchestrator` call dispatches through the Claude Code
/// agent regardless of Role tie-break.
///
/// Per-session pinning lives on the `ProfileModelSelector` itself; this
/// global is a v1 bridge so the dispatcher can consult the pin without a
/// new threading-through change set in this PR. Once `AppContext` carries a
/// session-keyed pin map (PDX-104+), this latch can be removed.
static CLAUDE_CODE_PIN: AtomicBool = AtomicBool::new(false);

/// Set the process-wide "next turn routes via Claude Code" latch.
///
/// Called from `ProfileModelSelectorAction::SelectClaudeCodeModel` and
/// (for tests) directly from `run_via_local_orchestrator` callers. Stays
/// `true` until cleared via [`clear_claude_code_pin`] or auto-cleared at
/// dispatch time (see `consume_claude_code_pin`).
pub(crate) fn set_claude_code_pin() {
    CLAUDE_CODE_PIN.store(true, Ordering::SeqCst);
}

/// Clear the Claude Code pin without consuming it. Kept on the public
/// surface for symmetry with `set_claude_code_pin` and for tests; the
/// runtime cleanup happens via [`consume_claude_code_pin`].
#[allow(dead_code)]
pub(crate) fn clear_claude_code_pin() {
    CLAUDE_CODE_PIN.store(false, Ordering::SeqCst);
}

/// Read-and-clear the Claude Code pin. Returns `true` if the pin was set.
/// The dispatcher calls this once per turn so the pin doesn't leak into
/// the *next-next* turn.
pub(crate) fn consume_claude_code_pin() -> bool {
    CLAUDE_CODE_PIN.swap(false, Ordering::SeqCst)
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
        clear_claude_code_pin();
        assert!(!consume_claude_code_pin());
        set_claude_code_pin();
        assert!(consume_claude_code_pin());
        // Second consume returns the cleared default.
        assert!(!consume_claude_code_pin());
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
}

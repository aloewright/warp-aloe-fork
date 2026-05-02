//! PDX-106 [B4] Fault-injection pass.
//!
//! These tests harden the routing + forwarding pipeline established by
//! PDX-103 / PDX-104 / PDX-105 by simulating the failure modes that the
//! production code is supposed to recover from:
//!
//! 1. **Health failover** — an agent flips its [`Health::healthy`] flag to
//!    `false` mid-session and the [`Router`] bypasses it on the next dispatch,
//!    falling through to the next-eligible provider.
//! 2. **Budget tier enforcement** — a provider is forced into
//!    [`BudgetTier::Critical`] and the router refuses non-Planner/Reviewer
//!    work, while still allowing Planner / Reviewer roles.
//! 3. **MCP re-target sequencing** — the [`McpForwarder`] correctly observes
//!    a sequence of `set_active(A) → set_active(B) → set_active(A)` switches,
//!    and a buffered tool call routed via the *current* `active_agent_id()`
//!    reaches the *currently* active agent rather than the one that was active
//!    when the call was enqueued.
//! 4. **Multi-agent precedence** — the router's tie-break across three
//!    healthy agents at the same tier is deterministic and lexicographic;
//!    same-cost ties are resolved by [`AgentId`].
//! 5. **End-to-end soak** — a single integration test that walks the full
//!    chain (router select → forwarder set_active → fault inject → re-select
//!    → forwarder re-target → subscriber observes the new agent), gated
//!    behind `#[ignore = "soak"]` so it runs only via `cargo test --ignored`.
//!
//! # Test agent
//!
//! The router only reads metadata off the [`Agent`] trait, so a stub with a
//! controllable [`Health`] field is enough to drive every scenario. The
//! [`FaultInjectingAgent`] holds its `Health` behind an [`Arc<RwLock<…>>`]
//! so tests can flip it from the outside without a re-registration.
//!
//! Tests that mutate process-wide globals (none here — the persistent router
//! lives in `app/`, not in the orchestrator crate) wouldn't need a global
//! lock; we keep these tests entirely orchestrator-local for that reason.

#![allow(clippy::needless_borrow)]

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use orchestrator::{
    Agent, AgentEventStream, AgentId, AgentRegistration, Budget, BudgetTier, Cap, Capabilities,
    ForwardingTarget, Health, McpForwarder, Provider, Role, Router, RouterError, Task,
    TaskContext, TaskId,
};
use tokio::sync::RwLock;

// ---------------------------------------------------------------------------
// FaultInjectingAgent
// ---------------------------------------------------------------------------

/// Stub [`Agent`] with a flippable [`Health`] — the workhorse for every
/// fault-injection test in this file.
///
/// The router reads `health()` synchronously, which means we can't put the
/// state behind an async lock. We use a [`std::sync::RwLock`] instead; the
/// critical section is a clone of [`Health`] (a small POD), so contention
/// is irrelevant in practice.
struct FaultInjectingAgent {
    id: AgentId,
    capabilities: Capabilities,
    health: Arc<std::sync::RwLock<Health>>,
}

impl FaultInjectingAgent {
    fn new(id: &str, roles: &[Role]) -> Self {
        Self {
            id: AgentId(id.to_string()),
            capabilities: Capabilities {
                roles: roles.iter().copied().collect::<HashSet<_>>(),
                max_context_tokens: 100_000,
                supports_tools: true,
                supports_vision: false,
            },
            health: Arc::new(std::sync::RwLock::new(Health {
                healthy: true,
                last_check: Utc::now(),
                error_rate: 0.0,
            })),
        }
    }

    /// Borrow the shared health handle so a test can flip the flag mid-flight.
    fn health_handle(&self) -> Arc<std::sync::RwLock<Health>> {
        Arc::clone(&self.health)
    }
}

#[async_trait]
impl Agent for FaultInjectingAgent {
    fn id(&self) -> AgentId {
        self.id.clone()
    }

    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    async fn execute(&self, _task: Task) -> Result<AgentEventStream, orchestrator::AgentError> {
        unreachable!(
            "FaultInjectingAgent::execute should never run; these tests only \
             exercise Router::select and McpForwarder, never agent dispatch."
        );
    }

    fn health(&self) -> Health {
        self.health.read().expect("health lock poisoned").clone()
    }
}

/// Mark `agent`'s health as unhealthy. Mirrors the sequence at
/// `crates/agents/src/claude_code.rs:323-327` where the production agent
/// flips the same fields when its subprocess exits with a non-zero status.
fn fault_inject_kill(handle: &Arc<std::sync::RwLock<Health>>) {
    let mut guard = handle.write().expect("health lock poisoned");
    guard.healthy = false;
    guard.last_check = Utc::now();
    guard.error_rate = (guard.error_rate + 1.0).min(1.0);
}

/// Convenience: a [`Task`] with the requested role and otherwise blank context.
fn task_with_role(role: Role) -> Task {
    Task {
        id: TaskId::new(),
        role,
        prompt: "fault injection".to_string(),
        context: TaskContext {
            cwd: PathBuf::from("/tmp"),
            env: HashMap::new(),
            metadata: HashMap::new(),
        },
        budget_hint: None,
    }
}

/// Build a [`Budget`] with a generous cap for every supplied provider —
/// keeps tier == Healthy for the duration of the test unless explicitly
/// drained.
fn ample_budget(providers: &[Provider]) -> Arc<Budget> {
    let mut caps = HashMap::new();
    for p in providers {
        caps.insert(
            *p,
            Cap {
                monthly_micro_dollars: 1_000_000_000,
                session_micro_dollars: 1_000_000_000,
            },
        );
    }
    Arc::new(Budget::new(caps))
}

/// Drain `provider`'s budget until its tier reaches `target`.
async fn drain_to_tier(
    budget: &Budget,
    provider: Provider,
    cap: Cap,
    target: BudgetTier,
) {
    let monthly = cap.monthly_micro_dollars;
    let charge = match target {
        BudgetTier::Healthy => return,
        BudgetTier::Warning => monthly / 2,
        BudgetTier::Critical => (monthly / 10) * 9,
        BudgetTier::Halted => monthly,
    };
    let _ = budget.try_charge(provider, charge).await;
}

// ---------------------------------------------------------------------------
// Task 1 — Health failover
// ---------------------------------------------------------------------------

/// **PDX-106 task 1 (acceptance criterion 1).**
///
/// A "Claude" agent and a "Codex" agent are both healthy; Claude wins the
/// tie-break. The Claude subprocess "dies" (we flip its `health.healthy =
/// false` exactly the way `claude_code.rs:323-327` does on a non-zero exit).
/// On the next [`Router::select`] dispatch, Claude is excluded by the health
/// filter and Codex is selected instead.
#[tokio::test]
async fn health_failover_routes_to_codex_after_claude_dies() {
    let budget = ample_budget(&[Provider::ClaudeCode, Provider::Codex]);
    let mut router = Router::new(budget);

    let claude = Arc::new(FaultInjectingAgent::new(
        "claude-fault",
        &[Role::Worker, Role::Planner],
    ));
    let codex = Arc::new(FaultInjectingAgent::new("codex-fault", &[Role::Worker]));
    let claude_health = claude.health_handle();

    router.register(AgentRegistration {
        agent: claude.clone(),
        provider: Provider::ClaudeCode,
        // Claude wins on cost first while both are healthy.
        estimated_micros_per_task: 1_000,
    });
    router.register(AgentRegistration {
        agent: codex.clone(),
        provider: Provider::Codex,
        estimated_micros_per_task: 5_000,
    });

    // Pre-fault: Claude wins the Worker dispatch.
    let task = task_with_role(Role::Worker);
    let chosen = router.select(&task).await.expect("pre-fault select");
    assert_eq!(
        chosen.id().0,
        "claude-fault",
        "Claude should win the tie-break before the fault"
    );

    // Inject the fault.
    fault_inject_kill(&claude_health);

    // Post-fault: Claude's `health.healthy` is `false`, so the health filter
    // excludes it and Codex wins.
    let chosen = router.select(&task).await.expect("post-fault select");
    assert_eq!(
        chosen.id().0,
        "codex-fault",
        "Codex must win after Claude is faulted out"
    );

    // The router itself doesn't care, but we sanity-check that the agent
    // honors the new health snapshot.
    assert!(!claude.health().healthy);
    assert!(claude.health().error_rate > 0.0);
}

/// **PDX-106 task 1 (negative case).**
///
/// When the *only* registered Worker dies, [`Router::select`] returns
/// [`RouterError::AllUnhealthy`] rather than silently picking a non-capable
/// agent. This guards against a regression where the health filter is
/// short-circuited.
#[tokio::test]
async fn health_failover_returns_allunhealthy_when_no_survivors() {
    let budget = ample_budget(&[Provider::ClaudeCode]);
    let mut router = Router::new(budget);

    let claude = Arc::new(FaultInjectingAgent::new("solo-claude", &[Role::Worker]));
    let claude_health = claude.health_handle();

    router.register(AgentRegistration {
        agent: claude,
        provider: Provider::ClaudeCode,
        estimated_micros_per_task: 1_000,
    });

    fault_inject_kill(&claude_health);

    let task = task_with_role(Role::Worker);
    let err = router.select(&task).await.err().expect("expected error");
    assert!(
        matches!(err, RouterError::AllUnhealthy),
        "expected AllUnhealthy, got {err:?}"
    );
}

/// **PDX-106 task 1 (recovery).**
///
/// An agent flipped to unhealthy is excluded; flipping it back makes it
/// eligible again on the next dispatch. The router stores a reference to the
/// agent's `health()` method, so recovery is observed without re-registration.
#[tokio::test]
async fn health_recovery_re_enables_agent_without_reregistration() {
    let budget = ample_budget(&[Provider::ClaudeCode, Provider::Codex]);
    let mut router = Router::new(budget);

    let claude = Arc::new(FaultInjectingAgent::new("c", &[Role::Worker]));
    let codex = Arc::new(FaultInjectingAgent::new("d", &[Role::Worker]));
    let claude_health = claude.health_handle();

    router.register(AgentRegistration {
        agent: claude,
        provider: Provider::ClaudeCode,
        estimated_micros_per_task: 1_000,
    });
    router.register(AgentRegistration {
        agent: codex,
        provider: Provider::Codex,
        estimated_micros_per_task: 2_000,
    });

    let task = task_with_role(Role::Worker);

    // Kill Claude → Codex wins.
    fault_inject_kill(&claude_health);
    assert_eq!(router.select(&task).await.unwrap().id().0, "d");

    // Restore Claude → Claude wins again on tie-break (lower cost).
    {
        let mut g = claude_health.write().unwrap();
        g.healthy = true;
        g.error_rate = 0.0;
    }
    assert_eq!(router.select(&task).await.unwrap().id().0, "c");
}

// ---------------------------------------------------------------------------
// Task 2 — Budget tier enforcement
// ---------------------------------------------------------------------------

/// **PDX-106 task 2 (acceptance criterion 2, primary).**
///
/// Forcing Claude's budget into [`BudgetTier::Critical`] and dispatching a
/// `Role::Worker` task must route to Codex. The Critical-tier filter only
/// admits Planner / Reviewer roles, so Worker has to fall through to a
/// healthy non-Critical provider.
#[tokio::test]
async fn budget_critical_demotes_worker_to_codex() {
    let claude_cap = Cap {
        monthly_micro_dollars: 1_000,
        session_micro_dollars: 1_000_000_000,
    };
    let mut caps = HashMap::new();
    caps.insert(Provider::ClaudeCode, claude_cap);
    caps.insert(
        Provider::Codex,
        Cap {
            monthly_micro_dollars: 1_000_000_000,
            session_micro_dollars: 1_000_000_000,
        },
    );
    let budget = Arc::new(Budget::new(caps));

    drain_to_tier(&budget, Provider::ClaudeCode, claude_cap, BudgetTier::Critical).await;
    assert_eq!(
        budget.current_tier(Provider::ClaudeCode).await.unwrap(),
        BudgetTier::Critical,
        "drain helper must put Claude into Critical"
    );
    assert_eq!(
        budget.current_tier(Provider::Codex).await.unwrap(),
        BudgetTier::Healthy,
        "Codex stays Healthy"
    );

    let mut router = Router::new(budget);
    router.register(AgentRegistration {
        agent: Arc::new(FaultInjectingAgent::new(
            "claude-critical",
            &[Role::Worker, Role::Planner],
        )),
        provider: Provider::ClaudeCode,
        estimated_micros_per_task: 1_000,
    });
    router.register(AgentRegistration {
        agent: Arc::new(FaultInjectingAgent::new("codex-healthy", &[Role::Worker])),
        provider: Provider::Codex,
        estimated_micros_per_task: 5_000,
    });

    let chosen = router
        .select(&task_with_role(Role::Worker))
        .await
        .expect("Worker task should fall through to Codex");
    assert_eq!(
        chosen.id().0,
        "codex-healthy",
        "Worker must skip the Critical-tier Claude provider"
    );
}

/// **PDX-106 task 2 (acceptance criterion 2, allow-list).**
///
/// A Planner task is *not* refused even when the only capable provider is
/// in [`BudgetTier::Critical`]; the tier filter exempts Planner / Reviewer.
#[tokio::test]
async fn budget_critical_still_allows_planner() {
    let cap = Cap {
        monthly_micro_dollars: 1_000,
        session_micro_dollars: 1_000_000_000,
    };
    let mut caps = HashMap::new();
    caps.insert(Provider::ClaudeCode, cap);
    let budget = Arc::new(Budget::new(caps));

    drain_to_tier(&budget, Provider::ClaudeCode, cap, BudgetTier::Critical).await;

    let mut router = Router::new(budget);
    router.register(AgentRegistration {
        agent: Arc::new(FaultInjectingAgent::new(
            "claude-critical",
            &[Role::Worker, Role::Planner, Role::Reviewer],
        )),
        provider: Provider::ClaudeCode,
        estimated_micros_per_task: 1_000,
    });

    let chosen = router
        .select(&task_with_role(Role::Planner))
        .await
        .expect("Planner is allow-listed at Critical tier");
    assert_eq!(chosen.id().0, "claude-critical");

    let chosen = router
        .select(&task_with_role(Role::Reviewer))
        .await
        .expect("Reviewer is allow-listed at Critical tier");
    assert_eq!(chosen.id().0, "claude-critical");
}

/// **PDX-106 task 2 (Halted tier).**
///
/// A Halted-tier provider is excluded for *all* roles, not just Worker. The
/// dispatcher must surface this distinctly from the Critical-tier rejection
/// because the recovery path is different (Halted → wait for monthly reset
/// or top-up; Critical → fall back).
#[tokio::test]
async fn budget_halted_excludes_planner_too() {
    let cap = Cap {
        monthly_micro_dollars: 1_000,
        session_micro_dollars: 1_000_000_000,
    };
    let mut caps = HashMap::new();
    caps.insert(Provider::ClaudeCode, cap);
    let budget = Arc::new(Budget::new(caps));

    drain_to_tier(&budget, Provider::ClaudeCode, cap, BudgetTier::Halted).await;
    assert_eq!(
        budget.current_tier(Provider::ClaudeCode).await.unwrap(),
        BudgetTier::Halted
    );

    let mut router = Router::new(budget);
    router.register(AgentRegistration {
        agent: Arc::new(FaultInjectingAgent::new(
            "claude-halted",
            &[Role::Planner, Role::Reviewer, Role::Worker],
        )),
        provider: Provider::ClaudeCode,
        estimated_micros_per_task: 1_000,
    });

    let err = router
        .select(&task_with_role(Role::Planner))
        .await
        .err()
        .expect("Halted tier should reject even Planner");
    // Halted is more severe than Critical; the router surfaces it as
    // BudgetHalted, not NoFallbackForTier.
    assert!(
        matches!(err, RouterError::BudgetHalted),
        "expected BudgetHalted, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// Task 3 — MCP re-target
// ---------------------------------------------------------------------------

/// **PDX-106 task 3 (acceptance criterion 4, primary).**
///
/// Subscribe an [`McpForwarder`] from a "client task", then drive
/// `set_active(A) → set_active(B) → set_active(A)` from a "dispatcher task"
/// and confirm the subscriber observes the *current* active agent at every
/// `borrow_and_update`.
///
/// This proves the watch channel coalesces correctly and that the
/// "set_active is a no-op when same id" invariant doesn't break the
/// sequencing.
#[tokio::test]
async fn mcp_forwarder_observes_full_switch_sequence() {
    let forwarder = Arc::new(McpForwarder::new());
    let mut rx = forwarder.subscribe();

    let agent_a = AgentId("agent-A".to_string());
    let agent_b = AgentId("agent-B".to_string());

    // First switch.
    assert!(forwarder.set_active(agent_a.clone()), "first set_active changes target");
    rx.changed().await.expect("subscriber notified of A");
    assert_eq!(
        rx.borrow_and_update().clone(),
        ForwardingTarget::Agent(agent_a.clone())
    );

    // Second switch — different agent, must notify.
    assert!(forwarder.set_active(agent_b.clone()), "switch to B is a change");
    rx.changed().await.expect("subscriber notified of B");
    assert_eq!(
        rx.borrow_and_update().clone(),
        ForwardingTarget::Agent(agent_b.clone())
    );

    // Third switch — back to A. Even though A was previously active, B is
    // currently active, so this is a real change.
    assert!(
        forwarder.set_active(agent_a.clone()),
        "back-to-A is a change because B is current"
    );
    rx.changed().await.expect("subscriber notified of A again");
    assert_eq!(
        rx.borrow_and_update().clone(),
        ForwardingTarget::Agent(agent_a.clone())
    );

    // Snapshot agrees with the watch.
    assert_eq!(forwarder.active_agent_id(), Some(agent_a));
}

/// **PDX-106 task 3 (acceptance criterion 4, buffered MCP tool calls).**
///
/// A "buffered MCP tool call" — modeled here as a coroutine that resolves
/// `forwarder.active_agent_id()` *at delivery time* — must reach the
/// currently-active agent, not the one that was active when the call was
/// enqueued. This is the load-bearing invariant for the B3 wiring:
/// switching agents while a tool call is in flight redirects the result.
#[tokio::test]
async fn buffered_mcp_call_targets_current_active_not_original() {
    let forwarder = Arc::new(McpForwarder::new());
    let original = AgentId("agent-original".to_string());
    let switched = AgentId("agent-switched".to_string());

    forwarder.set_active(original.clone());

    // Enqueue the "buffered tool call". The call captures only the
    // forwarder handle, not the active agent at enqueue time — that's the
    // contract the McpForwarder is designed around.
    let fw = Arc::clone(&forwarder);
    let buffered = tokio::spawn(async move {
        // Simulate a tiny processing delay so the switch can win the race.
        tokio::task::yield_now().await;
        fw.active_agent_id()
    });

    // Switch the active agent before the buffered call resolves.
    forwarder.set_active(switched.clone());

    let delivered_to = buffered
        .await
        .expect("buffered task completed")
        .expect("active agent set");
    assert_eq!(
        delivered_to, switched,
        "buffered MCP call must reach the *current* active agent, not the original"
    );
}

/// **PDX-106 task 3 (subscribe-after-switch).**
///
/// A subscriber attached *after* `set_active` was called still sees the
/// current target on its first read — the watch channel always preserves
/// the latest value rather than dropping it for late subscribers. This
/// matters because in production the subscriber is spawned by the MCP
/// manager, which may attach after the orchestrator has already picked
/// a default agent.
#[tokio::test]
async fn late_subscribe_sees_current_target() {
    let forwarder = Arc::new(McpForwarder::new());
    let agent = AgentId("pre-existing".to_string());

    forwarder.set_active(agent.clone());

    // Subscribe *after* the switch.
    let mut rx = forwarder.subscribe();
    let initial = rx.borrow_and_update().clone();
    assert_eq!(
        initial,
        ForwardingTarget::Agent(agent),
        "late subscriber must see the current target on first read"
    );
}

/// **PDX-106 task 3 (multiple-subscriber re-target).**
///
/// Two independent subscribers — modeling, e.g., the chat MCP forwarder and
/// the build MCP forwarder — both observe the same switch sequence. We
/// verify they each see B after the switch.
#[tokio::test]
async fn multi_subscriber_retarget_observed_by_all() {
    let forwarder = Arc::new(McpForwarder::new());

    let mut rx1 = forwarder.subscribe();
    let mut rx2 = forwarder.subscribe();

    let a = AgentId("alpha".to_string());
    let b = AgentId("beta".to_string());

    forwarder.set_active(a.clone());
    let _ = rx1.borrow_and_update();
    let _ = rx2.borrow_and_update();

    forwarder.set_active(b.clone());

    assert!(rx1.has_changed().unwrap());
    assert!(rx2.has_changed().unwrap());
    assert_eq!(rx1.borrow_and_update().clone(), ForwardingTarget::Agent(b.clone()));
    assert_eq!(rx2.borrow_and_update().clone(), ForwardingTarget::Agent(b));
}

// ---------------------------------------------------------------------------
// Task 4 — Multi-agent precedence
// ---------------------------------------------------------------------------

/// **PDX-106 task 4 (acceptance criterion 5, deterministic tie-break).**
///
/// Three healthy agents at the same tier and same cost — the [`AgentId`]
/// lexicographic tiebreaker decides the winner. We register them in random
/// order and confirm the lex-min wins, repeated 100 times, to guard against
/// any HashMap-iteration nondeterminism leaking through into the sort.
///
/// This also models "concurrent dispatch with different roles" cheaply:
/// the router itself is stateless across `select` calls, so concurrent
/// dispatchers picking different roles never race for the same agent
/// (the agents can be picked by multiple dispatchers; the router never
/// hands out exclusive ownership).
#[tokio::test]
async fn multi_agent_tie_break_is_deterministic_and_lexicographic() {
    let budget = ample_budget(&[Provider::ClaudeCode, Provider::Codex, Provider::Ollama]);
    let mut router = Router::new(budget);

    // Insertion order is deliberately scrambled relative to the expected
    // winner ("alpha"). The router uses a `HashMap`, so insertion order is
    // *not* the tie-break — only `AgentId` is.
    router.register(AgentRegistration {
        agent: Arc::new(FaultInjectingAgent::new("zulu", &[Role::Worker])),
        provider: Provider::ClaudeCode,
        estimated_micros_per_task: 1_000,
    });
    router.register(AgentRegistration {
        agent: Arc::new(FaultInjectingAgent::new("mike", &[Role::Worker])),
        provider: Provider::Codex,
        estimated_micros_per_task: 1_000,
    });
    router.register(AgentRegistration {
        agent: Arc::new(FaultInjectingAgent::new("alpha", &[Role::Worker])),
        provider: Provider::Ollama,
        estimated_micros_per_task: 1_000,
    });

    let task = task_with_role(Role::Worker);
    let first = router.select(&task).await.unwrap().id();
    assert_eq!(first.0, "alpha", "lex-min should win the three-way tie");
    for _ in 0..100 {
        let again = router.select(&task).await.unwrap().id();
        assert_eq!(again, first, "selection must be deterministic across calls");
    }
}

/// **PDX-106 task 4 (concurrent dispatch).**
///
/// Two dispatchers running in parallel, asking for different roles, must
/// each receive a capable agent without budget races. The router holds an
/// `Arc<Budget>` and `Budget::current_tier` takes a read lock, so
/// concurrent reads are safe. `try_charge` would serialize on the write
/// lock, but we don't `try_charge` here — we just `select` — so this is
/// purely a read-path concurrency check.
#[tokio::test]
async fn concurrent_dispatch_picks_different_providers_without_races() {
    let budget = ample_budget(&[Provider::ClaudeCode, Provider::Codex, Provider::Ollama]);
    let mut router = Router::new(budget);

    router.register(AgentRegistration {
        agent: Arc::new(FaultInjectingAgent::new(
            "claude",
            &[Role::Planner, Role::Reviewer],
        )),
        provider: Provider::ClaudeCode,
        estimated_micros_per_task: 1_000,
    });
    router.register(AgentRegistration {
        agent: Arc::new(FaultInjectingAgent::new("codex", &[Role::Worker])),
        provider: Provider::Codex,
        estimated_micros_per_task: 2_000,
    });
    router.register(AgentRegistration {
        agent: Arc::new(FaultInjectingAgent::new("ollama", &[Role::Summarize])),
        provider: Provider::Ollama,
        estimated_micros_per_task: 100,
    });

    let router = Arc::new(RwLock::new(router));

    let r1 = Arc::clone(&router);
    let r2 = Arc::clone(&router);
    let r3 = Arc::clone(&router);

    let (planner, worker, summarize) = tokio::join!(
        async move {
            let g = r1.read().await;
            g.select(&task_with_role(Role::Planner)).await.unwrap().id()
        },
        async move {
            let g = r2.read().await;
            g.select(&task_with_role(Role::Worker)).await.unwrap().id()
        },
        async move {
            let g = r3.read().await;
            g.select(&task_with_role(Role::Summarize)).await.unwrap().id()
        },
    );

    assert_eq!(planner.0, "claude");
    assert_eq!(worker.0, "codex");
    assert_eq!(summarize.0, "ollama");
}

// ---------------------------------------------------------------------------
// Task 5 — End-to-end soak
// ---------------------------------------------------------------------------

/// **PDX-106 task 5 (end-to-end soak).**
///
/// One integration test that walks the full chain:
///
/// * Build a router with three providers (Claude / Codex / Ollama).
/// * Dispatch a Worker task → Claude wins (cheapest cost while healthy).
/// * Wire an [`McpForwarder`]; `set_active(claude)` and confirm the
///   subscriber sees the change.
/// * Fault-inject Claude (kill its health flag).
/// * Re-dispatch the same Worker task → Codex wins (Claude excluded by the
///   health filter).
/// * `set_active(codex)`; confirm the subscriber sees the re-target.
/// * Fault-inject Codex.
/// * Re-dispatch → Ollama would win for `Summarize` but for Worker it errors
///   out — *which is the correct behavior* because Ollama only advertises
///   Summarize in this fixture.
/// * Switch the task to `Role::Summarize` → Ollama wins.
/// * `set_active(ollama)`; subscriber sees the final target.
///
/// Marked `#[ignore = "soak"]` so it only runs under
/// `cargo test --ignored`. The body is fast (sub-second), but soak tests
/// belong on a different cadence than the ordinary unit suite — that
/// matches the convention requested in PDX-106's task list.
#[tokio::test]
#[ignore = "soak"]
async fn soak_full_failover_chain_with_mcp_retarget() {
    // -- Build router & forwarder --
    let budget = ample_budget(&[Provider::ClaudeCode, Provider::Codex, Provider::Ollama]);
    let mut router = Router::new(budget);

    let claude = Arc::new(FaultInjectingAgent::new(
        "claude",
        &[Role::Worker, Role::Planner],
    ));
    let codex = Arc::new(FaultInjectingAgent::new("codex", &[Role::Worker]));
    let ollama = Arc::new(FaultInjectingAgent::new("ollama", &[Role::Summarize]));
    let claude_h = claude.health_handle();
    let codex_h = codex.health_handle();

    router.register(AgentRegistration {
        agent: claude,
        provider: Provider::ClaudeCode,
        estimated_micros_per_task: 1_000,
    });
    router.register(AgentRegistration {
        agent: codex,
        provider: Provider::Codex,
        estimated_micros_per_task: 2_000,
    });
    router.register(AgentRegistration {
        agent: ollama,
        provider: Provider::Ollama,
        estimated_micros_per_task: 100,
    });

    let forwarder = Arc::new(McpForwarder::new());
    let mut rx = forwarder.subscribe();

    // -- Step 1: dispatch Worker → Claude wins --
    let chosen = router.select(&task_with_role(Role::Worker)).await.unwrap();
    let id = chosen.id();
    assert_eq!(id.0, "claude");
    forwarder.set_active(id.clone());
    rx.changed().await.unwrap();
    assert_eq!(
        rx.borrow_and_update().clone(),
        ForwardingTarget::Agent(id.clone())
    );

    // -- Step 2: kill Claude --
    fault_inject_kill(&claude_h);

    // -- Step 3: re-dispatch Worker → Codex wins --
    let chosen = router.select(&task_with_role(Role::Worker)).await.unwrap();
    let id = chosen.id();
    assert_eq!(id.0, "codex");
    forwarder.set_active(id.clone());
    rx.changed().await.unwrap();
    assert_eq!(
        rx.borrow_and_update().clone(),
        ForwardingTarget::Agent(id.clone())
    );

    // -- Step 4: kill Codex --
    fault_inject_kill(&codex_h);

    // -- Step 5: Worker has no survivors --
    let err = router
        .select(&task_with_role(Role::Worker))
        .await
        .err()
        .expect("no Worker survivor");
    assert!(
        matches!(err, RouterError::AllUnhealthy),
        "Worker pool exhausted should be AllUnhealthy, got {err:?}"
    );

    // -- Step 6: Summarize → Ollama wins --
    let chosen = router
        .select(&task_with_role(Role::Summarize))
        .await
        .unwrap();
    let id = chosen.id();
    assert_eq!(id.0, "ollama");
    forwarder.set_active(id.clone());
    rx.changed().await.unwrap();
    assert_eq!(
        rx.borrow_and_update().clone(),
        ForwardingTarget::Agent(id)
    );
}

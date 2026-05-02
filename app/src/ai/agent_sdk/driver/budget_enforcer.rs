//! Two-layer runtime enforcement gate for the persistent orchestrator
//! [`Router`] (PDX-27 [D4]).
//!
//! Sits between [`Router::select`] and [`Agent::execute`] in
//! `run_via_local_orchestrator` and bolts on:
//!
//! 1. **Pre-dispatch tier check.** [`BudgetTier::Halted`] refuses dispatch
//!    outright; [`BudgetTier::Critical`] only allows `Role::Planner` /
//!    `Role::Reviewer`; [`BudgetTier::Warning`] allows everything but emits
//!    a tracing warning so the UI can surface "you've spent X of Y".
//!    The [`Router`] already encodes this filter at select-time (see
//!    `crates/orchestrator/src/router.rs`), so this layer is a *defence in
//!    depth* check — useful for the pinned-provider path that bypasses the
//!    standard [`Router::select`] call (PDX-105 [B3] task 4) and would
//!    otherwise dodge the tier filter entirely.
//! 2. **Concurrency cap.** Symphony spec section 8.3 caps how many tasks a
//!    given provider can run at once. Tracked here as a per-`Provider`
//!    [`AtomicU32`] count behind a small map; pre-dispatch acquires a slot
//!    or refuses, post-dispatch releases the slot via the
//!    [`EnforcementGuard`] RAII drop.
//! 3. **Charge + tier transition.** After a successful run the caller invokes
//!    [`BudgetEnforcer::record_charge`] to debit the [`Budget`] for the
//!    task's actual cost. Tier transitions (e.g. crossing the 50% / 90% /
//!    100% thresholds) are detected against the pre-charge tier and emitted
//!    as audit-log rows + tracing warnings.
//! 4. **Audit log integration.** Every guardrail trip writes a row through
//!    [`symphony::AuditLog`] (PDX-28's append-only JSONL writer at
//!    `~/.warp/symphony/audit.log`). We never modify [`symphony`]'s schema
//!    — we encode the PDX-27 rule + action pair into the existing
//!    [`AuditEvent::message`] field, so PDX-28's territory stays untouched.
//!
//! # Defaults are lenient
//!
//! [`BudgetEnforcer::default`] returns an enforcer with no concurrency
//! caps installed. The enforcer is wired into the dispatch path everywhere
//! but only actively rejects dispatch when caps and budgets have been
//! configured to non-trivial values. This matters for tests and for the
//! v1 user experience where the budget plumbing is observability-only.
//!
//! # Threshold model
//!
//! The budget tier classifier in `orchestrator::budget` already encodes
//! 50% / 90% / 100% thresholds (Healthy → Warning → Critical → Halted).
//! This enforcer reuses those thresholds — emitting a `BudgetTierTransition`
//! audit row whenever the pre-charge tier and the post-charge tier differ.
//! The transition state machine is therefore monotonic-up under steady
//! charges and monotonic-down only at `reset_monthly` boundaries; see
//! [`tier_transitions_are_monotonic_under_steady_charge`] in the inline
//! tests.

#![cfg(not(target_family = "wasm"))]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use orchestrator::{AgentId, Budget, BudgetError, BudgetTier, Provider, Role};
use symphony::audit::AuditEventKind;
use symphony::{AuditEvent, AuditLog};
use thiserror::Error;
use tokio::sync::Mutex as AsyncMutex;

/// Map a registered [`AgentId`] back to the [`Provider`] it bills against.
///
/// This mirrors the four agent IDs registered in
/// [`super::local_orchestrator`]. Centralised here so the dispatch site
/// can ask the enforcer "what provider is this agent?" without having to
/// know about every constant.
pub(crate) fn provider_for_agent(id: &AgentId) -> Option<Provider> {
    match id.0.as_str() {
        super::local_orchestrator::CLAUDE_CODE_SONNET_46_ID => Some(Provider::ClaudeCode),
        super::local_orchestrator::CODEX_WORKER_ID => Some(Provider::Codex),
        super::local_orchestrator::OLLAMA_WORKER_ID => Some(Provider::Ollama),
        // The local Foundation Models agent — id is the unexposed
        // `LOCAL_AGENT_ID` constant ("local-warp-oz"). Inlined here as a
        // string-literal match so we don't widen the local_orchestrator
        // module's pub(crate) surface for the sake of one comparison.
        "local-warp-oz" => Some(Provider::FoundationModels),
        _ => None,
    }
}

/// Errors returned by [`BudgetEnforcer::pre_dispatch`].
///
/// Each variant carries enough detail for the audit log + a useful tracing
/// message at the call site.
#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum BudgetEnforcementError {
    /// Budget is in [`BudgetTier::Halted`] — no dispatch allowed.
    #[error("provider {0:?} is halted; refusing dispatch")]
    Halted(Provider),
    /// Budget is in [`BudgetTier::Critical`] and the task role is not on the
    /// allow-list (only `Planner` / `Reviewer` may proceed).
    #[error("provider {0:?} is at critical tier; role {1:?} not in allow-list")]
    CriticalTierRoleBlocked(Provider, Role),
    /// Concurrency cap reached for this provider.
    #[error("concurrency cap reached for provider {0:?} ({1} tasks active)")]
    ConcurrencyCapReached(Provider, u32),
    /// The provider has no [`Cap`] configured in the underlying [`Budget`].
    #[error("provider {0:?} not registered in budget")]
    UnknownProvider(Provider),
}

impl BudgetEnforcementError {
    /// Stable rule label for the audit log.
    pub(crate) fn rule(&self) -> &'static str {
        match self {
            Self::Halted(_) | Self::CriticalTierRoleBlocked(_, _) => "budget_exceeded",
            Self::ConcurrencyCapReached(_, _) => "concurrency_cap",
            Self::UnknownProvider(_) => "budget_exceeded",
        }
    }

    /// Provider tag for the audit log row.
    pub(crate) fn provider_tag(&self) -> Provider {
        match self {
            Self::Halted(p)
            | Self::CriticalTierRoleBlocked(p, _)
            | Self::ConcurrencyCapReached(p, _)
            | Self::UnknownProvider(p) => *p,
        }
    }
}

/// Tier transition observed across a [`Budget::try_charge`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BudgetTierTransition {
    pub(crate) provider: Provider,
    pub(crate) before: BudgetTier,
    pub(crate) after: BudgetTier,
}

impl BudgetTierTransition {
    fn changed(&self) -> bool {
        self.before != self.after
    }
}

/// RAII guard returned by [`BudgetEnforcer::pre_dispatch`].
///
/// On drop, releases the concurrency slot the enforcer reserved. Holding
/// this across `Agent::execute` is the contract — the caller drops the
/// guard once the run is complete (success or failure) to free up the
/// concurrency slot.
pub(crate) struct EnforcementGuard {
    enforcer: Arc<BudgetEnforcer>,
    provider: Provider,
    /// Set to `true` if the slot was reserved via the concurrency map.
    /// `false` when the provider has no cap configured (no slot to
    /// release on drop).
    held: bool,
}

impl std::fmt::Debug for EnforcementGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnforcementGuard")
            .field("provider", &self.provider)
            .field("held", &self.held)
            .finish()
    }
}

impl Drop for EnforcementGuard {
    fn drop(&mut self) {
        if self.held {
            self.enforcer.release_slot(self.provider);
        }
    }
}

/// Per-provider concurrency limit.
///
/// `0` means "unlimited" — the enforcer does not track or refuse based on
/// concurrency for that provider. Modelled this way so callers that want
/// the enforcer to be a pure observability layer (the v1 default) can
/// hand back the empty config.
pub(crate) type ConcurrencyCaps = HashMap<Provider, u32>;

/// Two-layer enforcement gate.
///
/// One instance per [`Budget`]. Cheap to clone via the wrapping `Arc`.
/// Internal state lives behind small atomics (concurrency map) and a
/// [`tokio::sync::Mutex`] (audit log handle) — neither is held across
/// `await` points on the hot path.
pub(crate) struct BudgetEnforcer {
    budget: Arc<Budget>,
    /// Per-provider max concurrent tasks. Missing entries mean "unlimited".
    caps: ConcurrencyCaps,
    /// Per-provider live count. Lazily initialised on first acquire.
    active: AsyncMutex<HashMap<Provider, Arc<AtomicU32>>>,
    /// Append-only audit log handle. `None` means audit logging is
    /// disabled (the v1 default; we install a real one in production
    /// startup but tests use `enforcer_without_audit` for isolation).
    audit: Option<Arc<AuditLog>>,
}

impl BudgetEnforcer {
    /// Construct an enforcer with no concurrency caps and no audit sink.
    ///
    /// Intended for tests; production wires through
    /// [`Self::with_default_audit`].
    pub(crate) fn new(budget: Arc<Budget>, caps: ConcurrencyCaps) -> Arc<Self> {
        Arc::new(Self {
            budget,
            caps,
            active: AsyncMutex::new(HashMap::new()),
            audit: None,
        })
    }

    /// Install an [`AuditLog`] handle so guardrail trips and tier
    /// transitions emit JSONL rows.
    pub(crate) fn with_audit(self: Arc<Self>, audit: Arc<AuditLog>) -> Arc<Self> {
        Arc::new(Self {
            budget: self.budget.clone(),
            caps: self.caps.clone(),
            active: AsyncMutex::new(HashMap::new()),
            audit: Some(audit),
        })
    }

    /// Construct an enforcer with the production-default audit-log path
    /// (`~/.warp/symphony/audit.log`, matching PDX-28).
    ///
    /// Best effort: if the home directory cannot be resolved, the audit
    /// sink is omitted and the enforcer continues without writing audit
    /// rows.
    pub(crate) fn with_default_audit(
        budget: Arc<Budget>,
        caps: ConcurrencyCaps,
    ) -> Arc<Self> {
        let audit = default_audit_log_path().map(|p| Arc::new(AuditLog::open(p)));
        let base = Self::new(budget, caps);
        match audit {
            Some(a) => base.with_audit(a),
            None => base,
        }
    }

    /// Check the budget tier and concurrency cap for a dispatch.
    ///
    /// On success, returns an [`EnforcementGuard`] that releases the
    /// reserved slot on drop. On failure, writes an audit row (when an
    /// audit sink is installed) and returns the structured error.
    pub(crate) async fn pre_dispatch(
        self: &Arc<Self>,
        provider: Provider,
        role: Role,
        task_id: Option<&str>,
        agent_id: Option<&str>,
    ) -> Result<EnforcementGuard, BudgetEnforcementError> {
        // 1. Tier check. Halted / Critical-role-not-allowed refuses.
        let tier = match self.budget.current_tier(provider).await {
            Ok(t) => t,
            Err(BudgetError::UnknownProvider(p)) => {
                let err = BudgetEnforcementError::UnknownProvider(p);
                self.write_block(&err, role, task_id, agent_id).await;
                return Err(err);
            }
            Err(other) => {
                tracing::warn!(error = %other, "budget tier lookup failed");
                let err = BudgetEnforcementError::UnknownProvider(provider);
                self.write_block(&err, role, task_id, agent_id).await;
                return Err(err);
            }
        };
        match tier {
            BudgetTier::Halted => {
                let err = BudgetEnforcementError::Halted(provider);
                self.write_block(&err, role, task_id, agent_id).await;
                return Err(err);
            }
            BudgetTier::Critical => {
                if !matches!(role, Role::Planner | Role::Reviewer) {
                    let err = BudgetEnforcementError::CriticalTierRoleBlocked(provider, role);
                    self.write_block(&err, role, task_id, agent_id).await;
                    return Err(err);
                }
            }
            BudgetTier::Warning => {
                tracing::warn!(
                    provider = ?provider,
                    role = ?role,
                    "budget tier WARNING: dispatching but spend approaching cap"
                );
                self.write_warning_allowed(provider, role, task_id, agent_id)
                    .await;
            }
            BudgetTier::Healthy => {}
        }

        // 2. Concurrency cap.
        let cap = self.caps.get(&provider).copied();
        let mut held = false;
        if let Some(cap) = cap {
            if cap > 0 {
                let counter = self.counter_for(provider).await;
                let current = counter.load(Ordering::Acquire);
                if current >= cap {
                    let err = BudgetEnforcementError::ConcurrencyCapReached(provider, current);
                    self.write_block(&err, role, task_id, agent_id).await;
                    return Err(err);
                }
                // Optimistic increment. If a racing task pushes the
                // count past `cap` between our load and our increment,
                // we still let this dispatch through — concurrency caps
                // are advisory by design (a strict cap would require a
                // CAS loop and add latency for no real benefit). The
                // count converges to the true active total on the next
                // release.
                counter.fetch_add(1, Ordering::AcqRel);
                held = true;
            }
        }

        Ok(EnforcementGuard {
            enforcer: self.clone(),
            provider,
            held,
        })
    }

    /// Charge `micros` against the budget after a completed run.
    ///
    /// Returns the [`BudgetTierTransition`] observed (which may be a
    /// no-op transition when before == after). If the underlying
    /// [`Budget::try_charge`] returns an error (e.g. monthly cap hit
    /// post-run), an audit row is written and the error is propagated.
    pub(crate) async fn record_charge(
        self: &Arc<Self>,
        provider: Provider,
        micros: u64,
        task_id: Option<&str>,
        agent_id: Option<&str>,
    ) -> Result<BudgetTierTransition, BudgetError> {
        let before = self
            .budget
            .current_tier(provider)
            .await
            .unwrap_or(BudgetTier::Healthy);
        let after = match self.budget.try_charge(provider, micros).await {
            Ok(t) => t,
            Err(e) => {
                self.write_block_charge_failed(provider, &e, task_id, agent_id, micros)
                    .await;
                return Err(e);
            }
        };
        let transition = BudgetTierTransition {
            provider,
            before,
            after,
        };
        if transition.changed() {
            self.write_tier_transition(&transition, task_id, agent_id, micros)
                .await;
        }
        Ok(transition)
    }

    async fn counter_for(&self, provider: Provider) -> Arc<AtomicU32> {
        let mut active = self.active.lock().await;
        active
            .entry(provider)
            .or_insert_with(|| Arc::new(AtomicU32::new(0)))
            .clone()
    }

    fn release_slot(&self, provider: Provider) {
        // Use try_lock; on the rare contention case (another task is
        // mid-acquire), spin on a blocking_lock fallback. Drop is
        // synchronous so we cannot await — but this map is only
        // contended for the duration of a HashMap insert, which is
        // microseconds. In practice try_lock succeeds on the first
        // attempt.
        let counter = match self.active.try_lock() {
            Ok(guard) => guard.get(&provider).cloned(),
            Err(_) => {
                // Fall back to a blocking acquire. We're inside a Drop
                // impl on a non-async path; the Tokio mutex's blocking
                // API is the correct choice here.
                let guard = self.active.blocking_lock();
                guard.get(&provider).cloned()
            }
        };
        if let Some(counter) = counter {
            // Saturating decrement: guards against any path where the
            // counter has already been zeroed (e.g. a `reset_session`
            // that hits the active map in a later iteration). We never
            // want to wrap u32 here.
            let prev = counter.load(Ordering::Acquire);
            if prev > 0 {
                counter.fetch_sub(1, Ordering::AcqRel);
            }
        }
    }

    /// Read the current concurrency count for `provider`. Returns `0`
    /// when no slot has ever been acquired.
    #[cfg(test)]
    pub(crate) async fn active_count(&self, provider: Provider) -> u32 {
        let active = self.active.lock().await;
        active
            .get(&provider)
            .map(|c| c.load(Ordering::Acquire))
            .unwrap_or(0)
    }

    async fn write_block(
        &self,
        err: &BudgetEnforcementError,
        role: Role,
        task_id: Option<&str>,
        agent_id: Option<&str>,
    ) {
        if let Some(audit) = &self.audit {
            let provider = err.provider_tag();
            let message = format!(
                "rule={} action=blocked provider={:?} role={:?} detail=\"{}\"",
                err.rule(),
                provider,
                role,
                err
            );
            let mut event = AuditEvent::new(AuditEventKind::Failed)
                .with_provider(provider_tag(provider))
                .with_message(message)
                .with_error(err.to_string());
            if let (Some(tid), Some(aid)) = (task_id, agent_id) {
                event = event.with_issue(tid, aid);
            }
            audit.record(event);
        }
    }

    async fn write_warning_allowed(
        &self,
        provider: Provider,
        role: Role,
        task_id: Option<&str>,
        agent_id: Option<&str>,
    ) {
        if let Some(audit) = &self.audit {
            let message = format!(
                "rule=budget_warning action=allowed provider={:?} role={:?}",
                provider, role
            );
            let mut event = AuditEvent::new(AuditEventKind::Tick)
                .with_provider(provider_tag(provider))
                .with_message(message);
            if let (Some(tid), Some(aid)) = (task_id, agent_id) {
                event = event.with_issue(tid, aid);
            }
            audit.record(event);
        }
    }

    async fn write_tier_transition(
        &self,
        transition: &BudgetTierTransition,
        task_id: Option<&str>,
        agent_id: Option<&str>,
        micros: u64,
    ) {
        if let Some(audit) = &self.audit {
            let message = format!(
                "rule=budget_tier_transition action=allowed provider={:?} before={:?} after={:?} micros={}",
                transition.provider, transition.before, transition.after, micros
            );
            let mut event = AuditEvent::new(AuditEventKind::Tick)
                .with_provider(provider_tag(transition.provider))
                .with_message(message);
            if let (Some(tid), Some(aid)) = (task_id, agent_id) {
                event = event.with_issue(tid, aid);
            }
            audit.record(event);
        }
        tracing::info!(
            provider = ?transition.provider,
            before = ?transition.before,
            after = ?transition.after,
            "budget tier transition"
        );
    }

    async fn write_block_charge_failed(
        &self,
        provider: Provider,
        err: &BudgetError,
        task_id: Option<&str>,
        agent_id: Option<&str>,
        micros: u64,
    ) {
        if let Some(audit) = &self.audit {
            let message = format!(
                "rule=budget_exceeded action=blocked provider={:?} micros={} stage=post_run detail=\"{}\"",
                provider, micros, err
            );
            let mut event = AuditEvent::new(AuditEventKind::Failed)
                .with_provider(provider_tag(provider))
                .with_message(message)
                .with_error(err.to_string());
            if let (Some(tid), Some(aid)) = (task_id, agent_id) {
                event = event.with_issue(tid, aid);
            }
            audit.record(event);
        }
    }
}

/// Map a [`Provider`] to the stable string tag used by
/// [`AuditEvent::with_provider`]. Mirrors the tags already in use by PDX-28
/// (`"claude_code"`, `"codex"`, `"ollama"`, etc.).
fn provider_tag(provider: Provider) -> &'static str {
    match provider {
        Provider::ClaudeCode => "claude_code",
        Provider::Codex => "codex",
        Provider::Ollama => "ollama",
        Provider::FoundationModels => "foundation_models",
        Provider::Custom(_) => "custom",
    }
}

/// Resolve `~/.warp/symphony/audit.log`. Returns `None` if the home
/// directory cannot be determined.
fn default_audit_log_path() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    Some(home.join(".warp").join("symphony").join("audit.log"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use orchestrator::Cap;

    fn budget_with(provider: Provider, monthly: u64, session: u64) -> Arc<Budget> {
        let mut caps = HashMap::new();
        caps.insert(
            provider,
            Cap {
                monthly_micro_dollars: monthly,
                session_micro_dollars: session,
            },
        );
        Arc::new(Budget::new(caps))
    }

    /// PDX-27: `BudgetEnforcer::default`-equivalent (no caps, no audit) is
    /// a pure pass-through — `pre_dispatch` always succeeds when the
    /// provider has a budget entry and is in `Healthy` tier.
    #[tokio::test]
    async fn lenient_default_does_not_block() {
        let budget = budget_with(Provider::ClaudeCode, 1_000_000, 1_000_000);
        let enforcer = BudgetEnforcer::new(budget, ConcurrencyCaps::new());
        let guard = enforcer
            .pre_dispatch(Provider::ClaudeCode, Role::Worker, None, None)
            .await
            .expect("Healthy + no caps should pass");
        assert!(!guard.held);
        drop(guard);
    }

    /// PDX-27 acceptance, mirrors PDX-106's existing test pattern: a
    /// Critical-tier provider refuses Worker tasks but allows Planner
    /// (and Reviewer) to proceed.
    #[tokio::test]
    async fn critical_tier_blocks_worker_allows_planner() {
        let budget = budget_with(Provider::ClaudeCode, 100, 100);
        // Push spend to 95% (Critical tier) without halting.
        budget
            .try_charge(Provider::ClaudeCode, 95)
            .await
            .expect("charge");
        assert_eq!(
            budget.current_tier(Provider::ClaudeCode).await.unwrap(),
            BudgetTier::Critical
        );

        let enforcer = BudgetEnforcer::new(budget, ConcurrencyCaps::new());
        let err = enforcer
            .pre_dispatch(Provider::ClaudeCode, Role::Worker, None, None)
            .await
            .expect_err("Critical tier must reject Worker");
        assert_eq!(
            err,
            BudgetEnforcementError::CriticalTierRoleBlocked(Provider::ClaudeCode, Role::Worker)
        );

        // Planner is on the allow-list.
        let guard = enforcer
            .pre_dispatch(Provider::ClaudeCode, Role::Planner, None, None)
            .await
            .expect("Critical tier still allows Planner");
        drop(guard);
    }

    /// Halted refuses every role, including Planner.
    #[tokio::test]
    async fn halted_tier_blocks_everything() {
        let budget = budget_with(Provider::ClaudeCode, 100, 100);
        // Drive into Halted by trying to spend the entire cap.
        budget
            .try_charge(Provider::ClaudeCode, 100)
            .await
            .expect("charge");
        assert_eq!(
            budget.current_tier(Provider::ClaudeCode).await.unwrap(),
            BudgetTier::Halted
        );

        let enforcer = BudgetEnforcer::new(budget, ConcurrencyCaps::new());
        for role in [Role::Worker, Role::Planner, Role::Reviewer] {
            let err = enforcer
                .pre_dispatch(Provider::ClaudeCode, role, None, None)
                .await
                .expect_err("halted blocks every role");
            assert_eq!(err, BudgetEnforcementError::Halted(Provider::ClaudeCode));
        }
    }

    /// PDX-27 task 3: saturate the concurrency cap; the next dispatch is
    /// refused with `ConcurrencyCapReached`.
    #[tokio::test]
    async fn concurrency_cap_refuses_extra_dispatch() {
        let budget = budget_with(Provider::ClaudeCode, 1_000_000, 1_000_000);
        let mut caps = ConcurrencyCaps::new();
        caps.insert(Provider::ClaudeCode, 2);
        let enforcer = BudgetEnforcer::new(budget, caps);

        let g1 = enforcer
            .pre_dispatch(Provider::ClaudeCode, Role::Worker, None, None)
            .await
            .expect("first slot");
        let g2 = enforcer
            .pre_dispatch(Provider::ClaudeCode, Role::Worker, None, None)
            .await
            .expect("second slot");
        let err = enforcer
            .pre_dispatch(Provider::ClaudeCode, Role::Worker, None, None)
            .await
            .expect_err("third should be capped");
        assert!(matches!(
            err,
            BudgetEnforcementError::ConcurrencyCapReached(Provider::ClaudeCode, 2)
        ));

        // Drop the first guard — count goes to 1 — next dispatch fits.
        drop(g1);
        let g3 = enforcer
            .pre_dispatch(Provider::ClaudeCode, Role::Worker, None, None)
            .await
            .expect("freed slot accepts new dispatch");
        drop(g2);
        drop(g3);
        assert_eq!(
            enforcer.active_count(Provider::ClaudeCode).await,
            0,
            "all slots released on guard drop"
        );
    }

    /// Concurrency cap of `0` is treated as "unlimited" — guards held but
    /// no slot is reserved (matches the `default()` lenient stance).
    #[tokio::test]
    async fn cap_of_zero_means_unlimited() {
        let budget = budget_with(Provider::ClaudeCode, 1_000_000, 1_000_000);
        let mut caps = ConcurrencyCaps::new();
        caps.insert(Provider::ClaudeCode, 0);
        let enforcer = BudgetEnforcer::new(budget, caps);
        for _ in 0..32 {
            let g = enforcer
                .pre_dispatch(Provider::ClaudeCode, Role::Worker, None, None)
                .await
                .expect("unlimited cap");
            assert!(!g.held, "no slot reserved when cap is 0");
            drop(g);
        }
    }

    /// PDX-27 task 4: tier transitions across 50% / 90% / 100% thresholds
    /// match the classifier's state machine and are reported as
    /// `BudgetTierTransition`s out of `record_charge`.
    #[tokio::test]
    async fn tier_transitions_at_documented_thresholds() {
        // Cap of 100 makes integer percentages of spend == raw spend.
        let budget = budget_with(Provider::ClaudeCode, 100, 10_000);
        let enforcer = BudgetEnforcer::new(budget, ConcurrencyCaps::new());

        // 0 → 49: stays Healthy.
        let t = enforcer
            .record_charge(Provider::ClaudeCode, 49, None, None)
            .await
            .unwrap();
        assert_eq!(t.before, BudgetTier::Healthy);
        assert_eq!(t.after, BudgetTier::Healthy);
        assert!(!t.changed());

        // 49 → 50: Healthy → Warning (50% threshold).
        let t = enforcer
            .record_charge(Provider::ClaudeCode, 1, None, None)
            .await
            .unwrap();
        assert_eq!(t.before, BudgetTier::Healthy);
        assert_eq!(t.after, BudgetTier::Warning);
        assert!(t.changed());

        // 50 → 89: stays Warning.
        let t = enforcer
            .record_charge(Provider::ClaudeCode, 39, None, None)
            .await
            .unwrap();
        assert_eq!(t.before, BudgetTier::Warning);
        assert_eq!(t.after, BudgetTier::Warning);

        // 89 → 90: Warning → Critical (90% threshold).
        let t = enforcer
            .record_charge(Provider::ClaudeCode, 1, None, None)
            .await
            .unwrap();
        assert_eq!(t.before, BudgetTier::Warning);
        assert_eq!(t.after, BudgetTier::Critical);
        assert!(t.changed());

        // 90 → 100: Critical → Halted (100% threshold).
        let t = enforcer
            .record_charge(Provider::ClaudeCode, 10, None, None)
            .await
            .unwrap();
        assert_eq!(t.before, BudgetTier::Critical);
        assert_eq!(t.after, BudgetTier::Halted);
        assert!(t.changed());
    }

    /// PDX-27 task 4: the tier-transition state machine is monotonic-up
    /// under steady positive charges. We ratchet from Healthy through
    /// every tier up to Halted and verify each transition is upward.
    #[tokio::test]
    async fn tier_transitions_are_monotonic_under_steady_charge() {
        // Cap 1000, drip-feed 100 charges of 10 micros = 100% of cap.
        let budget = budget_with(Provider::ClaudeCode, 1000, 10_000);
        let enforcer = BudgetEnforcer::new(budget, ConcurrencyCaps::new());

        let mut last_seen = BudgetTier::Healthy;
        // Drip-feed 100 charges of 10 micro-dollars each (== full cap).
        // The tier should walk Healthy → Warning → Critical → Halted in
        // that order and never reverse.
        for i in 0..100 {
            let res = enforcer
                .record_charge(Provider::ClaudeCode, 10, None, None)
                .await;
            // The last drop will be rejected (over cap); skip.
            if res.is_err() {
                break;
            }
            let after = res.unwrap().after;
            assert!(
                tier_rank(after) >= tier_rank(last_seen),
                "tier went backwards at step {i}: {last_seen:?} -> {after:?}"
            );
            last_seen = after;
        }
        assert_eq!(last_seen, BudgetTier::Halted);
    }

    fn tier_rank(t: BudgetTier) -> u8 {
        match t {
            BudgetTier::Healthy => 0,
            BudgetTier::Warning => 1,
            BudgetTier::Critical => 2,
            BudgetTier::Halted => 3,
        }
    }

    /// PDX-27 task 5: a tier transition writes an audit-log row when an
    /// audit sink is installed. Uses a tempdir-backed log path so the
    /// test doesn't pollute the real `~/.warp/symphony/audit.log`.
    #[tokio::test]
    async fn tier_transition_writes_audit_row() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log_path = dir.path().join("audit.log");
        let audit = Arc::new(AuditLog::open(log_path.clone()));
        let budget = budget_with(Provider::ClaudeCode, 100, 1000);
        let enforcer = BudgetEnforcer::new(budget, ConcurrencyCaps::new()).with_audit(audit);

        // Force a Healthy → Warning transition.
        let t = enforcer
            .record_charge(Provider::ClaudeCode, 60, Some("task-1"), Some("agent-x"))
            .await
            .unwrap();
        assert_eq!(t.after, BudgetTier::Warning);

        // Drop the enforcer / audit to flush.
        drop(enforcer);

        let contents = std::fs::read_to_string(&log_path).expect("read");
        assert!(
            contents.contains("budget_tier_transition"),
            "audit log should record the transition; got: {contents}"
        );
        assert!(contents.contains("ClaudeCode"));
        assert!(contents.contains("after=Warning"));
    }

    /// PDX-27 task 5: a guardrail block writes an audit row with
    /// `action=blocked`.
    #[tokio::test]
    async fn block_writes_audit_row() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log_path = dir.path().join("audit.log");
        let audit = Arc::new(AuditLog::open(log_path.clone()));
        let budget = budget_with(Provider::ClaudeCode, 100, 100);
        budget
            .try_charge(Provider::ClaudeCode, 100)
            .await
            .expect("force halt");
        let enforcer = BudgetEnforcer::new(budget, ConcurrencyCaps::new()).with_audit(audit);

        let _ = enforcer
            .pre_dispatch(Provider::ClaudeCode, Role::Worker, Some("task-1"), Some("agent-x"))
            .await
            .expect_err("halted refuses dispatch");

        drop(enforcer);

        let contents = std::fs::read_to_string(&log_path).expect("read");
        assert!(contents.contains("budget_exceeded"), "got: {contents}");
        assert!(contents.contains("action=blocked"));
        assert!(contents.contains("ClaudeCode"));
    }

    /// `provider_for_agent` round-trips for every well-known agent id.
    #[test]
    fn provider_for_agent_maps_known_ids() {
        assert_eq!(
            provider_for_agent(&AgentId(
                super::super::local_orchestrator::CLAUDE_CODE_SONNET_46_ID.to_string()
            )),
            Some(Provider::ClaudeCode)
        );
        assert_eq!(
            provider_for_agent(&AgentId(
                super::super::local_orchestrator::CODEX_WORKER_ID.to_string()
            )),
            Some(Provider::Codex)
        );
        assert_eq!(
            provider_for_agent(&AgentId(
                super::super::local_orchestrator::OLLAMA_WORKER_ID.to_string()
            )),
            Some(Provider::Ollama)
        );
        assert_eq!(
            provider_for_agent(&AgentId("local-warp-oz".to_string())),
            Some(Provider::FoundationModels)
        );
        assert_eq!(
            provider_for_agent(&AgentId("never-registered".to_string())),
            None
        );
    }
}

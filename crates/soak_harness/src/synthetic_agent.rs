//! Synthetic agent driving the soak harness.
//!
//! Implements [`orchestrator::Agent`] with behaviour selected by decoding a
//! [`crate::fixtures::BehaviorTag`] from the Linear-style identifier the
//! harness builds in [`crate::fixtures::FixtureIssue::to_issue`]. The agent
//! is deterministic per-tag so invariants can predict the audit-log shape.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use futures_util::stream;
use orchestrator::{
    Agent, AgentError, AgentEvent, AgentEventStream, AgentId, Capabilities, Health, Role, Task,
};

use crate::fixtures::BehaviorTag;

/// A test-only agent the soak harness registers with the router.
///
/// Decodes the [`BehaviorTag`] suffix off `task.prompt` (the orchestrator
/// renders the prompt with `{{ issue.identifier }}` so the suffix is present)
/// and returns the matching event stream.
pub struct SyntheticAgent {
    id: AgentId,
    caps: Capabilities,
    /// Counters consumed by the harness metrics layer.
    invocations: Arc<AtomicU64>,
    /// Number of tasks that completed successfully via this agent.
    completed: Arc<AtomicU64>,
    /// Number of tasks that emitted a `Failed` event.
    failed: Arc<AtomicU64>,
    /// Number of `RequestTestDeletion` invocations observed (used to assert
    /// the auto_healing block fired downstream).
    test_deletion_attempts: Arc<AtomicU64>,
    /// Counters for budget-bomb invocations.
    budget_bomb_attempts: Arc<AtomicU64>,
}

impl SyntheticAgent {
    /// Construct an agent with the conventional id `synthetic-soak`.
    pub fn new() -> Self {
        let mut roles = HashSet::new();
        roles.insert(Role::Worker);
        Self {
            id: AgentId("synthetic-soak".to_string()),
            caps: Capabilities {
                roles,
                max_context_tokens: 200_000,
                supports_tools: true,
                supports_vision: false,
            },
            invocations: Arc::new(AtomicU64::new(0)),
            completed: Arc::new(AtomicU64::new(0)),
            failed: Arc::new(AtomicU64::new(0)),
            test_deletion_attempts: Arc::new(AtomicU64::new(0)),
            budget_bomb_attempts: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Total number of `execute` calls observed.
    pub fn invocations(&self) -> u64 {
        self.invocations.load(Ordering::Relaxed)
    }
    /// Number of streams that emitted `Completed`.
    pub fn completed(&self) -> u64 {
        self.completed.load(Ordering::Relaxed)
    }
    /// Number of streams that emitted `Failed`.
    pub fn failed(&self) -> u64 {
        self.failed.load(Ordering::Relaxed)
    }
    /// Number of `RequestTestDeletion` invocations seen.
    pub fn test_deletion_attempts(&self) -> u64 {
        self.test_deletion_attempts.load(Ordering::Relaxed)
    }
    /// Number of `BudgetBomb` invocations seen.
    pub fn budget_bomb_attempts(&self) -> u64 {
        self.budget_bomb_attempts.load(Ordering::Relaxed)
    }

    fn decode_tag(prompt: &str) -> BehaviorTag {
        // The orchestrator renders the prompt with the identifier embedded;
        // we scan for the longest matching suffix token. Default to
        // `HappyFast` when nothing matches so a non-soak run (e.g. unit
        // test using this agent without our fixture catalog) still
        // terminates cleanly.
        for tag in [
            BehaviorTag::Stalling,
            BehaviorTag::Failing,
            BehaviorTag::RefuseBadPrompt,
            BehaviorTag::BigDiff,
            BehaviorTag::RequestTestDeletion,
            BehaviorTag::BudgetBomb,
            BehaviorTag::HappySlow,
            BehaviorTag::HappyFast,
        ] {
            // Look for `-TAG` to avoid false positives from the fixture
            // titles (which never embed the suffix tokens).
            let needle = format!("-{}", tag.suffix());
            if prompt.contains(&needle) {
                return tag;
            }
        }
        BehaviorTag::HappyFast
    }
}

impl Default for SyntheticAgent {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Agent for SyntheticAgent {
    fn id(&self) -> AgentId {
        self.id.clone()
    }

    fn capabilities(&self) -> &Capabilities {
        &self.caps
    }

    async fn execute(&self, task: Task) -> Result<AgentEventStream, AgentError> {
        self.invocations.fetch_add(1, Ordering::Relaxed);
        let task_id = task.id;
        let tag = Self::decode_tag(&task.prompt);

        match tag {
            BehaviorTag::HappyFast => {
                self.completed.fetch_add(1, Ordering::Relaxed);
                let s = stream::iter(vec![
                    AgentEvent::Started { task_id },
                    AgentEvent::OutputChunk { text: "ok".into() },
                    AgentEvent::Completed {
                        task_id,
                        summary: Some("happy-fast done".into()),
                    },
                ]);
                Ok(Box::pin(s))
            }
            BehaviorTag::HappySlow => {
                let completed = Arc::clone(&self.completed);
                // tokio_stream::wrappers isn't in the dep set; build a
                // small async stream by hand via async-stream is also not
                // pulled in. Use `stream::unfold` with a step-state machine.
                let s = stream::unfold(0u8, move |step| {
                    let completed = Arc::clone(&completed);
                    async move {
                        match step {
                            0 => {
                                tokio::time::sleep(Duration::from_millis(50)).await;
                                Some((AgentEvent::Started { task_id }, 1))
                            }
                            1 => {
                                tokio::time::sleep(Duration::from_millis(200)).await;
                                Some((
                                    AgentEvent::OutputChunk {
                                        text: "thinking…".into(),
                                    },
                                    2,
                                ))
                            }
                            2 => {
                                tokio::time::sleep(Duration::from_millis(200)).await;
                                completed.fetch_add(1, Ordering::Relaxed);
                                Some((
                                    AgentEvent::Completed {
                                        task_id,
                                        summary: Some("happy-slow done".into()),
                                    },
                                    3,
                                ))
                            }
                            _ => None,
                        }
                    }
                });
                Ok(Box::pin(s))
            }
            BehaviorTag::Failing => {
                self.failed.fetch_add(1, Ordering::Relaxed);
                let s = stream::iter(vec![
                    AgentEvent::Started { task_id },
                    AgentEvent::Failed {
                        task_id,
                        error: "synthetic failure (Failing fixture)".into(),
                    },
                ]);
                Ok(Box::pin(s))
            }
            BehaviorTag::Stalling => {
                // Emit Started only; then hang. The orchestrator's
                // `reconcile_stalled` will abort the spawning JoinHandle
                // after `agent.stall_timeout_ms`. The stream itself never
                // terminates voluntarily.
                let s = stream::unfold(0u8, move |step| async move {
                    match step {
                        0 => Some((AgentEvent::Started { task_id }, 1)),
                        _ => {
                            // Hang forever (or until the JoinHandle is
                            // aborted by stall detection / task cancel).
                            tokio::time::sleep(Duration::from_secs(60 * 60 * 24)).await;
                            None
                        }
                    }
                });
                Ok(Box::pin(s))
            }
            BehaviorTag::RefuseBadPrompt => {
                self.failed.fetch_add(1, Ordering::Relaxed);
                let s = stream::iter(vec![
                    AgentEvent::Started { task_id },
                    AgentEvent::Failed {
                        task_id,
                        error: "refused: prompt rejected by guardrail".into(),
                    },
                ]);
                Ok(Box::pin(s))
            }
            BehaviorTag::BigDiff => {
                // Successful completion; the audit-log post-step will
                // observe a synthetic DiffGuardExceeded marker in the
                // OutputChunk text. The harness invariant layer scans for
                // this marker and treats it as expected for BigDiff
                // fixtures.
                self.completed.fetch_add(1, Ordering::Relaxed);
                let s = stream::iter(vec![
                    AgentEvent::Started { task_id },
                    AgentEvent::OutputChunk {
                        text: "[harness-marker] BigDiff produced an oversized diff (synthetic)".into(),
                    },
                    AgentEvent::Completed {
                        task_id,
                        summary: Some("big-diff done (expected DiffGuardExceeded follow-up)".into()),
                    },
                ]);
                Ok(Box::pin(s))
            }
            BehaviorTag::RequestTestDeletion => {
                self.test_deletion_attempts.fetch_add(1, Ordering::Relaxed);
                self.completed.fetch_add(1, Ordering::Relaxed);
                let s = stream::iter(vec![
                    AgentEvent::Started { task_id },
                    AgentEvent::ToolCall {
                        name: "delete_file".into(),
                        args: serde_json::json!({ "path": "tests/canary.rs" }),
                    },
                    AgentEvent::OutputChunk {
                        text: "[harness-marker] attempted-test-deletion (auto_healing should block)".into(),
                    },
                    AgentEvent::Completed {
                        task_id,
                        summary: Some("test-deletion attempt issued".into()),
                    },
                ]);
                Ok(Box::pin(s))
            }
            BehaviorTag::BudgetBomb => {
                self.budget_bomb_attempts.fetch_add(1, Ordering::Relaxed);
                self.completed.fetch_add(1, Ordering::Relaxed);
                let s = stream::iter(vec![
                    AgentEvent::Started { task_id },
                    AgentEvent::OutputChunk {
                        text: "[harness-marker] budget-bomb large-context simulated".into(),
                    },
                    AgentEvent::Completed {
                        task_id,
                        summary: Some("budget bomb completed".into()),
                    },
                ]);
                Ok(Box::pin(s))
            }
        }
    }

    fn health(&self) -> Health {
        Health {
            healthy: true,
            last_check: Utc::now(),
            error_rate: 0.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orchestrator::{Task, TaskContext, TaskId};

    fn task_for(identifier: &str) -> Task {
        Task {
            id: TaskId::new(),
            role: Role::Worker,
            prompt: format!("Issue {} please", identifier),
            context: TaskContext {
                cwd: std::env::temp_dir(),
                env: Default::default(),
                metadata: Default::default(),
            },
            budget_hint: None,
        }
    }

    #[tokio::test]
    async fn happy_fast_completes() {
        let agent = SyntheticAgent::new();
        let mut s = agent.execute(task_for("SOAK-0001-HFAST")).await.unwrap();
        let mut saw_completed = false;
        while let Some(ev) = futures_util::StreamExt::next(&mut s).await {
            if matches!(ev, AgentEvent::Completed { .. }) {
                saw_completed = true;
            }
        }
        assert!(saw_completed);
        assert_eq!(agent.completed(), 1);
    }

    #[tokio::test]
    async fn failing_emits_failed() {
        let agent = SyntheticAgent::new();
        let mut s = agent.execute(task_for("SOAK-0002-FAIL")).await.unwrap();
        let mut saw_failed = false;
        while let Some(ev) = futures_util::StreamExt::next(&mut s).await {
            if matches!(ev, AgentEvent::Failed { .. }) {
                saw_failed = true;
            }
        }
        assert!(saw_failed);
        assert_eq!(agent.failed(), 1);
    }

    #[tokio::test]
    async fn test_deletion_emits_tool_call() {
        let agent = SyntheticAgent::new();
        let mut s = agent.execute(task_for("SOAK-0003-TESTDEL")).await.unwrap();
        let mut saw_tool_call = false;
        while let Some(ev) = futures_util::StreamExt::next(&mut s).await {
            if let AgentEvent::ToolCall { name, .. } = &ev {
                if name == "delete_file" {
                    saw_tool_call = true;
                }
            }
        }
        assert!(saw_tool_call);
        assert_eq!(agent.test_deletion_attempts(), 1);
    }

    #[tokio::test]
    async fn unknown_identifier_defaults_to_happy_fast() {
        let agent = SyntheticAgent::new();
        let mut s = agent.execute(task_for("PDX-29")).await.unwrap();
        let mut saw_completed = false;
        while let Some(ev) = futures_util::StreamExt::next(&mut s).await {
            if matches!(ev, AgentEvent::Completed { .. }) {
                saw_completed = true;
            }
        }
        assert!(saw_completed);
    }
}

//! Symphony's poll-loop state machine and dispatcher (spec §7, §8).
//!
//! Owns the runtime state, the registered agents, the workspace manager,
//! and the audit log. On every tick:
//!
//!   1. Pull candidate issues from the tracker.
//!   2. Filter them per spec §8.2 (active state, not running/claimed,
//!      required label, concurrency cap respected).
//!   3. Sort by `(priority asc, created_at oldest, identifier lex)`.
//!   4. Dispatch the first eligible issue, streaming events into the audit
//!      log as the agent runs.
//!
//! Stall detection, retries, and reconciliation are deliberately omitted
//! from the MVP.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures_util::StreamExt;
use orchestrator::{
    Agent, AgentEvent, AgentEventStream, AgentId, Role, Router, RouterError, Task, TaskContext,
    TaskId,
};
use thiserror::Error;
use tokio::sync::RwLock;

use auto_healing::{TestDeletionCheck, TestDeletionDecision};

use crate::audit::{AuditEvent, AuditEventKind, AuditLog};
use crate::deploy_tool::{DeployTool, TOOL_NAME as DEPLOY_TOOL};
use crate::diff_guard::{DiffGuard, DiffGuardError};
use crate::linear_graphql::{LinearGraphQlTool, TOOL_NAME as LINEAR_GRAPHQL_TOOL};
use crate::simulator_tool::SimulatorTool;
use crate::numstat::collect_workspace_diffs;
use crate::reload::WorkflowHandle;
use crate::tracker::{Issue, TrackerError};
use crate::workflow::WorkflowDefinition;
use crate::workspace::{Workspace, WorkspaceError, WorkspaceManager};

/// Trait abstracting the tracker so tests can inject a mock.
#[async_trait]
pub trait IssueSource: Send + Sync {
    /// Fetch all issues whose state is in `active_states`.
    async fn fetch_candidate_issues(
        &self,
        active_states: &[String],
    ) -> Result<Vec<Issue>, TrackerError>;

    /// Post a comment to an issue. Default impl is a no-op so mock
    /// implementations in tests don't have to provide one.
    async fn add_comment(
        &self,
        _issue_id: &str,
        _body: &str,
    ) -> Result<(), TrackerError> {
        Ok(())
    }

    /// Transition an issue to a named state. Default no-op so mock
    /// implementations don't have to provide one.
    async fn transition_issue(
        &self,
        _issue_id: &str,
        _target_state_name: &str,
    ) -> Result<(), TrackerError> {
        Ok(())
    }
}

#[async_trait]
impl IssueSource for crate::tracker::LinearClient {
    async fn fetch_candidate_issues(
        &self,
        active_states: &[String],
    ) -> Result<Vec<Issue>, TrackerError> {
        crate::tracker::LinearClient::fetch_candidate_issues(self, active_states).await
    }

    async fn add_comment(
        &self,
        issue_id: &str,
        body: &str,
    ) -> Result<(), TrackerError> {
        crate::tracker::LinearClient::add_comment(self, issue_id, body).await
    }

    async fn transition_issue(
        &self,
        issue_id: &str,
        target_state_name: &str,
    ) -> Result<(), TrackerError> {
        crate::tracker::LinearClient::transition_issue(self, issue_id, target_state_name).await
    }
}

/// Top-level orchestrator errors.
#[derive(Debug, Error)]
pub enum OrchestratorError {
    /// Tracker call failed.
    #[error("tracker: {0}")]
    Tracker(#[from] TrackerError),
    /// Workspace setup failed.
    #[error("workspace: {0}")]
    Workspace(#[from] WorkspaceError),
    /// Router refused to assign an agent.
    #[error("router: {0}")]
    Router(String),
    /// Diff guard rejected the run.
    #[error("diff guard: {0}")]
    DiffGuard(#[from] DiffGuardError),
    /// Catch-all for unexpected failures.
    #[error("{0}")]
    Other(String),
}

impl From<RouterError> for OrchestratorError {
    fn from(value: RouterError) -> Self {
        OrchestratorError::Router(value.to_string())
    }
}

/// Aggregate outcome of one agent execution, accumulated from the event
/// stream and consumed by the post-run handler to build a Linear comment.
#[derive(Debug, Default, Clone)]
struct RunOutcome {
    success: bool,
    summary: Option<String>,
    error: Option<String>,
}

/// Bookkeeping for a currently-running issue dispatch.
#[derive(Debug, Clone)]
pub struct RunningEntry {
    /// Linear issue id.
    pub issue_id: String,
    /// Linear identifier (`PDX-12`).
    pub identifier: String,
    /// On-disk workspace path.
    pub workspace_path: PathBuf,
    /// Wall-clock instant the dispatch started.
    pub started_at: Instant,
    /// Most recent moment we observed an `AgentEvent` from this run.
    /// Used by stall detection in `tick`. Defaults to `started_at`.
    pub last_event_at: Instant,
    /// Selected agent id.
    pub agent_id: AgentId,
    /// Handle to the spawned tokio task driving this run, used by stall
    /// detection to abort. `Arc` so the handle can be cloned for tests
    /// without consuming.
    pub task_handle: Option<Arc<tokio::task::JoinHandle<()>>>,
    /// PDX-112 §10.5: most recent JSON response observed for an in-flight
    /// `linear_graphql` tool call. Populated by `consume_stream` whenever
    /// the agent issues a `linear_graphql` ToolCall and Symphony executes
    /// it on the agent's behalf. Tests use this to assert the daemon-side
    /// execution path actually fired without round-tripping the result
    /// back into the subprocess.
    pub last_linear_graphql_result: Option<serde_json::Value>,
    /// PDX-114 [E2]: most recent JSON envelope observed for an in-flight
    /// `deploy` tool call. Same role as `last_linear_graphql_result`,
    /// scoped to the deploy tool. Tests use this to assert the daemon
    /// created a workflow instance on the agent's behalf without the
    /// agent ever invoking `wrangler deploy` / `gh release` itself.
    pub last_deploy_tool_result: Option<serde_json::Value>,
}

/// Pending retry of a failed or stalled run.
#[derive(Debug, Clone)]
pub struct RetryEntry {
    /// Linear issue id.
    pub issue_id: String,
    /// Linear identifier.
    pub identifier: String,
    /// Attempt counter. The first retry is `1`.
    pub attempt: u32,
    /// Earliest moment this retry is allowed to fire.
    pub due_at: Instant,
    /// Most recent error string (audit-only).
    pub error: Option<String>,
}

/// Mutable runtime state, guarded by an `RwLock` so that the tick task and
/// the spawned agent tasks can both mutate it.
#[derive(Default, Debug)]
pub struct RuntimeState {
    /// Issues actively being worked on, keyed by issue id.
    pub running: HashMap<String, RunningEntry>,
    /// Issues that have been claimed in this tick but haven't reached
    /// `running` state yet — kept as a defensive guard against double
    /// dispatch within a single tick.
    pub claimed: HashSet<String>,
    /// Issues we've completed in this process lifetime.
    pub completed: HashSet<String>,
    /// Pending retries keyed by issue id.
    pub retry_queue: HashMap<String, RetryEntry>,
}

/// Orchestrator core.
pub struct Orchestrator {
    /// Live, swap-able workflow definition. Reads go through
    /// [`WorkflowHandle::load`] to obtain a per-call snapshot
    /// (`Arc<WorkflowDefinition>`); writes are performed by the live-reload
    /// watcher (see [`crate::reload`]). PDX-111.
    workflow: Arc<WorkflowHandle>,
    tracker: Arc<dyn IssueSource>,
    workspaces: Arc<WorkspaceManager>,
    router: Arc<Router>,
    state: Arc<RwLock<RuntimeState>>,
    audit: Arc<AuditLog>,
    diff_guard: DiffGuard,
    /// PDX-28 [D5] auto-healing: test-deletion guardrail. Always wired
    /// with the default pattern set; the spec acceptance criterion
    /// explicitly calls out catching `app/src/auth/auth_manager_test.rs`
    /// deletions, so we keep this on by default rather than making it
    /// opt-in via the workflow front matter.
    test_deletion: TestDeletionCheck,
    /// PDX-112 §10.5 — daemon-mediated `linear_graphql` tool. Optional so
    /// integration tests with mock trackers can omit it; production wiring
    /// always installs the tool wrapping the same `LinearClient` used for
    /// candidate fetching, comment posting, and state transitions.
    linear_graphql_tool: Option<LinearGraphQlTool>,
    /// PDX-113 — daemon-mediated `simulator` tool. Optional; macOS hosts
    /// install a real [`XcrunSimulatorExecutor`]-backed tool, other hosts
    /// either omit the tool entirely or install one whose dispatch always
    /// returns `unsupported_platform`. The agent never sees `xcrun` or
    /// any iOS / macOS credentials directly — only structured JSON
    /// responses cross the daemon boundary.
    #[allow(dead_code)] // wired into agent boundary in a follow-up; see PDX-113.
    simulator_tool: Option<SimulatorTool>,
    /// PDX-114 [E2] — daemon-mediated `deploy` tool. Optional; production
    /// wiring installs a [`DeployTool`] whose
    /// [`crate::deploy_tool::DeployWorkflowClient`] talks to the
    /// control-plane Worker. The agent never sees the `wrangler` /
    /// `gh release` / `cargo publish` / `npm publish` invocation
    /// directly — it only receives `{ workflow_instance_id,
    /// status: "awaiting_approval" }` and learns to wait for the human
    /// approval to land via Linear.
    deploy_tool: Option<DeployTool>,
}

impl Orchestrator {
    /// Construct a new orchestrator wiring all collaborators.
    ///
    /// Accepts an owned [`WorkflowDefinition`] for ergonomics; it is wrapped
    /// in a fresh [`WorkflowHandle`]. Callers that already hold a handle
    /// (e.g. `main.rs`, which shares the handle with the live-reload
    /// watcher) should use [`Orchestrator::with_handle`] instead.
    pub fn new(
        workflow: WorkflowDefinition,
        tracker: Arc<dyn IssueSource>,
        workspaces: Arc<WorkspaceManager>,
        router: Arc<Router>,
        audit: Arc<AuditLog>,
    ) -> Self {
        Self::with_handle(
            Arc::new(WorkflowHandle::new(workflow)),
            tracker,
            workspaces,
            router,
            audit,
        )
    }

    /// Construct a new orchestrator with an externally-owned
    /// [`WorkflowHandle`]. Use this in production so the live-reload watcher
    /// and the orchestrator share the same handle and the next tick after a
    /// successful reload picks up the new config without restart (PDX-111).
    pub fn with_handle(
        workflow: Arc<WorkflowHandle>,
        tracker: Arc<dyn IssueSource>,
        workspaces: Arc<WorkspaceManager>,
        router: Arc<Router>,
        audit: Arc<AuditLog>,
    ) -> Self {
        let max_diff = workflow.load().config.agent.max_diff_lines;
        Self {
            workflow,
            tracker,
            workspaces,
            router,
            state: Arc::new(RwLock::new(RuntimeState::default())),
            audit,
            diff_guard: DiffGuard::new(max_diff),
            test_deletion: TestDeletionCheck::default(),
            linear_graphql_tool: None,
            simulator_tool: None,
            deploy_tool: None,
        }
    }

    /// Install the daemon-mediated `linear_graphql` tool (PDX-112).
    /// Production wiring threads in a tool wrapping the same `LinearClient`
    /// used for the rest of tracker IO; tests can omit it.
    pub fn with_linear_graphql_tool(mut self, tool: LinearGraphQlTool) -> Self {
        self.linear_graphql_tool = Some(tool);
        self
    }

    /// Whether the daemon-mediated `linear_graphql` tool is installed.
    pub fn has_linear_graphql_tool(&self) -> bool {
        self.linear_graphql_tool.is_some()
    }

    /// Install the daemon-mediated `simulator` tool (PDX-113). Mirrors
    /// [`Self::with_linear_graphql_tool`]: the daemon owns the executor
    /// and the agent only ever sees JSON responses, so no `xcrun` /
    /// simulator credentials cross the subprocess boundary.
    pub fn with_simulator_tool(mut self, tool: SimulatorTool) -> Self {
        self.simulator_tool = Some(tool);
        self
    }

    /// Whether the daemon-mediated `simulator` tool is installed.
    pub fn has_simulator_tool(&self) -> bool {
        self.simulator_tool.is_some()
    }

    /// Install the daemon-mediated `deploy` tool (PDX-114 [E2]). Mirrors
    /// [`Self::with_linear_graphql_tool`]: the daemon owns the deploy
    /// pipeline (workflow instance creation, secrets, approver list)
    /// and the agent only ever sees a `{ workflow_instance_id,
    /// status: "awaiting_approval" }` envelope, so no Cloudflare /
    /// npm / GitHub / Cargo deploy credentials cross the subprocess
    /// boundary.
    pub fn with_deploy_tool(mut self, tool: DeployTool) -> Self {
        self.deploy_tool = Some(tool);
        self
    }

    /// Whether the daemon-mediated `deploy` tool is installed.
    pub fn has_deploy_tool(&self) -> bool {
        self.deploy_tool.is_some()
    }

    /// Snapshot the live workflow definition. Cheap (`Arc::clone` under a
    /// brief read lock); call once per logical operation that needs the
    /// config and reuse the snapshot — both for consistency *within* an
    /// operation and to take only one lock round-trip.
    pub fn workflow_snapshot(&self) -> Arc<WorkflowDefinition> {
        self.workflow.load()
    }

    /// Snapshot of current runtime state. Useful for tests.
    pub async fn state_snapshot(&self) -> (HashMap<String, RunningEntry>, HashSet<String>) {
        let s = self.state.read().await;
        (s.running.clone(), s.completed.clone())
    }

    /// Main loop: tick on `polling.interval_ms` until `shutdown` resolves.
    ///
    /// The interval is re-read from the live workflow handle at the top of
    /// every loop iteration, so a `polling.interval_ms` edit (PDX-111 live
    /// reload) takes effect on the *next* tick without restart.
    pub async fn run(self: Arc<Self>, mut shutdown: tokio::sync::oneshot::Receiver<()>) {
        loop {
            let interval =
                Duration::from_millis(self.workflow.load().config.polling.interval_ms);
            tokio::select! {
                _ = tokio::time::sleep(interval) => {
                    if let Err(e) = self.tick().await {
                        tracing::warn!(error = %e, "tick failed");
                    }
                }
                _ = &mut shutdown => {
                    tracing::info!("shutdown requested; exiting orchestrator loop");
                    break;
                }
            }
        }
    }

    /// One iteration of the poll loop.
    ///
    /// Sequence per Symphony spec §8.1:
    /// 1. Reconcile (stall detection on running issues — §8.5 Part A)
    /// 2. Process retry queue (any due retries → re-dispatch)
    /// 3. Fetch candidate issues + run dispatch preflight + dispatch
    pub async fn tick(self: &Arc<Self>) -> Result<(), OrchestratorError> {
        self.audit.record(AuditEvent::new(AuditEventKind::Tick));

        // §8.5 Part A: stall detection.
        self.reconcile_stalled().await;

        // §8.4: process retry queue (issues whose backoff has elapsed).
        self.process_retry_queue().await;

        // PDX-111: snapshot the live workflow once per tick. Live edits
        // arrive between ticks; reads within a single tick are consistent.
        let wf = self.workflow.load();
        let active = wf.config.tracker.active_states.clone();
        let candidates = self.tracker.fetch_candidate_issues(&active).await?;

        // Snapshot state to filter without holding the lock during the
        // filter pass — we only re-acquire it to mutate `claimed`/`running`.
        let (running, claimed, completed) = {
            let s = self.state.read().await;
            (
                s.running.keys().cloned().collect::<HashSet<_>>(),
                s.claimed.clone(),
                s.completed.clone(),
            )
        };

        let max = wf.config.agent.max_concurrent_agents;
        if running.len() >= max {
            tracing::debug!(
                running = running.len(),
                cap = max,
                "concurrency cap reached; skipping tick"
            );
            return Ok(());
        }

        let label_lc = wf.config.agent.agent_label_required.to_lowercase();
        let active_set: HashSet<&str> = active.iter().map(|s| s.as_str()).collect();
        let mut eligible: Vec<Issue> = candidates
            .into_iter()
            .filter(|i| active_set.contains(i.state.as_str()))
            .filter(|i| !running.contains(&i.id))
            .filter(|i| !claimed.contains(&i.id))
            .filter(|i| !completed.contains(&i.id))
            .filter(|i| i.labels.iter().any(|l| l == &label_lc))
            .collect();

        // §8.2 sort: priority asc (lowest number = most urgent), then
        // created_at oldest, then identifier lex. Treat absent priority as
        // "very low" (after every numbered priority). Linear convention is
        // 0=No priority, 1=Urgent, 2=High, 3=Med, 4=Low — we still sort
        // ascending so callers should normalize 0 to a sentinel if desired.
        eligible.sort_by(|a, b| {
            a.priority
                .unwrap_or(i32::MAX)
                .cmp(&b.priority.unwrap_or(i32::MAX))
                .then_with(|| match (a.created_at, b.created_at) {
                    (Some(x), Some(y)) => x.cmp(&y),
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, None) => std::cmp::Ordering::Equal,
                })
                .then_with(|| a.identifier.cmp(&b.identifier))
        });

        if let Some(issue) = eligible.into_iter().next() {
            self.dispatch(issue).await?;
        }

        Ok(())
    }

    /// Dispatch a single issue: claim, materialize workspace, render prompt,
    /// route to an agent, spawn the streaming task. The streaming half runs
    /// asynchronously; this function returns once the agent task has been
    /// spawned and the issue has been promoted from `claimed` to `running`.
    pub async fn dispatch(self: &Arc<Self>, issue: Issue) -> Result<(), OrchestratorError> {
        // Atomic claim.
        {
            let mut s = self.state.write().await;
            if s.running.contains_key(&issue.id) || s.claimed.contains(&issue.id) {
                return Ok(());
            }
            s.claimed.insert(issue.id.clone());
        }
        self.audit.record(
            AuditEvent::new(AuditEventKind::Claimed)
                .with_issue(issue.id.clone(), issue.identifier.clone()),
        );

        let workspace = match self.workspaces.ensure_for(&issue).await {
            Ok(ws) => ws,
            Err(e) => {
                let mut s = self.state.write().await;
                s.claimed.remove(&issue.id);
                return Err(e.into());
            }
        };

        // PDX-111: snapshot the workflow at dispatch time so this run keeps
        // operating against the config that was live when it started, even
        // if the live-reload watcher swaps in a new definition while the
        // agent is mid-flight.
        let wf_snapshot = self.workflow.load();
        let prompt = wf_snapshot
            .render_prompt(&issue, None)
            .map_err(|e| OrchestratorError::Other(e.to_string()))?;

        // §10.5 / PDX-112: the agent runs in an env that NEVER carries
        // Linear credentials. The `linear_graphql` tool is the only path
        // to Linear and is mediated entirely by the daemon. Sanitize even
        // though we start from an empty map so a future caller adding env
        // forwarding doesn't accidentally regress the invariant.
        let mut env = HashMap::new();
        env.retain(|k: &String, _| !is_linear_secret_key(k));

        let task = Task {
            id: TaskId::new(),
            role: Role::Worker,
            prompt,
            context: TaskContext {
                cwd: workspace.path.clone(),
                env,
                metadata: HashMap::new(),
            },
            budget_hint: None,
        };

        let agent: Arc<dyn Agent> = {
            let agent = self.router.select(&task).await?;
            agent.clone()
        };
        let agent_id = agent.id();

        let now = Instant::now();
        // Promote claimed → running. We insert a placeholder JoinHandle
        // first; the real handle is set immediately after spawn so stall
        // detection can abort it.
        {
            let mut s = self.state.write().await;
            s.claimed.remove(&issue.id);
            s.running.insert(
                issue.id.clone(),
                RunningEntry {
                    issue_id: issue.id.clone(),
                    identifier: issue.identifier.clone(),
                    workspace_path: workspace.path.clone(),
                    started_at: now,
                    last_event_at: now,
                    agent_id: agent_id.clone(),
                    task_handle: None,
                    last_linear_graphql_result: None,
                    last_deploy_tool_result: None,
                },
            );
        }

        self.audit.record(
            AuditEvent::new(AuditEventKind::Dispatched)
                .with_issue(issue.id.clone(), issue.identifier.clone())
                .with_provider(agent_id.0.clone()),
        );

        // Run before_run hook (fatal on failure).
        if let Err(e) = self.workspaces.run_before_run_hook(&workspace).await {
            self.cleanup_running(&issue.id).await;
            return Err(e.into());
        }

        let this = Arc::clone(self);
        let issue_id_for_task = issue.id.clone();
        let handle = tokio::spawn(async move {
            this.run_agent(issue, workspace, agent, task).await;
        });
        // Store the handle so stall detection can abort.
        {
            let mut s = self.state.write().await;
            if let Some(entry) = s.running.get_mut(&issue_id_for_task) {
                entry.task_handle = Some(Arc::new(handle));
            }
        }

        Ok(())
    }

    async fn run_agent(
        self: Arc<Self>,
        issue: Issue,
        workspace: Workspace,
        agent: Arc<dyn Agent>,
        task: Task,
    ) {
        let provider = agent.id().0.clone();
        let stream = match agent.execute(task).await {
            Ok(s) => s,
            Err(e) => {
                self.audit.record(
                    AuditEvent::new(AuditEventKind::Failed)
                        .with_issue(issue.id.clone(), issue.identifier.clone())
                        .with_provider(provider.clone())
                        .with_error(e.to_string()),
                );
                self.workspaces.run_after_run_hook(&workspace).await;
                self.cleanup_running(&issue.id).await;
                return;
            }
        };

        let outcome = self.consume_stream(stream, &issue, &provider).await;
        self.run_post_steps(&issue, &workspace, &provider, outcome).await;
    }

    async fn consume_stream(
        &self,
        mut stream: AgentEventStream,
        issue: &Issue,
        provider: &str,
    ) -> RunOutcome {
        let mut outcome = RunOutcome::default();
        while let Some(ev) = stream.next().await {
            // Update last_event_at on every event so stall detection
            // measures inactivity, not absolute runtime.
            {
                let mut s = self.state.write().await;
                if let Some(entry) = s.running.get_mut(&issue.id) {
                    entry.last_event_at = Instant::now();
                }
            }
            match ev {
                AgentEvent::Started { task_id } => {
                    tracing::info!(?task_id, "agent started");
                }
                AgentEvent::OutputChunk { text } => {
                    self.audit.record(
                        AuditEvent::new(AuditEventKind::Chunk)
                            .with_issue(issue.id.clone(), issue.identifier.clone())
                            .with_provider(provider.to_string())
                            .with_message(truncate(&text, 256)),
                    );
                }
                AgentEvent::ToolCall { name, args } => {
                    self.audit.record(
                        AuditEvent::new(AuditEventKind::ToolCall)
                            .with_issue(issue.id.clone(), issue.identifier.clone())
                            .with_provider(provider.to_string())
                            .with_message(name.clone()),
                    );
                    // PDX-112 §10.5: intercept `linear_graphql` calls, run
                    // them daemon-side, and emit a synthetic tool_result so
                    // the broader stack (and the audit log) sees the
                    // structured response. If the tool isn't installed, we
                    // emit a clear error rather than silently dropping the
                    // call.
                    if name == LINEAR_GRAPHQL_TOOL {
                        let result = match &self.linear_graphql_tool {
                            Some(tool) => tool.execute(&args).await,
                            None => serde_json::json!({
                                "data": serde_json::Value::Null,
                                "errors": [{
                                    "message": "linear_graphql tool not configured on daemon",
                                    "extensions": { "kind": "not_configured" }
                                }],
                            }),
                        };
                        self.audit.record(
                            AuditEvent::new(AuditEventKind::ToolResult)
                                .with_issue(issue.id.clone(), issue.identifier.clone())
                                .with_provider(provider.to_string())
                                .with_message(LINEAR_GRAPHQL_TOOL.to_string()),
                        );
                        // Stash the result on the running entry's metadata
                        // so callers (and tests) can introspect the most
                        // recent linear_graphql payload.
                        let mut s = self.state.write().await;
                        if let Some(entry) = s.running.get_mut(&issue.id) {
                            entry.last_linear_graphql_result = Some(result);
                        }
                    } else if name == DEPLOY_TOOL {
                        // PDX-114 [E2]: intercept `deploy` tool calls and
                        // route them through the daemon-side
                        // [`DeployTool`], which validates against
                        // `WORKFLOW.md` and POSTs to the control-plane
                        // Worker to create a `DeployWorkflow` instance.
                        // The agent only ever sees the structured
                        // envelope returned here — never the underlying
                        // `wrangler` / `gh release` / `cargo publish` /
                        // `npm publish` invocation.
                        let result = match &self.deploy_tool {
                            Some(tool) => tool.execute(&args).await,
                            None => serde_json::json!({
                                "error": {
                                    "kind": "not_configured",
                                    "message": "deploy tool not configured on daemon",
                                }
                            }),
                        };
                        self.audit.record(
                            AuditEvent::new(AuditEventKind::ToolResult)
                                .with_issue(issue.id.clone(), issue.identifier.clone())
                                .with_provider(provider.to_string())
                                .with_message(DEPLOY_TOOL.to_string()),
                        );
                        let mut s = self.state.write().await;
                        if let Some(entry) = s.running.get_mut(&issue.id) {
                            entry.last_deploy_tool_result = Some(result);
                        }
                    }
                }
                AgentEvent::ToolResult { name, .. } => {
                    self.audit.record(
                        AuditEvent::new(AuditEventKind::ToolResult)
                            .with_issue(issue.id.clone(), issue.identifier.clone())
                            .with_provider(provider.to_string())
                            .with_message(name),
                    );
                }
                AgentEvent::Completed { task_id, summary } => {
                    let msg = summary.unwrap_or_else(|| format!("task {}", task_id));
                    self.audit.record(
                        AuditEvent::new(AuditEventKind::Completed)
                            .with_issue(issue.id.clone(), issue.identifier.clone())
                            .with_provider(provider.to_string())
                            .with_message(msg.clone()),
                    );
                    outcome.success = true;
                    outcome.summary = Some(msg);
                }
                AgentEvent::Failed { task_id, error } => {
                    let err = format!("{} (task {})", error, task_id);
                    self.audit.record(
                        AuditEvent::new(AuditEventKind::Failed)
                            .with_issue(issue.id.clone(), issue.identifier.clone())
                            .with_provider(provider.to_string())
                            .with_error(err.clone()),
                    );
                    outcome.success = false;
                    outcome.error = Some(err);
                }
            }
        }
        outcome
    }

    async fn run_post_steps(
        &self,
        issue: &Issue,
        workspace: &Workspace,
        provider: &str,
        outcome: RunOutcome,
    ) {
        let mut diff_summary = match self.diff_guard.check(&workspace.path).await {
            Ok(stat) => {
                tracing::info!(
                    insertions = stat.insertions,
                    deletions = stat.deletions,
                    "diff guard ok"
                );
                format!("+{} -{} lines", stat.insertions, stat.deletions)
            }
            Err(e) => {
                self.audit.record(
                    AuditEvent::new(AuditEventKind::DiffGuardExceeded)
                        .with_issue(issue.id.clone(), issue.identifier.clone())
                        .with_error(e.to_string()),
                );
                format!("diff guard exceeded: {}", e)
            }
        };

        // PDX-28 [D5]: test-deletion guardrail.
        match collect_workspace_diffs(&workspace.path).await {
            Ok(diffs) => {
                if let TestDeletionDecision::Block {
                    reason,
                    offending_path,
                } = self.test_deletion.evaluate(&diffs)
                {
                    tracing::warn!(
                        issue = %issue.identifier,
                        path = %offending_path,
                        "test-deletion guardrail tripped"
                    );
                    self.audit.record(
                        AuditEvent::new(AuditEventKind::TestDeletionBlocked)
                            .with_issue(issue.id.clone(), issue.identifier.clone())
                            .with_message(offending_path)
                            .with_error(reason.clone()),
                    );
                    diff_summary.push_str(&format!("\n  test-deletion blocked: {}", reason));
                }
            }
            Err(e) => {
                tracing::warn!(
                    issue = %issue.identifier,
                    error = %e,
                    "test-deletion numstat collect failed; skipping check"
                );
            }
        }

        self.workspaces.run_after_run_hook(workspace).await;

        // Optional Linear write-back: post a comment summarizing the run.
        // Skipped if `agent.comment_on_completion = false` in WORKFLOW.md
        // OR if the issue source is a mock that doesn't implement add_comment.
        let wf = self.workflow.load();
        if wf.config.agent.comment_on_completion {
            let body = self.compose_completion_comment(provider, &diff_summary, &outcome);
            if let Err(e) = self.tracker.add_comment(&issue.id, &body).await {
                tracing::warn!(
                    issue = %issue.identifier,
                    error = %e,
                    "failed to post Linear comment; continuing"
                );
                // Comment failures don't fail the run — observability only.
            }
        }

        // Optional state transition. Successful runs go to
        // handoff_state_on_success (e.g. "In Review"); failures go to
        // handoff_state_on_failure (e.g. "Backlog") if configured.
        let target = if outcome.success {
            wf.config.agent.handoff_state_on_success.as_deref()
        } else {
            wf.config.agent.handoff_state_on_failure.as_deref()
        };
        if let Some(target_state) = target {
            if let Err(e) = self.tracker.transition_issue(&issue.id, target_state).await {
                tracing::warn!(
                    issue = %issue.identifier,
                    target = target_state,
                    error = %e,
                    "failed to transition Linear state; continuing"
                );
            } else {
                tracing::info!(
                    issue = %issue.identifier,
                    target = target_state,
                    "transitioned Linear state"
                );
            }
        }

        // Failure path: schedule a retry with backoff before we drop the
        // running entry. Successes go straight to `completed`.
        if !outcome.success {
            // Don't loop on the issue forever — schedule_retry handles cap.
            self.schedule_retry(
                issue.id.clone(),
                issue.identifier.clone(),
                1,
                outcome.error.clone(),
            )
            .await;
        }

        let mut s = self.state.write().await;
        s.running.remove(&issue.id);
        if outcome.success {
            s.completed.insert(issue.id.clone());
        }
    }

    /// Build the human-readable Markdown body posted back to the Linear
    /// issue when an agent run completes.
    fn compose_completion_comment(
        &self,
        provider: &str,
        diff_summary: &str,
        outcome: &RunOutcome,
    ) -> String {
        let header = if outcome.success { "✅ Symphony — agent run complete" } else { "⚠️ Symphony — agent run failed" };
        let mut body = format!("**{}**\n\n", header);
        body.push_str(&format!("- Agent: `{}`\n", provider));
        body.push_str(&format!("- Diff: {}\n", diff_summary));
        if let Some(s) = &outcome.summary {
            body.push_str(&format!("- Summary: {}\n", truncate(s, 400)));
        }
        if let Some(e) = &outcome.error {
            body.push_str(&format!("- Error: {}\n", truncate(e, 400)));
        }
        body.push_str("\n_Posted automatically by Symphony. Review the workspace and transition this issue manually if it's ready to ship._\n");
        body
    }

    async fn cleanup_running(&self, issue_id: &str) {
        let mut s = self.state.write().await;
        s.running.remove(issue_id);
        s.claimed.remove(issue_id);
    }

    /// §8.5 Part A: scan running entries; abort any whose
    /// `now - last_event_at > stall_timeout_ms`. Aborted runs are queued
    /// for retry with backoff. `stall_timeout_ms <= 0` disables.
    async fn reconcile_stalled(&self) {
        let stall_ms = self.workflow.load().config.agent.stall_timeout_ms;
        if stall_ms == 0 {
            return;
        }
        let now = Instant::now();
        let stall_threshold = Duration::from_millis(stall_ms);

        // Collect stalled entries first (without holding write lock).
        let stalled: Vec<RunningEntry> = {
            let s = self.state.read().await;
            s.running
                .values()
                .filter(|e| now.duration_since(e.last_event_at) > stall_threshold)
                .cloned()
                .collect()
        };

        for entry in stalled {
            tracing::warn!(
                issue = %entry.identifier,
                stalled_for_ms = now.duration_since(entry.last_event_at).as_millis() as u64,
                "stall timeout exceeded; aborting agent"
            );
            if let Some(handle) = &entry.task_handle {
                handle.abort();
            }
            self.audit.record(
                AuditEvent::new(AuditEventKind::Stalled)
                    .with_issue(entry.issue_id.clone(), entry.identifier.clone())
                    .with_provider(entry.agent_id.0.clone())
                    .with_error("stall timeout exceeded".to_string()),
            );
            self.cleanup_running(&entry.issue_id).await;
            // Schedule retry from attempt = previous_attempt + 1; we use 1
            // here because RunningEntry doesn't track attempt currently.
            // Practical effect: stalled runs always restart with backoff
            // tier 1 (10s). Acceptable for MVP.
            self.schedule_retry(
                entry.issue_id,
                entry.identifier,
                1,
                Some("stalled".to_string()),
            )
            .await;
        }
    }

    /// §8.4: any retry whose `due_at <= now` gets re-dispatched in this
    /// tick. Retries that haven't matured stay queued.
    async fn process_retry_queue(self: &Arc<Self>) {
        let now = Instant::now();
        let due: Vec<RetryEntry> = {
            let mut s = self.state.write().await;
            let due_ids: Vec<String> = s
                .retry_queue
                .iter()
                .filter(|(_, r)| r.due_at <= now)
                .map(|(id, _)| id.clone())
                .collect();
            due_ids
                .into_iter()
                .filter_map(|id| s.retry_queue.remove(&id))
                .collect()
        };

        for retry in due {
            // Re-fetch the issue to validate it's still active & eligible.
            let wf = self.workflow.load();
            let active = wf.config.tracker.active_states.clone();
            let candidates = match self.tracker.fetch_candidate_issues(&active).await {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(error = %e, "retry candidate fetch failed; deferring");
                    // Re-enqueue with same due_at + 30s.
                    self.schedule_retry(
                        retry.issue_id,
                        retry.identifier,
                        retry.attempt,
                        retry.error,
                    )
                    .await;
                    continue;
                }
            };
            let label_lc = wf.config.agent.agent_label_required.to_lowercase();
            let issue = candidates.into_iter().find(|i| {
                i.id == retry.issue_id && i.labels.iter().any(|l| l == &label_lc)
            });
            let Some(issue) = issue else {
                tracing::info!(
                    issue = %retry.identifier,
                    "retry target no longer eligible; dropping"
                );
                continue;
            };

            self.audit.record(
                AuditEvent::new(AuditEventKind::RetryDispatched)
                    .with_issue(retry.issue_id.clone(), retry.identifier.clone())
                    .with_message(format!("attempt {}", retry.attempt)),
            );

            if let Err(e) = self.dispatch(issue.clone()).await {
                tracing::warn!(error = %e, "retry dispatch failed");
                // Schedule another retry at the next attempt level.
                self.schedule_retry(
                    issue.id,
                    issue.identifier,
                    retry.attempt + 1,
                    Some(e.to_string()),
                )
                .await;
            }
        }
    }

    /// Insert (or update) a retry entry for `issue_id` with exponential
    /// backoff capped at `max_retry_backoff_ms`. Caps total attempts at
    /// `max_retry_attempts`; emits a `RetryGivenUp` audit event if exceeded.
    async fn schedule_retry(
        &self,
        issue_id: String,
        identifier: String,
        attempt: u32,
        error: Option<String>,
    ) {
        let wf = self.workflow.load();
        let max_attempts = wf.config.agent.max_retry_attempts;
        if attempt > max_attempts {
            tracing::warn!(
                issue = %identifier,
                attempt,
                max_attempts,
                "retry attempts exhausted; giving up"
            );
            self.audit.record(
                AuditEvent::new(AuditEventKind::RetryGivenUp)
                    .with_issue(issue_id, identifier)
                    .with_error(error.unwrap_or_default()),
            );
            return;
        }
        let max_backoff = wf.config.agent.max_retry_backoff_ms;
        let delay_ms = retry_backoff_ms(attempt, max_backoff);
        let due_at = Instant::now() + Duration::from_millis(delay_ms);

        let entry = RetryEntry {
            issue_id: issue_id.clone(),
            identifier: identifier.clone(),
            attempt,
            due_at,
            error: error.clone(),
        };
        {
            let mut s = self.state.write().await;
            s.retry_queue.insert(issue_id.clone(), entry);
        }
        self.audit.record(
            AuditEvent::new(AuditEventKind::RetryScheduled)
                .with_issue(issue_id, identifier)
                .with_message(format!("attempt {} delay {}ms", attempt, delay_ms))
                .with_error(error.unwrap_or_default()),
        );
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        let mut end = n;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        let mut t = s[..end].to_string();
        t.push('…');
        t
    }
}

/// PDX-112 §10.5 — predicate matching env var names that could leak Linear
/// credentials into the agent subprocess. Used by `dispatch` to scrub the
/// task's env map; exposed for direct testing so the env-leak audit test
/// can assert the policy independently of a live agent run.
pub fn is_linear_secret_key(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper == "LINEAR_API_KEY"
        || upper == "LINEAR_API_TOKEN"
        || upper == "LINEAR_TOKEN"
        || upper.starts_with("LINEAR_")
}

/// Compute the retry backoff delay in ms per Symphony §8.4.
///
/// Exposed as a free function for direct unit testing; the orchestrator's
/// `schedule_retry` calls the same formula inline.
pub fn retry_backoff_ms(attempt: u32, max_backoff_ms: u64) -> u64 {
    10_000u64
        .saturating_mul(1u64 << (attempt.saturating_sub(1) as u64).min(20))
        .min(max_backoff_ms)
}

#[cfg(test)]
mod backoff_tests {
    use super::retry_backoff_ms;

    #[test]
    fn first_retry_is_ten_seconds() {
        assert_eq!(retry_backoff_ms(1, 300_000), 10_000);
    }

    #[test]
    fn second_retry_doubles_to_twenty_seconds() {
        assert_eq!(retry_backoff_ms(2, 300_000), 20_000);
    }

    #[test]
    fn fifth_retry_is_clipped_by_default_max() {
        // 10_000 * 2^4 = 160_000, under the 300k cap
        assert_eq!(retry_backoff_ms(5, 300_000), 160_000);
    }

    #[test]
    fn extreme_attempt_clipped_to_cap() {
        assert_eq!(retry_backoff_ms(50, 300_000), 300_000);
    }

    #[test]
    fn lower_cap_clips_earlier() {
        assert_eq!(retry_backoff_ms(3, 30_000), 30_000);
    }

    #[test]
    fn zero_attempt_treated_as_one() {
        // saturating_sub(1) on 0 gives 0; 1 << 0 == 1; result = 10_000
        assert_eq!(retry_backoff_ms(0, 300_000), 10_000);
    }
}

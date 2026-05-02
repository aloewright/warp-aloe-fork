/**
 * Shared payload and result types for Helm Cloudflare Workflows.
 *
 * The shapes here intentionally mirror — but do not depend on — the Rust
 * `cloud_protocol` crate (PDX-17). Workflow payloads cross the JSON-over-WS
 * boundary between Worker code and Symphony's reconciliation tick, so the
 * field names match the protocol's `TaskEvent` / `AgentHealth` discriminators.
 *
 * A future PR (PDX-26 Triggers, PDX-28 Audit Log) will narrow these to a
 * generated TS module produced from the Rust crate. For now they are
 * hand-rolled and documented inline.
 */
export type AgentRole =
  | "coder"
  | "reviewer"
  | "tester"
  | "planner"
  | "researcher"
  | "security"
  | string;

export interface SwarmAgentSpec {
  /** Stable agent id reused across retries; never the WS connection id. */
  id: string;
  role: AgentRole;
  /** Free-form task spec passed to the agent runtime. */
  task: string;
  /** Per-agent retry budget. Independent of the Workflow step retry policy. */
  maxRetries?: number;
}

export interface SwarmWorkflowParams {
  swarmId: string;
  /** Triggering Linear issue / Symphony task id, propagated to TaskEvent.task_id. */
  taskId: string;
  agents: SwarmAgentSpec[];
  /**
   * Optional handoff target — typically a parent swarm or Linear issue.
   * Workflow appends the aggregated result here on success.
   */
  handoff?: {
    kind: "linear" | "swarm" | "webhook";
    target: string;
  };
}

export interface SwarmAgentResult {
  agentId: string;
  status: "succeeded" | "failed";
  output?: string;
  error?: string;
  attempts: number;
}

export interface SwarmWorkflowResult {
  swarmId: string;
  taskId: string;
  succeeded: number;
  failed: number;
  results: SwarmAgentResult[];
  /** ISO-8601 timestamp when the swarm aggregated results. */
  completedAt: string;
}

export interface DeployWorkflowParams {
  deployId: string;
  /** Worker / service / container target. Free-form; resolved by deploy step. */
  target: string;
  /** Build artifact reference — git sha, R2 object, or container digest. */
  artifact: string;
  /**
   * Approver identities allowed to satisfy the gate. The runtime accepts the
   * first valid approval event; additional approvals are ignored.
   */
  approvers: string[];
  /**
   * How long to wait for an approval before the workflow gives up. Maps to
   * `step.waitForEvent` timeout. Default is set by the Workflow class.
   */
  approvalTimeout?: string;
}

export interface DeployWorkflowResult {
  deployId: string;
  target: string;
  artifact: string;
  buildOk: boolean;
  testsOk: boolean;
  approval: {
    approver: string;
    approvedAt: string;
    rationale?: string;
  };
  deployedAt: string;
}

export interface ScheduledTaskWorkflowParams {
  /** Cron-trigger-derived id; stable across retries of the same scheduled run. */
  scheduleId: string;
  taskId: string;
  agentId: string;
  task: string;
  /** ISO-8601 — used so step output is deterministic for a given tick. */
  scheduledFor: string;
}

export interface ScheduledTaskWorkflowResult {
  scheduleId: string;
  taskId: string;
  status: "succeeded" | "failed";
  output?: string;
  error?: string;
  attempts: number;
  completedAt: string;
}

export interface AgentHealthSnapshot {
  agentId: string;
  /** ISO-8601 timestamp of the most recent heartbeat. */
  lastHeartbeat: string;
  status: "healthy" | "degraded" | "stalled" | "unknown";
  inFlightTasks: number;
}

export interface WatchdogWorkflowParams {
  /** Stall threshold in seconds. Agents whose lastHeartbeat is older are flagged. */
  stallSeconds: number;
  /** Optional explicit snapshots; if absent, the Workflow fetches them. */
  snapshots?: AgentHealthSnapshot[];
}

export interface WatchdogWorkflowResult {
  inspectedAt: string;
  inspected: number;
  stalled: AgentHealthSnapshot[];
  degraded: AgentHealthSnapshot[];
  /**
   * Audit log entries emitted by the workflow. PDX-28 will replace this with
   * a real audit log binding; until then they're returned in the result and
   * also written to console.log with a `[WATCHDOG][AUDIT]` marker.
   */
  auditEntries: Array<{
    severity: "info" | "warn" | "error";
    message: string;
    agentId?: string;
    at: string;
  }>;
}

import {
  WorkflowEntrypoint,
  type WorkflowEvent,
  type WorkflowStep,
  type WorkflowStepConfig
} from "cloudflare:workers";
import type {
  AgentHealthSnapshot,
  WatchdogWorkflowParams,
  WatchdogWorkflowResult
} from "./types.js";

export const WATCHDOG_STEP_CONFIG: WorkflowStepConfig = {
  retries: { limit: 3, delay: "10 seconds", backoff: "exponential" },
  timeout: "2 minutes"
};

/**
 * Pure classifier — split out for unit-testing without the Workflow runtime.
 * Snapshots older than `stallSeconds` are flagged as stalled regardless of
 * their reported status; explicit `degraded` status is also surfaced.
 */
export function classifySnapshots(
  snapshots: AgentHealthSnapshot[],
  stallSeconds: number,
  now: Date = new Date()
): {
  stalled: AgentHealthSnapshot[];
  degraded: AgentHealthSnapshot[];
} {
  const stallMs = stallSeconds * 1000;
  const stalled: AgentHealthSnapshot[] = [];
  const degraded: AgentHealthSnapshot[] = [];

  for (const snap of snapshots) {
    const last = Date.parse(snap.lastHeartbeat);
    const ageMs = Number.isFinite(last) ? now.getTime() - last : Infinity;
    if (snap.status === "stalled" || ageMs >= stallMs) {
      stalled.push(snap);
      continue;
    }
    if (snap.status === "degraded") {
      degraded.push(snap);
    }
  }

  return { stalled, degraded };
}

export interface WatchdogWorkflowEnv {
  /** Optional binding to fetch live agent health snapshots. */
  AGENT_RUNTIME?: Fetcher;
}

export class WatchdogWorkflow extends WorkflowEntrypoint<
  WatchdogWorkflowEnv,
  WatchdogWorkflowParams
> {
  async run(
    event: Readonly<WorkflowEvent<WatchdogWorkflowParams>>,
    step: WorkflowStep
  ): Promise<WatchdogWorkflowResult> {
    const params = event.payload;

    const snapshots = await step.do(
      "collect-snapshots",
      WATCHDOG_STEP_CONFIG,
      async () => {
        if (params.snapshots && params.snapshots.length > 0) {
          return params.snapshots;
        }
        // TODO(PDX-21): query agent-runtime for live snapshots once the
        // service binding is wired. Until then we return [] so the watchdog
        // remains a no-op rather than failing loudly.
        return [] as AgentHealthSnapshot[];
      }
    );

    const inspectedAt = await step.do("inspect", async () => {
      const now = new Date();
      const { stalled, degraded } = classifySnapshots(
        snapshots,
        params.stallSeconds,
        now
      );
      return { iso: now.toISOString(), stalled, degraded };
    });

    const auditEntries = await step.do("emit-audit", async () => {
      const entries: WatchdogWorkflowResult["auditEntries"] = [];
      for (const s of inspectedAt.stalled) {
        const entry = {
          severity: "error" as const,
          message: `agent ${s.agentId} stalled (last heartbeat ${s.lastHeartbeat})`,
          agentId: s.agentId,
          at: inspectedAt.iso
        };
        // TODO(PDX-28): replace console.log with audit-log binding write.
        console.log(`[WATCHDOG][AUDIT][error] ${entry.message}`);
        entries.push(entry);
      }
      for (const s of inspectedAt.degraded) {
        const entry = {
          severity: "warn" as const,
          message: `agent ${s.agentId} degraded (in-flight=${s.inFlightTasks})`,
          agentId: s.agentId,
          at: inspectedAt.iso
        };
        // TODO(PDX-28): replace console.log with audit-log binding write.
        console.log(`[WATCHDOG][AUDIT][warn] ${entry.message}`);
        entries.push(entry);
      }
      if (entries.length === 0) {
        const entry = {
          severity: "info" as const,
          message: `watchdog clean: ${snapshots.length} agents inspected`,
          at: inspectedAt.iso
        };
        console.log(`[WATCHDOG][AUDIT][info] ${entry.message}`);
        entries.push(entry);
      }
      return entries;
    });

    return {
      inspectedAt: inspectedAt.iso,
      inspected: snapshots.length,
      stalled: inspectedAt.stalled,
      degraded: inspectedAt.degraded,
      auditEntries
    };
  }
}

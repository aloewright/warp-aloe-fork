import {
  WorkflowEntrypoint,
  type WorkflowEvent,
  type WorkflowStep,
  type WorkflowStepConfig,
  type WorkflowTimeoutDuration
} from "cloudflare:workers";
import type { DeployWorkflowParams, DeployWorkflowResult } from "./types.js";

/**
 * Default approval gate timeout. Workflows deploys hold open for up to a day
 * by default; the operator can override per-deploy via params.approvalTimeout.
 */
export const DEFAULT_APPROVAL_TIMEOUT: WorkflowTimeoutDuration = "24 hours";

export const BUILD_STEP_CONFIG: WorkflowStepConfig = {
  retries: { limit: 2, delay: "30 seconds", backoff: "exponential" },
  timeout: "20 minutes"
};

export const TEST_STEP_CONFIG: WorkflowStepConfig = {
  retries: { limit: 1, delay: "30 seconds", backoff: "constant" },
  timeout: "30 minutes"
};

export const DEPLOY_STEP_CONFIG: WorkflowStepConfig = {
  retries: { limit: 2, delay: "1 minute", backoff: "exponential" },
  timeout: "15 minutes"
};

/**
 * Approval event payload pushed via `Workflow.sendEvent` once an approver
 * clicks the link in the Helm UI. The Workflow runtime correlates the event
 * by name (`approval`) and resumes the suspended run.
 */
export interface DeployApprovalEvent {
  approver: string;
  approvedAt: string;
  rationale?: string;
}

/**
 * Pure validator for an inbound approval event. Tested directly so the
 * approval policy can evolve (e.g. quorum, role-based) without a Worker.
 */
export function validateApproval(
  event: DeployApprovalEvent | undefined,
  approvers: string[]
): { ok: true; event: DeployApprovalEvent } | { ok: false; reason: string } {
  if (!event) return { ok: false, reason: "no approval event received" };
  if (!event.approver) return { ok: false, reason: "approver missing" };
  if (!approvers.includes(event.approver)) {
    return { ok: false, reason: `approver ${event.approver} not authorized` };
  }
  if (!event.approvedAt) return { ok: false, reason: "approvedAt missing" };
  return { ok: true, event };
}

export interface DeployWorkflowEnv {
  /**
   * Optional binding for build/deploy execution. Real wiring lands with the
   * deploy CLI (out of scope here). Tests inject mocks via subclass.
   */
  DEPLOY_RUNNER?: Fetcher;
}

export class DeployWorkflow extends WorkflowEntrypoint<
  DeployWorkflowEnv,
  DeployWorkflowParams
> {
  async run(
    event: Readonly<WorkflowEvent<DeployWorkflowParams>>,
    step: WorkflowStep
  ): Promise<DeployWorkflowResult> {
    const params = event.payload;

    await step.do("validate-deploy", async () => {
      if (!params.deployId || !params.target || !params.artifact) {
        throw new Error("DeployWorkflow: deployId, target, artifact are required");
      }
      if (!Array.isArray(params.approvers) || params.approvers.length === 0) {
        throw new Error("DeployWorkflow: at least one approver is required");
      }
      return { ok: true };
    });

    const buildOk = await step.do("build", BUILD_STEP_CONFIG, async () => {
      // TODO(PDX-26): wire to real build runner via service binding.
      console.log(
        `[DeployWorkflow][BUILD] deploy=${params.deployId} artifact=${params.artifact}`
      );
      return true;
    });

    const testsOk = await step.do("test", TEST_STEP_CONFIG, async () => {
      // TODO(PDX-26): wire to real test runner.
      console.log(
        `[DeployWorkflow][TEST] deploy=${params.deployId} artifact=${params.artifact}`
      );
      return true;
    });

    if (!buildOk || !testsOk) {
      throw new Error(
        `DeployWorkflow: pre-approval gate failed (build=${buildOk}, tests=${testsOk})`
      );
    }

    // Suspend until an approval event arrives. The Workflow runtime persists
    // state so the Worker can be evicted while we wait — that's the whole
    // point of using Workflows for the deploy gate.
    const approvalTimeout: WorkflowTimeoutDuration =
      (params.approvalTimeout as WorkflowTimeoutDuration | undefined) ??
      DEFAULT_APPROVAL_TIMEOUT;

    const approvalEvent = await step.waitForEvent<DeployApprovalEvent>(
      `await-approval:${params.deployId}`,
      { type: "approval", timeout: approvalTimeout }
    );

    const validated = await step.do("validate-approval", async () => {
      const v = validateApproval(approvalEvent.payload, params.approvers);
      if (!v.ok) {
        throw new Error(`DeployWorkflow: approval rejected — ${v.reason}`);
      }
      return v.event;
    });

    const deployedAt = await step.do("deploy", DEPLOY_STEP_CONFIG, async () => {
      // TODO(PDX-26): replace with real deploy invocation.
      console.log(
        `[DeployWorkflow][DEPLOY] deploy=${params.deployId} target=${params.target} approver=${validated.approver}`
      );
      return new Date().toISOString();
    });

    return {
      deployId: params.deployId,
      target: params.target,
      artifact: params.artifact,
      buildOk: true,
      testsOk: true,
      approval: validated,
      deployedAt
    };
  }
}

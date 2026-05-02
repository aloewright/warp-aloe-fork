import {
  WorkflowEntrypoint,
  type WorkflowEvent,
  type WorkflowStep,
  type WorkflowStepConfig
} from "cloudflare:workers";
import type {
  ScheduledTaskWorkflowParams,
  ScheduledTaskWorkflowResult
} from "./types.js";

/**
 * Single-task durable execution. Cron Triggers fire on a schedule; the Worker
 * scheduled handler creates a ScheduledTaskWorkflow instance with the cron
 * tick id as scheduleId. The Workflow then carries the task across Worker
 * restarts, retries, and eviction — which is what the bare scheduled handler
 * cannot do on its own.
 */
export const SCHEDULED_TASK_STEP_CONFIG: WorkflowStepConfig = {
  retries: { limit: 5, delay: "30 seconds", backoff: "exponential" },
  timeout: "15 minutes"
};

export interface ScheduledTaskWorkflowEnv {
  AGENT_RUNTIME?: Fetcher;
}

export class ScheduledTaskWorkflow extends WorkflowEntrypoint<
  ScheduledTaskWorkflowEnv,
  ScheduledTaskWorkflowParams
> {
  async run(
    event: Readonly<WorkflowEvent<ScheduledTaskWorkflowParams>>,
    step: WorkflowStep
  ): Promise<ScheduledTaskWorkflowResult> {
    const params = event.payload;

    await step.do("validate-scheduled-task", async () => {
      if (!params.scheduleId || !params.taskId || !params.agentId) {
        throw new Error(
          "ScheduledTaskWorkflow: scheduleId, taskId, agentId are required"
        );
      }
      return { ok: true };
    });

    const outcome = await step
      .do(
        `run-task:${params.taskId}`,
        SCHEDULED_TASK_STEP_CONFIG,
        async (ctx) => {
          // TODO(PDX-21): replace with service binding call to agent-runtime.
          console.log(
            `[ScheduledTaskWorkflow][DISPATCH] schedule=${params.scheduleId} task=${params.taskId} attempt=${ctx.attempt}`
          );
          return {
            status: "succeeded" as const,
            output: `task ${params.taskId} executed at ${params.scheduledFor}`,
            attempts: ctx.attempt
          };
        }
      )
      .catch(async (err: unknown) =>
        step.do("record-failure", async () => ({
          status: "failed" as const,
          error: err instanceof Error ? err.message : String(err),
          attempts: SCHEDULED_TASK_STEP_CONFIG.retries?.limit ?? 1
        }))
      );

    const completedAt = await step.do("finalize", async () =>
      new Date().toISOString()
    );

    return {
      scheduleId: params.scheduleId,
      taskId: params.taskId,
      status: outcome.status,
      output: "output" in outcome ? outcome.output : undefined,
      error: "error" in outcome ? outcome.error : undefined,
      attempts: outcome.attempts,
      completedAt
    };
  }
}

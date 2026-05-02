import {
  WorkflowEntrypoint,
  type WorkflowEvent,
  type WorkflowStep,
  type WorkflowStepConfig
} from "cloudflare:workers";
import type {
  SwarmAgentResult,
  SwarmAgentSpec,
  SwarmWorkflowParams,
  SwarmWorkflowResult
} from "./types.js";

/**
 * Per-agent step retry config. The Workflow runtime retries the step body on
 * thrown errors; SwarmWorkflow trusts that mechanism rather than implementing
 * its own loop. `exponential` backoff matches the orchestrator's default
 * dispatch policy (see PDX-42 dispatcher).
 */
export const AGENT_STEP_CONFIG: WorkflowStepConfig = {
  retries: { limit: 3, delay: "5 seconds", backoff: "exponential" },
  timeout: "10 minutes"
};

/**
 * Aggregate per-agent results into a SwarmWorkflowResult. Pure function so the
 * orchestration logic is unit-testable without the Workflow runtime.
 */
export function aggregateSwarmResults(
  params: SwarmWorkflowParams,
  results: SwarmAgentResult[],
  completedAt: string = new Date().toISOString()
): SwarmWorkflowResult {
  const succeeded = results.filter((r) => r.status === "succeeded").length;
  return {
    swarmId: params.swarmId,
    taskId: params.taskId,
    succeeded,
    failed: results.length - succeeded,
    results,
    completedAt
  };
}

/**
 * Dispatch a single agent. Exposed for test injection. Real implementation
 * should call into the agent-runtime Worker via service binding once PDX-21
 * lands; until then it returns a placeholder string and is mocked in tests.
 */
export type AgentDispatcher = (
  agent: SwarmAgentSpec,
  attempt: number
) => Promise<string>;

export const defaultAgentDispatcher: AgentDispatcher = async (agent) => {
  // TODO(PDX-21): replace with service-binding call to agent-runtime.
  // The dispatch payload mirrors cloud_protocol::TaskEvent::Dispatched.
  return `dispatched:${agent.id}`;
};

export interface SwarmWorkflowEnv {
  /**
   * Optional service binding to the agent-runtime Worker. Wired through
   * wrangler.workflows.toml. Tests pass undefined and inject a dispatcher.
   */
  AGENT_RUNTIME?: Fetcher;
}

export class SwarmWorkflow extends WorkflowEntrypoint<
  SwarmWorkflowEnv,
  SwarmWorkflowParams
> {
  async run(
    event: Readonly<WorkflowEvent<SwarmWorkflowParams>>,
    step: WorkflowStep
  ): Promise<SwarmWorkflowResult> {
    const params = event.payload;

    // Validate input as its own step so a malformed payload is recorded as a
    // distinct workflow error rather than crashing inside an agent step.
    await step.do("validate-swarm", async () => {
      if (!params.swarmId || !params.taskId) {
        throw new Error("SwarmWorkflow: swarmId and taskId are required");
      }
      if (!Array.isArray(params.agents) || params.agents.length === 0) {
        throw new Error("SwarmWorkflow: at least one agent is required");
      }
      return { ok: true };
    });

    // Dispatch + collect each agent in its own durable step so individual
    // agent failures retry without re-running the whole swarm.
    const results: SwarmAgentResult[] = [];
    for (const agent of params.agents) {
      const result = await step.do(
        `agent:${agent.id}`,
        AGENT_STEP_CONFIG,
        async (ctx) => {
          try {
            const output = await defaultAgentDispatcher(agent, ctx.attempt);
            const r: SwarmAgentResult = {
              agentId: agent.id,
              status: "succeeded",
              output,
              attempts: ctx.attempt
            };
            return r;
          } catch (err) {
            // Re-throw so the Workflow runtime applies the retry policy. Once
            // retries are exhausted we transform the error into a `failed`
            // result in the surrounding catch.
            throw err;
          }
        }
      ).catch(async (err: unknown): Promise<SwarmAgentResult> => {
        // After step.do exhausts retries, capture the failure as a result so
        // the swarm can still aggregate and hand off partial completion.
        return step.do(`agent:${agent.id}:fallback`, async () => ({
          agentId: agent.id,
          status: "failed",
          error: err instanceof Error ? err.message : String(err),
          attempts: AGENT_STEP_CONFIG.retries?.limit ?? 1
        }));
      });
      results.push(result);
    }

    const aggregated = await step.do("aggregate-results", async () =>
      aggregateSwarmResults(params, results)
    );

    if (params.handoff) {
      await step.do("handoff", async () => {
        // TODO(PDX-26): replace with real handoff trigger emit.
        console.log(
          `[SwarmWorkflow][HANDOFF] kind=${params.handoff!.kind} target=${params.handoff!.target} swarm=${params.swarmId}`
        );
        return { handed_off: true };
      });
    }

    return aggregated;
  }
}

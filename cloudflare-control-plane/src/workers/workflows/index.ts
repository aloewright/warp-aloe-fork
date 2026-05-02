/**
 * Helm Workflows Worker — hosts the four PDX-25 durability workflows:
 *   - SwarmWorkflow
 *   - DeployWorkflow (with approval gate)
 *   - ScheduledTaskWorkflow (driven by Cron Triggers)
 *   - WatchdogWorkflow
 *
 * The Worker exposes a thin admin HTTP surface so the Helm CLI / control
 * plane can create instances and (for DeployWorkflow) send approval events.
 * It also wires the Cron Trigger handler that fans out into
 * ScheduledTaskWorkflow + WatchdogWorkflow on schedule.
 *
 * Real auth and audit-log integration arrive with PDX-26 (Triggers) and
 * PDX-28 (Audit Log). For now the Worker re-uses the `requireAccess` helper
 * from shared/http.ts.
 */
import { health, json, methodNotAllowed, notFound, requireAccess } from "../../shared/http.js";
import { manifestForRuntime, type HelmEnvironment } from "../../shared/manifest.js";
import { SwarmWorkflow } from "./swarm-workflow.js";
import { DeployWorkflow, type DeployApprovalEvent } from "./deploy-workflow.js";
import { ScheduledTaskWorkflow } from "./scheduled-task-workflow.js";
import { WatchdogWorkflow } from "./watchdog-workflow.js";
import type {
  DeployWorkflowParams,
  ScheduledTaskWorkflowParams,
  SwarmWorkflowParams,
  WatchdogWorkflowParams
} from "./types.js";

export { SwarmWorkflow, DeployWorkflow, ScheduledTaskWorkflow, WatchdogWorkflow };
export * from "./types.js";

/**
 * Minimal subset of the Workflow binding API we use here. Typed structurally
 * so tests can pass a fake without depending on `cloudflare:workers` runtime
 * types in the test environment.
 */
export interface WorkflowBinding {
  create(options: { id?: string; params: unknown }): Promise<{ id: string }>;
  get(id: string): Promise<{
    sendEvent(event: { type: string; payload: unknown }): Promise<void>;
    status(): Promise<{ status: string }>;
  }>;
}

export interface WorkflowsEnv {
  HELM_ENVIRONMENT: HelmEnvironment;
  HELM_VERSION: string;
  HELM_BUILD_ID: string;
  HELM_MANIFEST_JSON: string;
  SWARM_WORKFLOW: WorkflowBinding;
  DEPLOY_WORKFLOW: WorkflowBinding;
  SCHEDULED_TASK_WORKFLOW: WorkflowBinding;
  WATCHDOG_WORKFLOW: WorkflowBinding;
}

interface CreateRequestBody {
  id?: string;
  params: unknown;
}

interface ApprovalRequestBody {
  approval: DeployApprovalEvent;
}

async function readJson<T>(request: Request): Promise<T> {
  return (await request.json()) as T;
}

/**
 * Resolve the appropriate Workflow binding for a workflow slug. Centralized so
 * the routing tests don't have to duplicate string→binding logic.
 */
export function resolveWorkflowBinding(
  env: WorkflowsEnv,
  slug: string
): WorkflowBinding | undefined {
  switch (slug) {
    case "swarm":
      return env.SWARM_WORKFLOW;
    case "deploy":
      return env.DEPLOY_WORKFLOW;
    case "scheduled-task":
      return env.SCHEDULED_TASK_WORKFLOW;
    case "watchdog":
      return env.WATCHDOG_WORKFLOW;
    default:
      return undefined;
  }
}

/**
 * Cron-driven scheduled handler. On each tick we fan out a watchdog run and
 * (when the cron payload provides a task) a ScheduledTaskWorkflow. PDX-26 will
 * replace this with a richer trigger router; today the cron config in
 * wrangler.workflows.toml fires this handler directly.
 */
async function onScheduled(
  controller: ScheduledController,
  env: WorkflowsEnv
): Promise<void> {
  const scheduleId = `${controller.cron}-${controller.scheduledTime}`;

  // Always run the watchdog on every tick.
  const watchdogParams: WatchdogWorkflowParams = {
    stallSeconds: 300
  };
  await env.WATCHDOG_WORKFLOW.create({
    id: `watchdog-${scheduleId}`,
    params: watchdogParams
  }).catch((err) => {
    console.log(`[Workflows][cron] watchdog enqueue failed: ${err}`);
  });

  // TODO(PDX-26): pull scheduled task definitions from a manifest binding.
  // For now we emit a no-op ScheduledTaskWorkflow so the durable path is
  // exercised end-to-end on every cron tick.
  const taskParams: ScheduledTaskWorkflowParams = {
    scheduleId,
    taskId: `cron-${controller.scheduledTime}`,
    agentId: "scheduler",
    task: "noop",
    scheduledFor: new Date(controller.scheduledTime).toISOString()
  };
  await env.SCHEDULED_TASK_WORKFLOW.create({
    id: `scheduled-${scheduleId}`,
    params: taskParams
  }).catch((err) => {
    console.log(`[Workflows][cron] scheduled-task enqueue failed: ${err}`);
  });
}

export default {
  async fetch(request: Request, env: WorkflowsEnv): Promise<Response> {
    const url = new URL(request.url);

    if (url.pathname === "/api/health") {
      return health({
        service: "helm-workflows",
        environment: env.HELM_ENVIRONMENT,
        version: env.HELM_VERSION,
        buildId: env.HELM_BUILD_ID
      });
    }

    const manifest = manifestForRuntime(env);
    const accessFailure = await requireAccess(
      request,
      manifest.access,
      env.HELM_ENVIRONMENT
    );
    if (accessFailure) return accessFailure;

    // POST /api/workflows/:slug/instances — create a new workflow run.
    const createMatch = url.pathname.match(
      /^\/api\/workflows\/([^/]+)\/instances$/
    );
    if (createMatch) {
      if (request.method !== "POST") return methodNotAllowed();
      const slug = createMatch[1];
      const binding = resolveWorkflowBinding(env, slug ?? "");
      if (!binding) return notFound();
      const body = await readJson<CreateRequestBody>(request).catch(
        () => ({ params: undefined } as CreateRequestBody)
      );
      // Light per-slug validation. Heavy validation lives inside each
      // Workflow's first step so a corrupt enqueue still produces a record.
      if (slug === "swarm") {
        const p = body.params as Partial<SwarmWorkflowParams> | undefined;
        if (!p?.swarmId || !p?.taskId || !Array.isArray(p?.agents)) {
          return json({ error: "invalid swarm params" }, { status: 400 });
        }
      } else if (slug === "deploy") {
        const p = body.params as Partial<DeployWorkflowParams> | undefined;
        if (!p?.deployId || !p?.target || !p?.artifact || !Array.isArray(p?.approvers)) {
          return json({ error: "invalid deploy params" }, { status: 400 });
        }
      }
      const instance = await binding.create({
        id: body.id,
        params: body.params
      });
      return json({ id: instance.id, slug }, { status: 201 });
    }

    // POST /api/workflows/deploy/instances/:id/approve — sends approval event.
    const approveMatch = url.pathname.match(
      /^\/api\/workflows\/deploy\/instances\/([^/]+)\/approve$/
    );
    if (approveMatch) {
      if (request.method !== "POST") return methodNotAllowed();
      const id = approveMatch[1] ?? "";
      const body = await readJson<ApprovalRequestBody>(request).catch(
        () => ({ approval: undefined } as unknown as ApprovalRequestBody)
      );
      if (!body.approval?.approver || !body.approval?.approvedAt) {
        return json({ error: "invalid approval payload" }, { status: 400 });
      }
      const instance = await env.DEPLOY_WORKFLOW.get(id);
      await instance.sendEvent({ type: "approval", payload: body.approval });
      return json({ id, accepted: true });
    }

    // GET /api/workflows/:slug/instances/:id — status.
    const statusMatch = url.pathname.match(
      /^\/api\/workflows\/([^/]+)\/instances\/([^/]+)$/
    );
    if (statusMatch) {
      if (request.method !== "GET") return methodNotAllowed();
      const slug = statusMatch[1];
      const id = statusMatch[2] ?? "";
      const binding = resolveWorkflowBinding(env, slug ?? "");
      if (!binding) return notFound();
      const instance = await binding.get(id);
      const status = await instance.status();
      return json({ id, slug, status: status.status });
    }

    return notFound();
  },

  async scheduled(
    controller: ScheduledController,
    env: WorkflowsEnv
  ): Promise<void> {
    await onScheduled(controller, env);
  }
};

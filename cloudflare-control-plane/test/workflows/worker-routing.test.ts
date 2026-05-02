import { describe, expect, it, vi } from "vitest";
import workflowsWorker, {
  resolveWorkflowBinding,
  type WorkflowBinding,
  type WorkflowsEnv
} from "../../src/workers/workflows/index.js";

function fakeBinding(): WorkflowBinding & {
  create: ReturnType<typeof vi.fn>;
  get: ReturnType<typeof vi.fn>;
  sendEvent: ReturnType<typeof vi.fn>;
} {
  const sendEvent = vi.fn().mockResolvedValue(undefined);
  const get = vi.fn().mockResolvedValue({
    sendEvent,
    status: vi.fn().mockResolvedValue({ status: "running" })
  });
  const create = vi.fn().mockImplementation(async (opts: { id?: string }) => ({
    id: opts.id ?? "generated-id"
  }));
  return { create, get, sendEvent } as unknown as WorkflowBinding & {
    create: ReturnType<typeof vi.fn>;
    get: ReturnType<typeof vi.fn>;
    sendEvent: ReturnType<typeof vi.fn>;
  };
}

const baseManifest = JSON.stringify({
  accountId: "acct",
  zone: { id: "zone", domain: "example.com" },
  environments: ["dev", "staging", "production"],
  workers: {
    "helm-control-plane": { routes: {}, metadata: {} },
    "helm-agent-runtime": { routes: {}, metadata: {} },
    "helm-cloudflare-mcp": { routes: {}, metadata: {} }
  },
  resources: { d1: {}, r2: {}, durableObjects: {}, kv: {}, aiGateways: {} },
  containers: {
    dev: { enabled: false, instanceClass: "dev" },
    staging: { enabled: false, instanceClass: "standard" },
    production: { enabled: false, instanceClass: "standard" }
  },
  // access.required = false so tests can issue unauthenticated requests.
  access: { required: false, teamDomain: "team.cloudflareaccess.com", audiences: {} },
  protected: []
});

function makeEnv(overrides: Partial<WorkflowsEnv> = {}): WorkflowsEnv {
  return {
    HELM_ENVIRONMENT: "dev",
    HELM_VERSION: "0.1.0",
    HELM_BUILD_ID: "test",
    HELM_MANIFEST_JSON: baseManifest,
    SWARM_WORKFLOW: fakeBinding(),
    DEPLOY_WORKFLOW: fakeBinding(),
    SCHEDULED_TASK_WORKFLOW: fakeBinding(),
    WATCHDOG_WORKFLOW: fakeBinding(),
    ...overrides
  };
}

describe("resolveWorkflowBinding", () => {
  it("maps each slug to its binding and returns undefined for unknown", () => {
    const env = makeEnv();
    expect(resolveWorkflowBinding(env, "swarm")).toBe(env.SWARM_WORKFLOW);
    expect(resolveWorkflowBinding(env, "deploy")).toBe(env.DEPLOY_WORKFLOW);
    expect(resolveWorkflowBinding(env, "scheduled-task")).toBe(
      env.SCHEDULED_TASK_WORKFLOW
    );
    expect(resolveWorkflowBinding(env, "watchdog")).toBe(env.WATCHDOG_WORKFLOW);
    expect(resolveWorkflowBinding(env, "bogus")).toBeUndefined();
  });
});

describe("Workflows worker fetch", () => {
  it("answers GET /api/health without bindings", async () => {
    const env = makeEnv();
    const res = await workflowsWorker.fetch(
      new Request("https://example.test/api/health"),
      env
    );
    expect(res.status).toBe(200);
    const body = (await res.json()) as { service: string; environment: string };
    expect(body.service).toBe("helm-workflows");
    expect(body.environment).toBe("dev");
  });

  it("creates a swarm instance via POST", async () => {
    const env = makeEnv();
    const res = await workflowsWorker.fetch(
      new Request("https://example.test/api/workflows/swarm/instances", {
        method: "POST",
        body: JSON.stringify({
          id: "swarm-abc",
          params: {
            swarmId: "swarm-abc",
            taskId: "task-1",
            agents: [{ id: "a", role: "coder", task: "x" }]
          }
        }),
        headers: { "content-type": "application/json" }
      }),
      env
    );
    expect(res.status).toBe(201);
    const body = (await res.json()) as { id: string; slug: string };
    expect(body.id).toBe("swarm-abc");
    expect(body.slug).toBe("swarm");
    expect(env.SWARM_WORKFLOW.create).toHaveBeenCalledOnce();
  });

  it("rejects malformed swarm params with 400", async () => {
    const env = makeEnv();
    const res = await workflowsWorker.fetch(
      new Request("https://example.test/api/workflows/swarm/instances", {
        method: "POST",
        body: JSON.stringify({ params: { swarmId: "x" } }),
        headers: { "content-type": "application/json" }
      }),
      env
    );
    expect(res.status).toBe(400);
    expect(env.SWARM_WORKFLOW.create).not.toHaveBeenCalled();
  });

  it("creates a deploy instance with an approver list", async () => {
    const env = makeEnv();
    const res = await workflowsWorker.fetch(
      new Request("https://example.test/api/workflows/deploy/instances", {
        method: "POST",
        body: JSON.stringify({
          id: "deploy-1",
          params: {
            deployId: "deploy-1",
            target: "helm-control-plane-dev",
            artifact: "abc123",
            approvers: ["alice@example.com"]
          }
        }),
        headers: { "content-type": "application/json" }
      }),
      env
    );
    expect(res.status).toBe(201);
    expect(env.DEPLOY_WORKFLOW.create).toHaveBeenCalledOnce();
  });

  it("forwards a deploy approval as a sendEvent on the workflow instance", async () => {
    const env = makeEnv();
    const res = await workflowsWorker.fetch(
      new Request(
        "https://example.test/api/workflows/deploy/instances/deploy-1/approve",
        {
          method: "POST",
          body: JSON.stringify({
            approval: {
              approver: "alice@example.com",
              approvedAt: "2026-05-02T00:00:00Z",
              rationale: "lgtm"
            }
          }),
          headers: { "content-type": "application/json" }
        }
      ),
      env
    );
    expect(res.status).toBe(200);
    expect(env.DEPLOY_WORKFLOW.get).toHaveBeenCalledWith("deploy-1");
    const deploy = env.DEPLOY_WORKFLOW as unknown as {
      sendEvent: ReturnType<typeof vi.fn>;
    };
    expect(deploy.sendEvent).toHaveBeenCalledWith({
      type: "approval",
      payload: {
        approver: "alice@example.com",
        approvedAt: "2026-05-02T00:00:00Z",
        rationale: "lgtm"
      }
    });
  });

  it("rejects an approval missing required fields", async () => {
    const env = makeEnv();
    const res = await workflowsWorker.fetch(
      new Request(
        "https://example.test/api/workflows/deploy/instances/deploy-1/approve",
        {
          method: "POST",
          body: JSON.stringify({ approval: { approver: "alice@example.com" } }),
          headers: { "content-type": "application/json" }
        }
      ),
      env
    );
    expect(res.status).toBe(400);
    expect(env.DEPLOY_WORKFLOW.get).not.toHaveBeenCalled();
  });

  it("returns instance status via GET", async () => {
    const env = makeEnv();
    const res = await workflowsWorker.fetch(
      new Request(
        "https://example.test/api/workflows/watchdog/instances/wd-1",
        { method: "GET" }
      ),
      env
    );
    expect(res.status).toBe(200);
    const body = (await res.json()) as { id: string; slug: string; status: string };
    expect(body).toMatchObject({ id: "wd-1", slug: "watchdog", status: "running" });
  });

  it("returns 404 for unknown workflow slugs", async () => {
    const env = makeEnv();
    const res = await workflowsWorker.fetch(
      new Request("https://example.test/api/workflows/bogus/instances", {
        method: "POST",
        body: "{}",
        headers: { "content-type": "application/json" }
      }),
      env
    );
    expect(res.status).toBe(404);
  });
});

describe("Workflows worker scheduled handler", () => {
  it("fans out into watchdog and scheduled-task on each cron tick", async () => {
    const env = makeEnv();
    const controller = {
      cron: "*/5 * * * *",
      scheduledTime: 1735689600000,
      noRetry: () => undefined
    } as unknown as ScheduledController;

    await workflowsWorker.scheduled(controller, env);

    expect(env.WATCHDOG_WORKFLOW.create).toHaveBeenCalledOnce();
    expect(env.SCHEDULED_TASK_WORKFLOW.create).toHaveBeenCalledOnce();

    const watchdogCall = (env.WATCHDOG_WORKFLOW.create as ReturnType<typeof vi.fn>).mock
      .calls[0]?.[0];
    expect(watchdogCall.id).toMatch(/^watchdog-/);
    expect(watchdogCall.params).toMatchObject({ stallSeconds: 300 });
  });
});

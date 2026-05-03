import { describe, expect, it } from "vitest";
import {
  aggregateSwarmResults,
  AGENT_STEP_CONFIG
} from "../../src/workers/workflows/swarm-workflow.js";
import {
  validateApproval,
  DEFAULT_APPROVAL_TIMEOUT,
  buildCommandForKind,
  testCommandForKind,
  deployCommandFor,
  invokeRunner,
  type DeployApprovalEvent,
  type DeployRunnerRequest
} from "../../src/workers/workflows/deploy-workflow.js";
import { classifySnapshots } from "../../src/workers/workflows/watchdog-workflow.js";
import type {
  AgentHealthSnapshot,
  SwarmAgentResult,
  SwarmWorkflowParams
} from "../../src/workers/workflows/types.js";

describe("SwarmWorkflow.aggregateSwarmResults", () => {
  const params: SwarmWorkflowParams = {
    swarmId: "swarm-1",
    taskId: "task-42",
    agents: [
      { id: "a", role: "coder", task: "x" },
      { id: "b", role: "tester", task: "y" }
    ]
  };

  it("counts succeeded vs failed and surfaces the timestamp", () => {
    const results: SwarmAgentResult[] = [
      { agentId: "a", status: "succeeded", output: "ok", attempts: 1 },
      { agentId: "b", status: "failed", error: "timeout", attempts: 3 }
    ];
    const agg = aggregateSwarmResults(params, results, "2026-05-02T00:00:00Z");
    expect(agg.swarmId).toBe("swarm-1");
    expect(agg.taskId).toBe("task-42");
    expect(agg.succeeded).toBe(1);
    expect(agg.failed).toBe(1);
    expect(agg.results).toHaveLength(2);
    expect(agg.completedAt).toBe("2026-05-02T00:00:00Z");
  });

  it("handles an empty result set", () => {
    const agg = aggregateSwarmResults(params, [], "2026-05-02T00:00:00Z");
    expect(agg.succeeded).toBe(0);
    expect(agg.failed).toBe(0);
  });

  it("uses an exponential retry policy with a non-zero limit", () => {
    expect(AGENT_STEP_CONFIG.retries?.limit ?? 0).toBeGreaterThan(0);
    expect(AGENT_STEP_CONFIG.retries?.backoff).toBe("exponential");
  });
});

describe("DeployWorkflow.validateApproval", () => {
  const approvers = ["alice@example.com", "bob@example.com"];

  it("accepts a well-formed approval from an authorized approver", () => {
    const event: DeployApprovalEvent = {
      approver: "alice@example.com",
      approvedAt: "2026-05-02T00:00:00Z",
      rationale: "lgtm"
    };
    const result = validateApproval(event, approvers);
    expect(result.ok).toBe(true);
    if (result.ok) expect(result.event.approver).toBe("alice@example.com");
  });

  it("rejects an approver not on the allowlist", () => {
    const event: DeployApprovalEvent = {
      approver: "mallory@example.com",
      approvedAt: "2026-05-02T00:00:00Z"
    };
    const result = validateApproval(event, approvers);
    expect(result.ok).toBe(false);
    if (!result.ok) expect(result.reason).toMatch(/not authorized/);
  });

  it("rejects a missing event entirely", () => {
    const result = validateApproval(undefined, approvers);
    expect(result.ok).toBe(false);
  });

  it("rejects an event missing approvedAt", () => {
    const result = validateApproval(
      { approver: "alice@example.com", approvedAt: "" },
      approvers
    );
    expect(result.ok).toBe(false);
  });

  it("exposes a non-zero default approval timeout", () => {
    expect(DEFAULT_APPROVAL_TIMEOUT).toBeTruthy();
  });
});

describe("DeployWorkflow PDX-114 step body helpers", () => {
  it("renders kind-aware build commands", () => {
    expect(buildCommandForKind("cloudflare_worker")).toBe("npm run build");
    expect(buildCommandForKind("npm_publish")).toBe("npm run build");
    expect(buildCommandForKind("cargo_publish")).toBe("cargo build --release");
    expect(buildCommandForKind("gh_release")).toBe("cargo build --release");
    // Default — preserves PDX-25 console-log path.
    expect(buildCommandForKind(undefined)).toBe("npm run build");
  });

  it("renders kind-aware test commands", () => {
    expect(testCommandForKind("cargo_publish")).toBe("cargo test --workspace");
    expect(testCommandForKind("gh_release")).toBe("cargo test --workspace");
    expect(testCommandForKind("cloudflare_worker")).toBe("npm test");
    expect(testCommandForKind(undefined)).toBe("npm test");
  });

  it("renders kind-aware deploy commands with env injected for cloudflare_worker", () => {
    expect(deployCommandFor("cloudflare_worker", "production", "x")).toBe(
      "wrangler deploy --env production"
    );
    expect(deployCommandFor("cloudflare_worker", undefined, "x")).toBe(
      "wrangler deploy"
    );
    expect(deployCommandFor("npm_publish", "production", "x")).toBe(
      "npm publish --access public"
    );
    expect(deployCommandFor("cargo_publish", "production", "x")).toBe(
      "cargo publish"
    );
    expect(deployCommandFor("gh_release", undefined, "v1.2.3")).toBe(
      "gh release create v1.2.3 --generate-notes"
    );
  });

  it("invokeRunner returns ok when no runner is bound (PDX-25 fallback)", async () => {
    const result = await invokeRunner(undefined, {
      step: "build",
      command: "npm run build",
      secrets: [],
      deployId: "deploy-1",
      artifact: "abc"
    });
    expect(result.ok).toBe(true);
  });

  it("invokeRunner POSTs to the runner binding when bound and surfaces failure", async () => {
    let captured: DeployRunnerRequest | null = null;
    const runner: Fetcher = {
      // Minimal Fetcher mock — Vitest runs in node so we don't need a
      // full Workers runtime.
      fetch: async (_input: RequestInfo, init?: RequestInit) => {
        captured = JSON.parse(init?.body as string) as DeployRunnerRequest;
        return new Response(JSON.stringify({ ok: false, error: "boom", exit_code: 7 }), {
          status: 500,
          headers: { "content-type": "application/json" }
        });
      }
    } as unknown as Fetcher;
    const result = await invokeRunner(runner, {
      step: "deploy",
      command: "wrangler deploy --env production",
      secrets: ["CLOUDFLARE_API_TOKEN"],
      deployId: "deploy-1",
      artifact: "abc",
      kind: "cloudflare_worker",
      envName: "production"
    });
    expect(result.ok).toBe(false);
    expect(result.error).toMatch(/boom|500/);
    expect(captured).not.toBeNull();
    expect(captured!.command).toBe("wrangler deploy --env production");
    expect(captured!.secrets).toEqual(["CLOUDFLARE_API_TOKEN"]);
  });

  it("invokeRunner parses an ok response from the runner", async () => {
    const runner: Fetcher = {
      fetch: async () =>
        new Response(JSON.stringify({ ok: true }), {
          status: 200,
          headers: { "content-type": "application/json" }
        })
    } as unknown as Fetcher;
    const result = await invokeRunner(runner, {
      step: "test",
      command: "npm test",
      secrets: [],
      deployId: "deploy-1",
      artifact: "abc"
    });
    expect(result.ok).toBe(true);
  });
});

describe("WatchdogWorkflow.classifySnapshots", () => {
  const now = new Date("2026-05-02T00:00:00Z");

  const snap = (
    overrides: Partial<AgentHealthSnapshot> = {}
  ): AgentHealthSnapshot => ({
    agentId: "agent-1",
    lastHeartbeat: now.toISOString(),
    status: "healthy",
    inFlightTasks: 0,
    ...overrides
  });

  it("flags snapshots older than the stall threshold as stalled", () => {
    const stale = snap({
      agentId: "stale",
      lastHeartbeat: new Date(now.getTime() - 10 * 60_000).toISOString()
    });
    const fresh = snap({ agentId: "fresh" });
    const { stalled, degraded } = classifySnapshots([stale, fresh], 60, now);
    expect(stalled.map((s) => s.agentId)).toEqual(["stale"]);
    expect(degraded).toEqual([]);
  });

  it("flags explicit stalled status even when heartbeat is fresh", () => {
    const explicit = snap({ agentId: "x", status: "stalled" });
    const { stalled } = classifySnapshots([explicit], 60, now);
    expect(stalled).toHaveLength(1);
  });

  it("surfaces degraded snapshots without flagging them as stalled", () => {
    const deg = snap({ agentId: "deg", status: "degraded", inFlightTasks: 7 });
    const { stalled, degraded } = classifySnapshots([deg], 60, now);
    expect(stalled).toEqual([]);
    expect(degraded.map((s) => s.agentId)).toEqual(["deg"]);
  });

  it("treats an unparseable heartbeat as stalled (defensive)", () => {
    const broken = snap({ agentId: "broken", lastHeartbeat: "not-a-date" });
    const { stalled } = classifySnapshots([broken], 60, now);
    expect(stalled.map((s) => s.agentId)).toEqual(["broken"]);
  });
});

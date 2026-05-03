import {
  WorkflowEntrypoint,
  type WorkflowEvent,
  type WorkflowStep,
  type WorkflowStepConfig,
  type WorkflowTimeoutDuration
} from "cloudflare:workers";
import type { DeployWorkflowParams, DeployWorkflowResult } from "./types.js";

/**
 * Extended deploy params we accept from PDX-114's daemon-mediated
 * `deploy` tool. The base `DeployWorkflowParams` shape is stable across
 * PDX-25 / PDX-19 callers; PDX-114 layers on the deploy `kind`,
 * `env_name`, and the Doppler-fronted `secrets` env var allowlist that
 * the deploy step uses to invoke the underlying CLI.
 *
 * The fields are optional so older callers still work — PDX-25's
 * console-log path is preserved when they aren't supplied.
 */
export interface ExtendedDeployParams extends DeployWorkflowParams {
  /** Deploy kind (`cloudflare_worker`, `npm_publish`, `cargo_publish`, `gh_release`). */
  kind?: "cloudflare_worker" | "npm_publish" | "cargo_publish" | "gh_release";
  /** Env name passed to the deploy CLI (e.g. `--env production`). */
  env_name?: string;
  /** Doppler-fronted secret env var names to inject into the deploy step. */
  secrets?: string[];
}

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
   *
   * PDX-114 [E2]: when bound, build/test/deploy steps fan out to
   * `DEPLOY_RUNNER.fetch("/run", { method: "POST", body: { command,
   * env } })`. The runner Worker (or the agent-runtime container, via
   * a tunneled binding) executes the command in a sandboxed
   * environment with the configured secrets injected. When unbound
   * the workflow falls back to PDX-25's console-log path so existing
   * tests keep passing.
   */
  DEPLOY_RUNNER?: Fetcher;
}

/**
 * PDX-114 — payload sent to the optional `DEPLOY_RUNNER` binding for
 * one build/test/deploy invocation. Matches the agent-runtime
 * container surface in PDX-21.
 */
export interface DeployRunnerRequest {
  /** Step name — `"build"` / `"test"` / `"deploy"`. */
  step: "build" | "test" | "deploy";
  /** Literal command line to execute (already shell-quoted). */
  command: string;
  /** Doppler-fronted secret env var names to inject. */
  secrets: string[];
  /** Workflow correlation id. */
  deployId: string;
  /** Build artifact reference (git sha, R2 object, container digest). */
  artifact: string;
  /** Deploy kind for downstream observability. */
  kind?: string;
  /** Environment name (production / staging / preview). */
  envName?: string;
}

/**
 * PDX-114 — runner response shape. `ok = false` aborts the workflow at
 * the call site with a descriptive error; the runner is expected to
 * stream logs to the agent-runtime audit log out-of-band.
 */
export interface DeployRunnerResponse {
  ok: boolean;
  /** When `ok = false`, a human-readable failure reason. */
  error?: string;
  /** Optional structured exit info — useful for the deploy comment back to Linear. */
  exit_code?: number;
}

/**
 * Render the build command for a given deploy kind. Pure so step
 * bodies remain testable; the kinds enumerated here mirror PDX-114's
 * `DeployConfig.kind` enum on the daemon side.
 */
export function buildCommandForKind(kind: string | undefined): string {
  switch (kind) {
    case "cloudflare_worker":
      // Default Cloudflare Worker target uses the monorepo build.
      return "npm run build";
    case "npm_publish":
      return "npm run build";
    case "cargo_publish":
      // `cargo publish --dry-run` validates the manifest + features
      // without actually pushing to crates.io. The real publish lives
      // in the deploy step.
      return "cargo build --release";
    case "gh_release":
      // Releases ship binaries; build them deterministically.
      return "cargo build --release";
    default:
      // PDX-25 default path — preserves the historical console-log
      // behaviour for callers that don't specify a kind.
      return "npm run build";
  }
}

/**
 * Render the test command for a given deploy kind.
 */
export function testCommandForKind(kind: string | undefined): string {
  switch (kind) {
    case "cargo_publish":
    case "gh_release":
      return "cargo test --workspace";
    default:
      return "npm test";
  }
}

/**
 * Render the deploy command for a given (kind, env_name, artifact).
 * Pure — exported so unit tests can pin the exact CLI invocations the
 * runner is asked to execute, even without a live runner binding.
 */
export function deployCommandFor(
  kind: string | undefined,
  envName: string | undefined,
  artifact: string
): string {
  switch (kind) {
    case "cloudflare_worker":
      return envName
        ? `wrangler deploy --env ${envName}`
        : "wrangler deploy";
    case "npm_publish":
      // `--access public` is the conservative default; private
      // packages are uncommon enough that the explicit override
      // surface lives in the deploy runner, not the workflow.
      return "npm publish --access public";
    case "cargo_publish":
      return "cargo publish";
    case "gh_release":
      // The artifact is the tag name in this kind.
      return `gh release create ${artifact} --generate-notes`;
    default:
      // PDX-25 default — keep the legacy log payload so existing
      // observability dashboards still see the deploy line.
      return `echo "deploy ${artifact}"`;
  }
}

/**
 * Invoke a deploy-runner step. When the optional `DEPLOY_RUNNER`
 * binding is unbound the function logs and returns `ok: true` so
 * existing PDX-25 callers keep working.
 *
 * Exported so callers can inject a fake `runner` in tests without a
 * full Workers `Fetcher` mock.
 */
export async function invokeRunner(
  runner: Fetcher | undefined,
  request: DeployRunnerRequest
): Promise<DeployRunnerResponse> {
  if (!runner) {
    console.log(
      `[DeployWorkflow][${request.step.toUpperCase()}] (no runner bound) ${request.command}`
    );
    return { ok: true };
  }
  const resp = await runner.fetch("https://runner.invalid/run", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(request)
  });
  if (!resp.ok) {
    return {
      ok: false,
      error: `runner returned ${resp.status}: ${await resp.text()}`,
      exit_code: resp.status
    };
  }
  const body = (await resp.json().catch(() => ({}))) as DeployRunnerResponse;
  return body;
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

    const ext = params as ExtendedDeployParams;
    const buildOk = await step.do("build", BUILD_STEP_CONFIG, async () => {
      const command = buildCommandForKind(ext.kind);
      const result = await invokeRunner(this.env.DEPLOY_RUNNER, {
        step: "build",
        command,
        secrets: ext.secrets ?? [],
        deployId: ext.deployId,
        artifact: ext.artifact,
        kind: ext.kind,
        envName: ext.env_name
      });
      if (!result.ok) {
        throw new Error(`DeployWorkflow: build failed — ${result.error ?? "unknown"}`);
      }
      return true;
    });

    const testsOk = await step.do("test", TEST_STEP_CONFIG, async () => {
      const command = testCommandForKind(ext.kind);
      const result = await invokeRunner(this.env.DEPLOY_RUNNER, {
        step: "test",
        command,
        secrets: ext.secrets ?? [],
        deployId: ext.deployId,
        artifact: ext.artifact,
        kind: ext.kind,
        envName: ext.env_name
      });
      if (!result.ok) {
        throw new Error(`DeployWorkflow: tests failed — ${result.error ?? "unknown"}`);
      }
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
      const command = deployCommandFor(ext.kind, ext.env_name, ext.artifact);
      const result = await invokeRunner(this.env.DEPLOY_RUNNER, {
        step: "deploy",
        command,
        secrets: ext.secrets ?? [],
        deployId: ext.deployId,
        artifact: ext.artifact,
        kind: ext.kind,
        envName: ext.env_name
      });
      if (!result.ok) {
        throw new Error(`DeployWorkflow: deploy failed — ${result.error ?? "unknown"}`);
      }
      console.log(
        `[DeployWorkflow][DEPLOY] deploy=${ext.deployId} target=${ext.target} env=${ext.env_name ?? "-"} approver=${validated.approver}`
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

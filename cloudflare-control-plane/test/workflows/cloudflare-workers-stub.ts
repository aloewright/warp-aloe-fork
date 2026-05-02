/**
 * Test-only stub for the `cloudflare:workers` virtual module.
 *
 * In production this module is provided by the Workerd runtime. For unit
 * tests of pure orchestration logic we only need the class shape — the
 * Workflow runtime itself isn't exercised under vitest because step
 * persistence is a runtime concern verified at integration time (PDX-29).
 */
export abstract class WorkflowEntrypoint<Env = unknown, T = unknown> {
  protected ctx: unknown;
  protected env: Env;
  constructor(ctx: unknown, env: Env) {
    this.ctx = ctx;
    this.env = env;
  }
  abstract run(event: { payload: T }, step: unknown): Promise<unknown>;
}

export class WorkflowStep {}

export type WorkflowEvent<T> = {
  payload: Readonly<T>;
  timestamp: Date;
  instanceId: string;
};

export type WorkflowStepEvent<T> = {
  payload: Readonly<T>;
  timestamp: Date;
  type: string;
};

export type WorkflowStepConfig = {
  retries?: { limit: number; delay: string | number; backoff?: string };
  timeout?: string | number;
};

export type WorkflowTimeoutDuration = string | number;

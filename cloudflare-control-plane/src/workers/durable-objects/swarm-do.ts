/**
 * SwarmDO — placeholder for multi-session swarm coordination.
 *
 * The functional swarm story currently lives in `SwarmWorkflow` (PDX-25).
 * SwarmDO will eventually own swarm-scoped state that spans Workflow runs
 * (e.g. shared scratchpad, agent leases, cross-session voting). For now it
 * is wired into wrangler so the migration is in place; calls return 501.
 */

/**
 * Minimal `DurableObjectState` subset we depend on. Mirrors the shape used
 * by the other DOs in this directory — see session-do.ts for the rationale.
 */
export interface SwarmDOState {
  storage: {
    get<T>(key: string): Promise<T | undefined>;
    put<T>(key: string, value: T): Promise<void>;
  };
}

export class SwarmDO {
  constructor(protected readonly state: SwarmDOState) {}

  async fetch(_request: Request): Promise<Response> {
    return new Response(
      JSON.stringify({
        error: "not_implemented",
        message: "SwarmDO is a stub; see PDX-20 follow-up."
      }),
      {
        status: 501,
        headers: { "content-type": "application/json" }
      }
    );
  }
}

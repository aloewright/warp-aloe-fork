/**
 * RepoDO — placeholder for repo-scoped state.
 *
 * Eventually owns: per-repo lease for Symphony reconciliation, R2 checkpoint
 * pointers, and the live commit graph the agent-runtime container reads from.
 * Wired into wrangler today so the DO migration v2 is monotonic; calls
 * return 501 until PDX-20 follow-ups land.
 */

/** Minimal DurableObjectState subset — see session-do.ts. */
export interface RepoDOState {
  storage: {
    get<T>(key: string): Promise<T | undefined>;
    put<T>(key: string, value: T): Promise<void>;
  };
}

export class RepoDO {
  constructor(protected readonly state: RepoDOState) {}

  async fetch(_request: Request): Promise<Response> {
    return new Response(
      JSON.stringify({
        error: "not_implemented",
        message: "RepoDO is a stub; see PDX-20 follow-up."
      }),
      {
        status: 501,
        headers: { "content-type": "application/json" }
      }
    );
  }
}

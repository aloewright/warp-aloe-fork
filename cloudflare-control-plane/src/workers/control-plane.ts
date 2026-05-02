/**
 * Helm control-plane Worker entry (PDX-19).
 *
 * Routing now lives in {@link ./app.ts} (Hono). This file is a thin shell
 * that:
 *   - Exports the `ControlPlaneRegistry` Durable Object class (preserved for
 *     wrangler binding compatibility from PDX-19's earlier scaffold).
 *   - Re-exports the PDX-20 Durable Object classes (`SessionDO`, `UserDO`,
 *     `SwarmDO`, `RepoDO`) so the wrangler runtime can bind them.
 *   - Re-exports the `AuthenticatedRequestContext` type for downstream tests.
 *   - Mounts the Hono app under the default `fetch` export.
 *   - Forwards Cron Triggers to the workflows scheduled handler shape so a
 *     future deployment can run cron on this Worker without splitting in
 *     two (PDX-25 already binds the workflows; the actual scheduled fan-out
 *     stays with the workflows Worker today).
 *
 * The Hono app accepts the helm session JWT either via `Authorization: Bearer`
 * or via a `?token=` query parameter — the WebSocket route relies on this so
 * browser clients (which can't set custom headers on `new WebSocket(...)`)
 * can still authenticate.
 */
import { json } from "../shared/http.js";
import { appSingleton, type ControlPlaneEnv } from "./app.js";

// Re-export the PDX-20 Durable Object classes so they are available to the
// Workers runtime. wrangler.control-plane.toml lists each by `class_name`.
export { SessionDO, UserDO, SwarmDO, RepoDO } from "./durable-objects/index.js";

export class ControlPlaneRegistry {
  constructor(private readonly state: DurableObjectState) {}

  async fetch(): Promise<Response> {
    const initializedAt = await this.state.storage.get<string>("initializedAt");
    if (!initializedAt) {
      await this.state.storage.put("initializedAt", new Date().toISOString());
    }
    return json({ ok: true, initializedAt: initializedAt ?? "created" });
  }
}

export default {
  async fetch(request: Request, env: ControlPlaneEnv, ctx: ExecutionContext): Promise<Response> {
    return appSingleton().fetch(request, env, ctx);
  },

  /**
   * Cron handler placeholder. Today the helm-workflows Worker owns the cron
   * fan-out (see `src/workers/workflows/index.ts`). When the helm-cloud
   * extraction merges everything into a single Worker, lift that handler in
   * here. Until then, this is a no-op so a stray cron binding doesn't
   * crash the deployment.
   */
  async scheduled(_controller: ScheduledController, _env: ControlPlaneEnv): Promise<void> {
    return;
  }
};

export type { AuthenticatedRequestContext } from "../shared/auth.js";
export type { ControlPlaneEnv } from "./app.js";

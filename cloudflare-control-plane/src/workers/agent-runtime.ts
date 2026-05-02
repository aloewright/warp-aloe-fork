import { verifyHelmJwt, extractHelmJwt, type AuthEnv } from "../shared/auth.js";
import { health, json, methodNotAllowed, notFound, requireAccess } from "../shared/http.js";
import { manifestForRuntime, type HelmEnvironment } from "../shared/manifest.js";

interface Env extends AuthEnv {
  HELM_ENVIRONMENT: HelmEnvironment;
  HELM_VERSION: string;
  HELM_BUILD_ID: string;
  HELM_MANIFEST_JSON: string;
  RUNTIME_SESSION_COORDINATOR: DurableObjectNamespace;
}

export class RuntimeSessionCoordinator {
  constructor(private readonly state: DurableObjectState) {}

  async fetch(request: Request): Promise<Response> {
    if (request.method !== "POST") return methodNotAllowed();
    const sessionId = crypto.randomUUID();
    await this.state.storage.put(`session:${sessionId}`, {
      id: sessionId,
      status: "created",
      createdAt: new Date().toISOString()
    });
    return json({ id: sessionId, status: "created", containersStarted: false }, { status: 201 });
  }
}

/**
 * Authenticate an inbound runtime request.
 *
 * Two acceptable shapes (in priority order):
 *
 *   1. Helm session JWT in `Authorization: Bearer <jwt>` — issued by the
 *      control plane's `/api/auth/session` (PDX-23). This is the cheap,
 *      cache-free path used by every authenticated client request.
 *
 *   2. Cloudflare Access JWT in `Cf-Access-Jwt-Assertion` — used for
 *      operator / out-of-band traffic that bypasses the helm session
 *      issuance flow (e.g. a dashboard ping). When `AUTH_KV` is not bound
 *      on this Worker (PDX-21 reserves wrangler.agent-runtime.toml so we
 *      cannot edit bindings here), JWKS caching is a no-op — verification
 *      still works, just with the per-request JWKS fetch.
 *
 * The Access path requires `requireAccess` because that's also what the
 * existing tests rely on. The helm path is preferred and short-circuits
 * the Access fetch when present.
 */
async function requireRuntimeAuth(request: Request, env: Env): Promise<Response | null> {
  const helmToken = extractHelmJwt(request);
  if (helmToken && env.HELM_JWT_SIGNING_KEY) {
    const result = await verifyHelmJwt(helmToken, {
      signingKey: env.HELM_JWT_SIGNING_KEY,
      authKv: env.AUTH_KV
    });
    if (result.ok) return null;
    return json({ error: "unauthorized", reason: result.reason }, { status: 401 });
  }
  const manifest = manifestForRuntime(env);
  return requireAccess(request, manifest.access, env.HELM_ENVIRONMENT, env.AUTH_KV);
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);

    if (url.pathname === "/api/health") {
      return health({
        service: "helm-agent-runtime",
        environment: env.HELM_ENVIRONMENT,
        version: env.HELM_VERSION,
        buildId: env.HELM_BUILD_ID
      });
    }

    const authFailure = await requireRuntimeAuth(request, env);
    if (authFailure) return authFailure;

    if (url.pathname === "/api/runtime/sessions") {
      if (request.method !== "POST") return methodNotAllowed();
      const id = env.RUNTIME_SESSION_COORDINATOR.idFromName(env.HELM_ENVIRONMENT);
      const object = env.RUNTIME_SESSION_COORDINATOR.get(id);
      return object.fetch(request);
    }

    return notFound();
  }
};

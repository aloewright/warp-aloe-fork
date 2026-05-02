import { health, json, methodNotAllowed, notFound, requireAccess } from "../shared/http.js";
import { manifestForRuntime, type HelmEnvironment } from "../shared/manifest.js";

interface Env {
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

    const manifest = manifestForRuntime(env);
    const accessFailure = requireAccess(request, manifest.access.required);
    if (accessFailure) return accessFailure;

    if (url.pathname === "/api/runtime/sessions") {
      if (request.method !== "POST") return methodNotAllowed();
      const id = env.RUNTIME_SESSION_COORDINATOR.idFromName(env.HELM_ENVIRONMENT);
      const object = env.RUNTIME_SESSION_COORDINATOR.get(id);
      return object.fetch(request);
    }

    return notFound();
  }
};

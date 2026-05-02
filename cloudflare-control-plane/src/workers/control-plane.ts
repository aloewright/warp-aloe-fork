import { auditInventory, createCloudflareClient, fetchInventory } from "../shared/cloudflare.js";
import { health, json, methodNotAllowed, notFound, requireAccess } from "../shared/http.js";
import { assertEnvironment, manifestForRuntime, type HelmEnvironment } from "../shared/manifest.js";

interface Env {
  HELM_ENVIRONMENT: HelmEnvironment;
  HELM_VERSION: string;
  HELM_BUILD_ID: string;
  HELM_MANIFEST_JSON: string;
  CLOUDFLARE_API_TOKEN?: string;
  CONTROL_PLANE_REGISTRY: DurableObjectNamespace;
}

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

async function resources(env: Env): Promise<Response> {
  const manifest = manifestForRuntime(env);
  assertEnvironment(manifest, env.HELM_ENVIRONMENT);
  if (!env.CLOUDFLARE_API_TOKEN) {
    return json(
      {
        manifest,
        reconciliation: {
          skipped: true,
          reason: "CLOUDFLARE_API_TOKEN is not configured."
        }
      },
      { status: 200 },
    );
  }

  const client = createCloudflareClient(env.CLOUDFLARE_API_TOKEN);
  const inventory = await fetchInventory(client, manifest.accountId);
  return json({
    manifest,
    reconciliation: auditInventory(manifest, env.HELM_ENVIRONMENT, inventory)
  });
}

async function onboardingCheck(request: Request, env: Env): Promise<Response> {
  if (request.method !== "POST") return methodNotAllowed();

  const manifest = manifestForRuntime(env);
  const body = (await request.json().catch(() => ({}))) as { environment?: string };
  const targetEnvironment = body.environment ?? env.HELM_ENVIRONMENT;
  assertEnvironment(manifest, targetEnvironment);

  return json({
    environment: targetEnvironment,
    account: {
      id: manifest.accountId,
      configured: !manifest.accountId.startsWith("replace-with")
    },
    zone: {
      id: manifest.zone.id,
      domain: manifest.zone.domain,
      configured: !manifest.zone.id.startsWith("replace-with")
    },
    access: {
      required: manifest.access.required,
      teamDomain: manifest.access.teamDomain,
      audienceConfigured: Boolean(manifest.access.audiences[targetEnvironment])
    },
    resources: {
      d1: Object.keys(manifest.resources.d1).filter((name) =>
        manifest.resources.d1[name]?.environment === targetEnvironment ||
        manifest.resources.d1[name]?.environment === "all"
      ),
      r2: Object.keys(manifest.resources.r2).filter((name) =>
        manifest.resources.r2[name]?.environment === targetEnvironment ||
        manifest.resources.r2[name]?.environment === "all"
      ),
      durableObjects: Object.keys(manifest.resources.durableObjects)
    },
    containers: manifest.containers[targetEnvironment]
  });
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);

    if (url.pathname === "/api/health") {
      return health({
        service: "helm-control-plane",
        environment: env.HELM_ENVIRONMENT,
        version: env.HELM_VERSION,
        buildId: env.HELM_BUILD_ID
      });
    }

    const manifest = manifestForRuntime(env);
    const accessFailure = requireAccess(request, manifest.access.required);
    if (accessFailure) return accessFailure;

    if (url.pathname === "/api/environments") {
      if (request.method !== "GET") return methodNotAllowed();
      return json({
        environments: manifest.environments.map((environment) => ({
          name: environment,
          configured: Boolean(manifest.access.audiences[environment]),
          containers: manifest.containers[environment]
        }))
      });
    }

    if (url.pathname === "/api/onboarding/check") {
      return onboardingCheck(request, env);
    }

    if (url.pathname === "/api/resources") {
      if (request.method !== "GET") return methodNotAllowed();
      return resources(env);
    }

    return notFound();
  }
};

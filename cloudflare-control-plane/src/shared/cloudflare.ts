import type { HelmEnvironment, HelmManifest } from "./manifest.js";
import { expectedWorkerNames, isProtectedResource, workerName } from "./manifest.js";

const CF_BASE = "https://api.cloudflare.com/client/v4";

export interface CloudflareClient {
  get(path: string): Promise<unknown>;
  delete(path: string): Promise<unknown>;
}

export function createCloudflareClient(token: string): CloudflareClient {
  async function request(path: string, init: RequestInit = {}): Promise<unknown> {
    const res = await fetch(`${CF_BASE}${path}`, {
      ...init,
      headers: {
        Authorization: `Bearer ${token}`,
        "Content-Type": "application/json",
        ...(init.headers as Record<string, string> | undefined)
      }
    });
    const body = (await res.json()) as {
      success: boolean;
      errors?: Array<{ message: string }>;
      result: unknown;
    };
    if (!res.ok || !body.success) {
      const msg = body.errors?.map((error) => error.message).join("; ") ?? `HTTP ${res.status}`;
      throw new Error(`Cloudflare API error: ${msg}`);
    }
    return body.result;
  }

  return {
    get: (path) => request(path),
    delete: (path) => request(path, { method: "DELETE" })
  };
}

interface NamedResource {
  id?: string;
  name?: string;
  title?: string;
  tags?: string[];
}

export interface LiveInventory {
  workers: NamedResource[];
  workerSettings: Record<string, { tags?: string[]; unavailable?: boolean }>;
  d1: NamedResource[];
  r2: NamedResource[];
  kv: NamedResource[];
  aiGateways: NamedResource[];
}

export async function fetchInventory(
  client: CloudflareClient,
  accountId: string,
): Promise<LiveInventory> {
  const [workers, d1, r2, kv, aiGateways] = await Promise.all([
    client.get(`/accounts/${accountId}/workers/scripts`),
    client.get(`/accounts/${accountId}/d1/database`),
    client.get(`/accounts/${accountId}/r2/buckets`),
    client.get(`/accounts/${accountId}/storage/kv/namespaces`),
    client.get(`/accounts/${accountId}/ai-gateway/gateways`)
  ]);

  const workerList = arrayResult(workers);
  const workerSettings = await fetchWorkerSettings(client, accountId, workerList);

  return {
    workers: workerList,
    workerSettings,
    d1: arrayResult(d1),
    r2: arrayResult(r2),
    kv: arrayResult(kv),
    aiGateways: arrayResult(aiGateways)
  };
}

async function fetchWorkerSettings(
  client: CloudflareClient,
  accountId: string,
  workers: NamedResource[],
): Promise<Record<string, { tags?: string[]; unavailable?: boolean }>> {
  const helmWorkers = workers
    .map(resourceName)
    .filter((name) => name.startsWith("helm-"));

  const entries = await Promise.all(
    helmWorkers.map(async (name) => {
      try {
        const settings = (await client.get(
          `/accounts/${accountId}/workers/scripts/${name}/settings`,
        )) as { tags?: string[] };
        return [name, { tags: settings.tags ?? [] }] as const;
      } catch {
        return [name, { unavailable: true }] as const;
      }
    }),
  );

  return Object.fromEntries(entries);
}

function arrayResult(value: unknown): NamedResource[] {
  if (Array.isArray(value)) return value as NamedResource[];
  if (
    value &&
    typeof value === "object" &&
    "items" in value &&
    Array.isArray((value as { items: unknown }).items)
  ) {
    return (value as { items: NamedResource[] }).items;
  }
  return [];
}

function resourceName(resource: NamedResource): string {
  return resource.name ?? resource.id ?? resource.title ?? "";
}

export interface AuditReport {
  environment: HelmEnvironment;
  expectedWorkers: string[];
  missingExpectedResources: string[];
  extraUnownedWorkers: string[];
  staleEnvironmentWorkers: string[];
  workersWithoutHelmMetadata: string[];
  workersWithUnknownHelmMetadata: string[];
  containersEnabledButUnused: boolean;
}

export function auditInventory(
  manifest: HelmManifest,
  env: HelmEnvironment,
  inventory: LiveInventory,
): AuditReport {
  const expectedWorkers = expectedWorkerNames(manifest, env);
  const workerNames = inventory.workers.map(resourceName).filter(Boolean);
  const missingExpectedResources = expectedWorkers.filter((name) => !workerNames.includes(name));
  const expectedAllEnvWorkers = new Set(
    manifest.environments.flatMap((environment) =>
      Object.keys(manifest.workers).map((baseName) => workerName(baseName, environment)),
    ),
  );

  const extraUnownedWorkers = workerNames.filter(
    (name) => name.startsWith("helm-") && !expectedAllEnvWorkers.has(name),
  );
  const staleEnvironmentWorkers = workerNames.filter((name) => {
    if (!name.startsWith("helm-") || expectedAllEnvWorkers.has(name)) return false;
    return name.endsWith("-dev") || name.endsWith("-staging") || name.endsWith("-production");
  });

  const workersWithoutHelmMetadata = inventory.workers
    .filter((worker) => {
      const name = resourceName(worker);
      if (!expectedAllEnvWorkers.has(name)) return false;
      const settings = inventory.workerSettings[name];
      if (settings?.unavailable) return false;
      const tags = settings?.tags ?? worker.tags ?? [];
      return !tags.some((tag) => tag.startsWith("helm."));
    })
    .map(resourceName)
    .filter(Boolean);

  const workersWithUnknownHelmMetadata = inventory.workers
    .filter((worker) => {
      const name = resourceName(worker);
      return expectedAllEnvWorkers.has(name) && inventory.workerSettings[name]?.unavailable === true;
    })
    .map(resourceName)
    .filter(Boolean);

  return {
    environment: env,
    expectedWorkers,
    missingExpectedResources,
    extraUnownedWorkers,
    staleEnvironmentWorkers,
    workersWithoutHelmMetadata,
    workersWithUnknownHelmMetadata,
    containersEnabledButUnused:
      manifest.containers[env]?.enabled === true &&
      !workerNames.includes(workerName("helm-agent-runtime", env))
  };
}

export interface CleanupCandidate {
  type: "worker" | "d1" | "r2" | "kv" | "aiGateway";
  name: string;
  id?: string;
  protected: boolean;
  production: boolean;
}

export function cleanupCandidates(
  manifest: HelmManifest,
  env: HelmEnvironment,
  inventory: LiveInventory,
): CleanupCandidate[] {
  const expected = new Set(
    manifest.environments.flatMap((environment) => expectedWorkerNames(manifest, environment)),
  );
  const candidates: CleanupCandidate[] = [];

  for (const worker of inventory.workers) {
    const name = resourceName(worker);
    if (!name.startsWith("helm-") || expected.has(name)) continue;
    candidates.push({
      type: "worker",
      name,
      id: worker.id,
      protected: isProtectedResource(manifest, "worker", name),
      production: name.endsWith("-production")
    });
  }

  return candidates;
}

export function assertDeletionAllowed(
  candidate: CleanupCandidate,
  options: { production: boolean; confirm?: string },
): void {
  if (candidate.protected) {
    throw new Error(`${candidate.name} is protected and cannot be deleted by cleanup.`);
  }
  if (candidate.production && (!options.production || options.confirm !== candidate.name)) {
    throw new Error(
      `Production deletion requires --production --confirm ${candidate.name}.`,
    );
  }
}

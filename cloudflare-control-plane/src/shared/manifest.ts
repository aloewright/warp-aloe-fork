import { z } from "zod";

export const HELM_ENVIRONMENTS = ["dev", "staging", "production"] as const;
export type HelmEnvironment = (typeof HELM_ENVIRONMENTS)[number];

const HelmEnvironmentSchema = z.enum(HELM_ENVIRONMENTS);
const EnvironmentStringMapSchema = z.object({
  dev: z.string().min(1).optional(),
  staging: z.string().min(1).optional(),
  production: z.string().min(1).optional()
});
const EnvironmentContainerMapSchema = z.object({
  dev: z.object({ enabled: z.boolean(), instanceClass: z.string().min(1) }),
  staging: z.object({ enabled: z.boolean(), instanceClass: z.string().min(1) }),
  production: z.object({ enabled: z.boolean(), instanceClass: z.string().min(1) })
});

const ResourceEntrySchema = z.object({
  environment: z.union([HelmEnvironmentSchema, z.literal("all")]),
  protected: z.boolean().default(false),
  ttlDays: z.number().int().positive().nullable().optional()
});

export const HelmManifestSchema = z.object({
  accountId: z.string().min(1),
  zone: z.object({
    id: z.string().min(1),
    domain: z.string().min(1)
  }),
  environments: z.array(HelmEnvironmentSchema).min(1),
  workers: z.record(
    z.object({
      routes: EnvironmentStringMapSchema.default({}),
      metadata: z.record(z.string(), z.string()).default({})
    })
  ),
  resources: z.object({
    d1: z.record(ResourceEntrySchema).default({}),
    r2: z.record(ResourceEntrySchema).default({}),
    durableObjects: z
      .record(
        ResourceEntrySchema.extend({
          className: z.string().min(1),
          sqliteBacked: z.boolean().default(true)
        })
      )
      .default({}),
    kv: z.record(ResourceEntrySchema).default({}),
    aiGateways: z.record(ResourceEntrySchema).default({})
  }),
  containers: EnvironmentContainerMapSchema,
  access: z.object({
    required: z.boolean().default(true),
    teamDomain: z.string().min(1),
    audiences: EnvironmentStringMapSchema.default({})
  }),
  protected: z
    .array(
      z.object({
        type: z.enum(["worker", "d1", "r2", "durableObject", "kv", "aiGateway"]),
        name: z.string().min(1),
        reason: z.string().min(1)
      })
    )
    .default([])
});

export type HelmManifest = z.infer<typeof HelmManifestSchema>;

export function parseManifest(value: unknown): HelmManifest {
  return HelmManifestSchema.parse(value);
}

export function parseManifestJson(json: string): HelmManifest {
  return parseManifest(JSON.parse(json) as unknown);
}

export function assertEnvironment(
  manifest: HelmManifest,
  env: string,
): asserts env is HelmEnvironment {
  if (!HELM_ENVIRONMENTS.includes(env as HelmEnvironment)) {
    throw new Error(`Unsupported environment "${env}". Expected dev, staging, or production.`);
  }
  if (!manifest.environments.includes(env as HelmEnvironment)) {
    throw new Error(`Environment "${env}" is not enabled in helm.cloudflare.json.`);
  }
}

export function workerName(baseName: string, env: HelmEnvironment): string {
  return `${baseName}-${env}`;
}

export function expectedWorkerNames(manifest: HelmManifest, env: HelmEnvironment): string[] {
  return Object.keys(manifest.workers).map((baseName) => workerName(baseName, env));
}

export function isProtectedResource(
  manifest: HelmManifest,
  type: HelmManifest["protected"][number]["type"],
  name: string,
): boolean {
  if (manifest.protected.some((entry) => entry.type === type && entry.name === name)) {
    return true;
  }

  if (type === "d1") return manifest.resources.d1[name]?.protected ?? false;
  if (type === "r2") return manifest.resources.r2[name]?.protected ?? false;
  if (type === "durableObject") return manifest.resources.durableObjects[name]?.protected ?? false;
  if (type === "kv") return manifest.resources.kv[name]?.protected ?? false;
  if (type === "aiGateway") return manifest.resources.aiGateways[name]?.protected ?? false;

  return false;
}

let cachedManifest: HelmManifest | null = null;
let cachedManifestJson: string | null = null;

export function manifestForRuntime(env: { HELM_MANIFEST_JSON?: string }): HelmManifest {
  const json = env.HELM_MANIFEST_JSON;
  if (!json) {
    throw new Error("HELM_MANIFEST_JSON is required for runtime manifest-backed APIs.");
  }
  if (json === cachedManifestJson && cachedManifest) {
    return cachedManifest;
  }
  cachedManifest = parseManifestJson(json);
  cachedManifestJson = json;
  return cachedManifest;
}

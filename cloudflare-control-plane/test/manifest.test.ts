import { describe, expect, it } from "vitest";
import {
  assertEnvironment,
  expectedWorkerNames,
  isProtectedResource,
  parseManifest,
  workerName
} from "../src/shared/manifest.js";

const manifest = parseManifest({
  accountId: "acct",
  zone: { id: "zone", domain: "example.com" },
  environments: ["dev", "staging", "production"],
  workers: {
    "helm-control-plane": { routes: {}, metadata: { "helm.owner": "control-plane" } },
    "helm-agent-runtime": { routes: {}, metadata: { "helm.owner": "agent-runtime" } },
    "helm-cloudflare-mcp": { routes: {}, metadata: { "helm.owner": "mcp" } }
  },
  resources: {
    d1: {
      "helm-control-plane-production": {
        environment: "production",
        protected: true
      }
    },
    r2: {},
    durableObjects: {},
    kv: {},
    aiGateways: {}
  },
  containers: {
    dev: { enabled: false, instanceClass: "dev" },
    staging: { enabled: false, instanceClass: "standard" },
    production: { enabled: true, instanceClass: "standard" }
  },
  access: {
    required: true,
    teamDomain: "team.cloudflareaccess.com",
    audiences: { dev: "dev-aud" }
  },
  protected: [{ type: "worker", name: "helm-cloudflare-mcp-production", reason: "prod" }]
});

describe("manifest parsing and naming", () => {
  it("uses Wrangler environment worker names", () => {
    expect(workerName("helm-control-plane", "dev")).toBe("helm-control-plane-dev");
    expect(expectedWorkerNames(manifest, "staging")).toEqual([
      "helm-control-plane-staging",
      "helm-agent-runtime-staging",
      "helm-cloudflare-mcp-staging"
    ]);
  });

  it("rejects environments outside the manifest", () => {
    expect(() => assertEnvironment(manifest, "qa")).toThrow(/Unsupported environment/);
  });

  it("honors explicit and resource-level protected markers", () => {
    expect(isProtectedResource(manifest, "worker", "helm-cloudflare-mcp-production")).toBe(true);
    expect(isProtectedResource(manifest, "d1", "helm-control-plane-production")).toBe(true);
    expect(isProtectedResource(manifest, "worker", "helm-control-plane-dev")).toBe(false);
  });
});

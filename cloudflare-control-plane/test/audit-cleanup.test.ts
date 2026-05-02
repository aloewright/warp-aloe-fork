import { describe, expect, it } from "vitest";
import {
  assertDeletionAllowed,
  auditInventory,
  cleanupCandidates
} from "../src/shared/cloudflare.js";
import { parseManifest } from "../src/shared/manifest.js";

const manifest = parseManifest({
  accountId: "acct",
  zone: { id: "zone", domain: "example.com" },
  environments: ["dev", "staging", "production"],
  workers: {
    "helm-control-plane": { routes: {}, metadata: {} },
    "helm-agent-runtime": { routes: {}, metadata: {} },
    "helm-cloudflare-mcp": { routes: {}, metadata: {} }
  },
  resources: { d1: {}, r2: {}, durableObjects: {}, kv: {}, aiGateways: {} },
  containers: {
    dev: { enabled: true, instanceClass: "dev" },
    staging: { enabled: false, instanceClass: "standard" },
    production: { enabled: false, instanceClass: "standard" }
  },
  access: { required: true, teamDomain: "team.cloudflareaccess.com", audiences: {} },
  protected: [{ type: "worker", name: "helm-keep-dev", reason: "fixture" }]
});

const inventory = {
  workers: [
    { id: "1", name: "helm-control-plane-dev" },
    { id: "2", name: "helm-cloudflare-mcp-dev" },
    { id: "3", name: "helm-extra-dev" },
    { id: "4", name: "helm-extra-production" },
    { id: "5", name: "helm-keep-dev" },
    { id: "6", name: "helm-control-plane-staging" }
  ],
  workerSettings: {
    "helm-control-plane-dev": { tags: ["helm.owner:control-plane"] },
    "helm-cloudflare-mcp-dev": { tags: [] }
  },
  d1: [],
  r2: [],
  kv: [],
  aiGateways: []
};

describe("audit and cleanup", () => {
  it("reports missing workers, extra helm workers, metadata gaps, and unused containers", () => {
    const report = auditInventory(manifest, "dev", inventory);
    expect(report.missingExpectedResources).toEqual(["helm-agent-runtime-dev"]);
    expect(report.extraUnownedWorkers).toContain("helm-extra-dev");
    expect(report.workersWithoutHelmMetadata).toContain("helm-cloudflare-mcp-dev");
    expect(report.containersEnabledButUnused).toBe(true);
  });

  it("requires production confirmation before deletion", () => {
    const production = cleanupCandidates(manifest, "dev", inventory).find(
      (candidate) => candidate.name === "helm-extra-production",
    );
    expect(production).toBeDefined();
    expect(() =>
      assertDeletionAllowed(production!, { production: false }),
    ).toThrow(/Production deletion requires/);
    expect(() =>
      assertDeletionAllowed(production!, {
        production: true,
        confirm: "helm-extra-production"
      }),
    ).not.toThrow();
  });

  it("does not offer expected workers from other environments for cleanup", () => {
    const candidates = cleanupCandidates(manifest, "dev", inventory);
    expect(candidates.map((candidate) => candidate.name)).not.toContain(
      "helm-control-plane-staging",
    );
  });

  it("does not allow protected resource deletion", () => {
    const protectedCandidate = cleanupCandidates(manifest, "dev", inventory).find(
      (candidate) => candidate.name === "helm-keep-dev",
    );
    expect(protectedCandidate).toBeDefined();
    expect(() =>
      assertDeletionAllowed(protectedCandidate!, { production: false, confirm: "helm-keep-dev" }),
    ).toThrow(/protected/);
  });
});

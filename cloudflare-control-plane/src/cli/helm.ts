#!/usr/bin/env node
import { readFile } from "node:fs/promises";
import { resolve } from "node:path";
import { spawn } from "node:child_process";
import {
  assertDeletionAllowed,
  auditInventory,
  cleanupCandidates,
  createCloudflareClient,
  fetchInventory
} from "../shared/cloudflare.js";
import { assertEnvironment, parseManifestJson, type HelmEnvironment } from "../shared/manifest.js";

interface Args {
  command: string[];
  flags: Record<string, string | boolean>;
}

function parseArgs(argv: string[]): Args {
  const command: string[] = [];
  const flags: Record<string, string | boolean> = {};

  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (!arg) continue;
    if (!arg.startsWith("--")) {
      command.push(arg);
      continue;
    }
    const key = arg.slice(2);
    const next = argv[i + 1];
    if (!next || next.startsWith("--")) {
      flags[key] = true;
    } else {
      flags[key] = next;
      i += 1;
    }
  }

  return { command, flags };
}

async function loadManifest() {
  const path = resolve(process.cwd(), "helm.cloudflare.json");
  return parseManifestJson(await readFile(path, "utf8"));
}

function requireEnv(flags: Record<string, string | boolean>): string {
  const env = flags.env;
  if (typeof env !== "string") {
    throw new Error("--env is required.");
  }
  return env;
}

async function withInventory(envName: string) {
  const manifest = await loadManifest();
  assertEnvironment(manifest, envName);
  const token = process.env.CLOUDFLARE_API_TOKEN;
  if (!token) throw new Error("CLOUDFLARE_API_TOKEN is required.");
  const inventory = await fetchInventory(createCloudflareClient(token), manifest.accountId);
  return { manifest, inventory, env: envName as HelmEnvironment };
}

async function run(command: string, args: string[]): Promise<void> {
  await new Promise<void>((resolveRun, reject) => {
    const child = spawn(command, args, { stdio: "inherit" });
    child.on("error", reject);
    child.on("exit", (code) => {
      if (code === 0) {
        resolveRun();
      } else {
        reject(new Error(`${command} exited with code ${code ?? "unknown"}.`));
      }
    });
  });
}

async function main(): Promise<void> {
  const { command, flags } = parseArgs(process.argv.slice(2));
  if (command[0] !== "cloud") {
    throw new Error("Expected command: helm cloud <init|check|audit|cleanup|deploy>.");
  }

  switch (command[1]) {
    case "init": {
      const manifest = await loadManifest();
      console.log(JSON.stringify({
        nextPrompts: [
          "Cloudflare account ID",
          "Cloudflare zone ID and domain",
          "Target environment: dev, staging, or production",
          "Cloudflare Access team domain and audience",
          "Containers enabled for this environment",
          "Create vs reuse for D1, R2, and Durable Objects"
        ],
        manifest
      }, null, 2));
      return;
    }
    case "check": {
      const env = requireEnv(flags);
      const manifest = await loadManifest();
      assertEnvironment(manifest, env);
      console.log(JSON.stringify({
        environment: env,
        access: manifest.access,
        containers: manifest.containers[env],
        workers: Object.keys(manifest.workers)
      }, null, 2));
      return;
    }
    case "audit": {
      const env = requireEnv(flags);
      const result = await withInventory(env);
      console.log(JSON.stringify(auditInventory(result.manifest, result.env, result.inventory), null, 2));
      return;
    }
    case "cleanup": {
      const env = requireEnv(flags);
      const result = await withInventory(env);
      const candidates = cleanupCandidates(result.manifest, result.env, result.inventory);
      if (!flags.confirm) {
        console.log(JSON.stringify({ dryRun: true, candidates }, null, 2));
        return;
      }
      for (const candidate of candidates) {
        assertDeletionAllowed(candidate, {
          production: flags.production === true,
          confirm: typeof flags.confirm === "string" ? flags.confirm : undefined
        });
        if (candidate.type === "worker") {
          await createCloudflareClient(process.env.CLOUDFLARE_API_TOKEN ?? "").delete(
            `/accounts/${result.manifest.accountId}/workers/scripts/${candidate.name}`,
          );
          console.log(`deleted worker ${candidate.name}`);
        }
      }
      return;
    }
    case "deploy": {
      const env = requireEnv(flags);
      const worker = flags.worker;
      if (typeof worker !== "string") throw new Error("--worker is required.");
      const config = worker === "helm-agent-runtime"
        ? "cloudflare-control-plane/wrangler.agent-runtime.toml"
        : worker === "helm-control-plane"
          ? "cloudflare-control-plane/wrangler.control-plane.toml"
          : "cloudflare-mcp/wrangler.toml";
      await run("wrangler", ["deploy", "-c", config, "--env", env]);
      return;
    }
    default:
      throw new Error("Expected command: helm cloud <init|check|audit|cleanup|deploy>.");
  }
}

main().catch((error: unknown) => {
  const message = error instanceof Error ? error.message : String(error);
  console.error(message);
  process.exitCode = 1;
});

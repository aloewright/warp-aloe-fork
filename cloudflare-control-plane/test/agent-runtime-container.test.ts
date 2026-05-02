import { execSync, spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

const __dirname = dirname(fileURLToPath(import.meta.url));
const projectRoot = resolve(__dirname, "..");
const dockerfile = resolve(projectRoot, "Dockerfile.agent-runtime");

function dockerAvailable(): boolean {
  try {
    execSync("docker version --format '{{.Server.Version}}'", {
      stdio: "ignore"
    });
    return true;
  } catch {
    return false;
  }
}

const describeIfDocker = dockerAvailable() ? describe : describe.skip;

describe("agent-runtime Dockerfile", () => {
  it("exists at the expected path", () => {
    expect(existsSync(dockerfile)).toBe(true);
  });
});

describeIfDocker("agent-runtime container build (docker required)", () => {
  // Building Ubuntu + Node + npm globals takes a few minutes on a cold cache.
  const tag = "helm/agent-runtime:vitest";

  it(
    "builds and exposes claude + codex on PATH",
    () => {
      const build = spawnSync(
        "docker",
        [
          "build",
          "--file",
          dockerfile,
          "--tag",
          tag,
          "--build-arg",
          "GIT_SHA=vitest",
          projectRoot
        ],
        { stdio: "inherit" }
      );
      expect(build.status).toBe(0);

      const claude = spawnSync(
        "docker",
        ["run", "--rm", tag, "claude", "--version"],
        { encoding: "utf8" }
      );
      expect(claude.status).toBe(0);

      const codex = spawnSync(
        "docker",
        ["run", "--rm", tag, "codex", "--version"],
        { encoding: "utf8" }
      );
      expect(codex.status).toBe(0);
    },
    10 * 60 * 1000
  );
});

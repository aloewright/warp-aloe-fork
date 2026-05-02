# Helm agent-runtime container

The agent-runtime container is the per-session execution environment that the
`helm-agent-runtime` Worker spins up via Cloudflare Containers when a new
agent session is created. It carries the agent CLIs (`claude`, `codex`),
along with `git`, `gh`, and the workspace tooling needed for the agent to
clone a repo, run, and push back changes.

## Image

- File: [`Dockerfile.agent-runtime`](../Dockerfile.agent-runtime)
- Base: `ubuntu:24.04` (glibc, recent enough for both Node 22 and modern Git)
- Installed:
  - Node.js 22 (NodeSource)
  - `@anthropic-ai/claude-code` (`claude`)
  - `@openai/codex` (`codex`)
  - `git`, `gh`, `curl`, `ca-certificates`, `jq`, `openssh-client`, `tini`
- User: non-root `agent` (uid/gid 1001), workspace at `/workspace`
- Entrypoint: [`scripts/agent-entrypoint.sh`](../scripts/agent-entrypoint.sh)
  - Sources `/workspace/.env` if present
  - Runs the requested CLI with unbuffered stdio for line-streaming
  - On `SIGTERM`, forwards to the child and waits up to 30s before `SIGKILL`
- Image is labelled with `helm.git-sha` for traceability.

Build locally:

```bash
cd cloudflare-control-plane
npm run build:container
# or directly:
./scripts/build-agent-runtime.sh
```

## Wrangler binding

The image is wired into `wrangler.agent-runtime.toml` via a top-level
`[[containers]]` block (see `cloudflare-control-plane/wrangler.agent-runtime.toml`):

```toml
[[containers]]
name = "agent-runtime"
image = "./Dockerfile.agent-runtime"
max_instances = 50
instance_type = "standard"
```

The `RuntimeSessionCoordinator` Durable Object (in
`src/workers/agent-runtime.ts`) is the lifecycle owner — it owns the mapping
from `sessionId` → container instance, starts/stops the container, and
proxies requests in.

## R2 workspace mount

Each session's `/workspace` is logically backed by an R2 prefix scoped by
`sessionId` so that work is durable across container restarts and so that
checkpoints can be replayed.

The intended flow (to be implemented as part of PDX-20 SessionDO lifecycle):

1. **Session create** — DO allocates a session id and an R2 prefix
   `sessions/<sessionId>/workspace/`.
2. **Container start** — DO starts the container with environment variables
   pointing at the R2 prefix:
   - `HELM_R2_BUCKET=<bucket>`
   - `HELM_R2_PREFIX=sessions/<sessionId>/workspace/`
   - `HELM_R2_ACCESS_KEY_ID` / `HELM_R2_SECRET_ACCESS_KEY` (scoped, short-TTL)
3. **Workspace hydrate** — entrypoint pulls the prefix into `/workspace`
   on boot (rclone-style sync, TODO).
4. **Checkpoint** — periodically (and on `SIGTERM` graceful shutdown) the
   entrypoint flushes `/workspace` back to R2 as the next checkpoint.
5. **Resume** — a follow-up session id can re-hydrate from the latest
   checkpoint manifest.

> Status: the Dockerfile and the binding ship in PDX-21. The R2 hydrate /
> checkpoint loop and the DO-driven container lifecycle are TODO and will
> land with PDX-20 (SessionDO) and the workspace-checkpoint task that
> follows.

## Lifecycle hooks (TODO — PDX-20)

The DO will need to:

- Call the Containers API to start a new instance keyed by `sessionId`.
- Stream stdout/stderr from the container to the client (the Worker holds
  the SSE/WebSocket; the DO is the source of truth for ordering).
- Send `SIGTERM` on session end and rely on the entrypoint's 30s grace
  window before forced shutdown.
- Persist final checkpoint manifest to D1 (PDX-22) once the container exits.

## Smoke test

A vitest smoke test (`test/agent-runtime-container.test.ts`) builds the
Dockerfile and verifies that both `claude --version` and `codex --version`
resolve inside it. The test is automatically skipped when `docker` is not on
the `PATH` (e.g. in CI runners that don't have Docker).

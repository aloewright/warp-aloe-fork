---
name: doppler-secret-fetch
description: Inject secrets into a build script or runtime via the Doppler CLI with cwd-based scoping. Never read raw .env files, never paste secret values into shells, never log secrets.
roles: []
tags: [doppler, secrets, cli, security]
---

# doppler-secret-fetch

Doppler is the source of truth for every secret in this org. The Doppler CLI uses **cwd-based scopes** (`--scope .`) so that the project/config combo is decided by where the command is run, not by a global default. This makes it safe for an agent to run `doppler run` in any worktree without leaking secrets across projects.

## When to use

- A build script, migration, or seed needs `CF_AIG_TOKEN`, database URLs, API keys, etc.
- A Worker dev session needs secrets injected (`wrangler dev`).
- A one-off Node/Python script needs to talk to a service.
- You're tempted to write `process.env.MY_KEY = "..."` or read a `.env` file. Don't — use Doppler.

## One-time setup (per laptop, per project)

```bash
# Sign in non-interactively if you have a personal token; otherwise interactive
doppler login --yes --scope .

# Bind this project directory to a Doppler project + config
doppler setup --scope . --project helm --config dev
```

`--scope .` writes the binding to `.doppler.yaml` in the current directory (gitignored). Subsequent `doppler` commands run from this tree (or below) automatically pick the right project + config.

**Critical**: never pass `--project` / `--config` ad-hoc to `doppler run` — that bypasses cwd scoping and can silently fetch the wrong environment. Always rely on the bound scope.

## Running a command with secrets injected

```bash
doppler run --scope . -- npm run build
doppler run --scope . -- node scripts/seed.ts
doppler run --scope . -- npx wrangler dev
```

Everything after `--` runs as a child process with all secrets in this Doppler config exported as env vars. The child sees `CF_AIG_TOKEN`, `DATABASE_URL`, etc. as if they had been `export`ed.

## Reading a single secret in a script

```bash
TOKEN=$(doppler secrets get CF_AIG_TOKEN --plain --scope .)
```

`--plain` strips the JSON wrapper. Use this only when piping into another tool (e.g. `wrangler secret put`). Never log `$TOKEN`.

## Listing what's available without revealing values

```bash
doppler secrets --scope .                # names + last-modified, NO values
doppler secrets --only-names --scope .   # just names
```

The Helm Doppler MCP (`crates/doppler_mcp`) exposes only this metadata view to agents — secret values never traverse the MCP boundary. If an agent needs a value, it must shell out to `doppler run` or `doppler secrets get` itself, with the user's local `doppler` auth.

## Per-environment configs

```
helm/
  ├─ dev          # local dev
  ├─ staging      # staging deploy
  └─ production   # prod deploy
```

Bind each worktree / CI environment to the right config:

```bash
doppler setup --scope . --project helm --config dev          # local
doppler setup --scope . --project helm --config staging      # staging branch
```

CI uses a service token (`DOPPLER_TOKEN` env var) instead of `doppler login`:

```yaml
# GitHub Actions
- run: doppler run -- npm run deploy
  env:
    DOPPLER_TOKEN: ${{ secrets.DOPPLER_PROD_TOKEN }}
```

The service token is scoped to one config (e.g. `production`), so even if it leaks it cannot read other environments.

## Adding a new secret

```bash
doppler secrets set CF_AIG_TOKEN --scope .
# prompts for value, never echoed
```

Or from a file:

```bash
doppler secrets set CF_AIG_TOKEN < /path/to/token-file --scope .
```

After setting in `dev`, promote to `staging` / `production` via the Doppler dashboard or:

```bash
doppler secrets download --scope . --no-file --format json | \
  doppler secrets upload --config staging
```

## Anti-patterns

- Committing `.env`, `.env.local`, `.dev.vars` with real values. Block the PR — these go in `.gitignore`.
- `export OPENAI_API_KEY=sk-...` in a shell rc file. Use Doppler.
- Hardcoding a secret in `wrangler.toml`, `Cargo.toml`, `package.json`, or any source file. Block the PR.
- `doppler run --project foo --config bar -- cmd` from a shared script. Use `--scope .` and rely on the bound config so different worktrees stay separated.
- Logging `process.env` or `doppler secrets download` output to stdout / a log file.
- Block any PR that adds a new `process.env.X` read without a corresponding `doppler secrets set X` documented in the PR or the project README.

---
name: wrangler-deploy
description: Deploy a Cloudflare Worker with wrangler, including secret bindings sourced from Doppler. Never commit secrets and never set them with raw values from a developer laptop.
roles: []
tags: [cloudflare, wrangler, deploy, secrets, doppler, ci]
---

# wrangler-deploy

This skill covers deploying a Worker with `wrangler deploy`, plus the boundary between **runtime bindings** (declared in `wrangler.toml`), **secret bindings** (set via `wrangler secret put`), and **Doppler-backed local env** (see the `doppler-secret-fetch` skill).

## When to use

- Shipping a new Worker for the first time.
- Adding a new secret a Worker reads from `env`.
- Wiring up the GitHub Actions deploy job.

## wrangler.toml — declare bindings, not values

```toml
name = "helm-api"
main = "src/index.ts"
compatibility_date = "2026-04-01"
compatibility_flags = ["nodejs_compat"]

[[d1_databases]]
binding = "DB"
database_name = "helm-prod"
database_id = "<uuid>"
migrations_dir = "db/migrations"

[[r2_buckets]]
binding = "ARTIFACTS"
bucket_name = "helm-artifacts"

[ai]
binding = "AI"

[observability]
enabled = true
```

Secrets do **not** appear here — only their names appear in code as `env.MY_SECRET`. The values are uploaded out-of-band.

## Setting a secret (one-time, then on rotation)

Never type a secret value into your shell. Pipe it from Doppler:

```bash
doppler secrets get CF_AIG_TOKEN --plain --scope . | \
  npx wrangler secret put CF_AIG_TOKEN
```

Verify:

```bash
npx wrangler secret list
```

Output should show `CF_AIG_TOKEN` (name only, never value).

## Local dev (`wrangler dev`)

For local dev, secrets come from Doppler — not from `.dev.vars`:

```bash
doppler run --scope . -- npx wrangler dev
```

Doppler injects `CF_AIG_TOKEN`, `CF_ACCOUNT_ID`, etc. into the process env, and wrangler exposes process env as bindings during `wrangler dev`.

If you must use `.dev.vars`, generate it from Doppler at session start and add it to `.gitignore`:

```bash
doppler secrets download --no-file --format env --scope . > .dev.vars
```

`.dev.vars` is gitignored. Never commit it.

## Deploying

```bash
npx wrangler deploy
```

For multi-environment:

```bash
npx wrangler deploy --env production
npx wrangler deploy --env staging
```

With `[env.production]` and `[env.staging]` blocks in `wrangler.toml`, each gets its own bindings. Secrets must be set per environment:

```bash
npx wrangler secret put CF_AIG_TOKEN --env production
```

## CI deploy (GitHub Actions)

```yaml
# .github/workflows/deploy.yml
name: deploy
on:
  push:
    branches: [main]

jobs:
  deploy:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-node@v4
        with:
          node-version: 20
      - run: npm ci
      - name: Apply D1 migrations
        run: npx wrangler d1 migrations apply helm-prod --remote
        env:
          CLOUDFLARE_API_TOKEN: ${{ secrets.CLOUDFLARE_API_TOKEN }}
          CLOUDFLARE_ACCOUNT_ID: ${{ secrets.CLOUDFLARE_ACCOUNT_ID }}
      - name: Deploy
        run: npx wrangler deploy
        env:
          CLOUDFLARE_API_TOKEN: ${{ secrets.CLOUDFLARE_API_TOKEN }}
          CLOUDFLARE_ACCOUNT_ID: ${{ secrets.CLOUDFLARE_ACCOUNT_ID }}
```

The `CLOUDFLARE_API_TOKEN` only needs `Workers Scripts:Edit` + `D1:Edit` for the target account — do not use a global token.

## Anti-patterns

- `wrangler secret put X` and pasting the value at the prompt. Always pipe from Doppler.
- Committing `.dev.vars`, `.env`, or any file containing a real secret value. Block the PR.
- Hardcoding `CLOUDFLARE_API_TOKEN` in `wrangler.toml` or any code. It belongs in CI secrets only.
- Deploying from a developer laptop to production. Production deploys go through CI on merge to `main`. Staging is fine to deploy locally for quick iteration.
- Block any PR that adds a new `env.X` read in Worker code without (a) a `wrangler secret put X` step in the deploy runbook or (b) a binding in `wrangler.toml`.

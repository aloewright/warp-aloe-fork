# Helm Cloudflare Control Plane

Phase C is intentionally split across three long-lived Workers:

- `helm-control-plane`: Access-gated API, onboarding checks, and manifest-backed resource registry.
- `helm-agent-runtime`: the only Worker allowed to coordinate runtime sessions and future Container lifecycle.
- `helm-cloudflare-mcp`: read-only Cloudflare inventory and audit tooling.

The root `helm.cloudflare.json` file is the local source of truth for environments, expected Worker names, routes, D1/R2/Durable Object resources, Container enablement, Access settings, and protected resources.

## Commands

```sh
npm install --prefix cloudflare-control-plane
npm run typecheck --prefix cloudflare-control-plane
npm test --prefix cloudflare-control-plane
npm run helm --prefix cloudflare-control-plane -- cloud check --env dev
npm run helm --prefix cloudflare-control-plane -- cloud audit --env dev
npm run helm --prefix cloudflare-control-plane -- cloud cleanup --env dev
npm run helm --prefix cloudflare-control-plane -- cloud deploy --worker helm-control-plane --env dev
```

`cloud cleanup` defaults to dry-run output. It deletes only unreferenced `helm-*` resources and refuses protected resources. Production deletion requires both `--production` and `--confirm <resource-name>`.

## Runtime Secrets

Set these on `helm-control-plane`:

```sh
wrangler secret put HELM_MANIFEST_JSON -c cloudflare-control-plane/wrangler.control-plane.toml --env dev
wrangler secret put CLOUDFLARE_API_TOKEN -c cloudflare-control-plane/wrangler.control-plane.toml --env dev
```

Set this on `helm-agent-runtime`:

```sh
wrangler secret put HELM_MANIFEST_JSON -c cloudflare-control-plane/wrangler.agent-runtime.toml --env dev
```

Repeat for `staging` and `production` after replacing placeholder values in `helm.cloudflare.json`.

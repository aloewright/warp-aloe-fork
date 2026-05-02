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

## Auth flow (PDX-23)

Auth is split into two JWTs:

- The **Cloudflare Access JWT** issued by your team's SSO. Verified via the JWKS at `https://<teamDomain>/cdn-cgi/access/certs`. JWKS responses are cached in `AUTH_KV` for 24h to avoid a fetch per request.
- The **helm session JWT** (HS256, 1h lifetime, claims: `sub`, `iat`, `exp`, `jti`, optional `scope`). Issued by `POST /api/auth/session` after a successful Access verification. Downstream Workers (agent-runtime, workflows) accept the helm JWT directly via `Authorization: Bearer <jwt>`. The signing key is the `HELM_JWT_SIGNING_KEY` wrangler secret.

Logout (`POST /api/auth/logout`) writes the bearer token's `jti` to the `AUTH_KV` denylist with TTL = remaining JWT lifetime so the entry self-evicts.

Local dev fallback: when the manifest's `access.required` is `false`, the control plane also accepts a Doppler service token via `X-Doppler-Token`. The token is validated against Doppler's `/v3/me` metadata endpoint; the helm JWT is issued with `scope = "doppler:<project>"` for audit attribution. We never read raw secret values (PDX-77 contract).

Create the KV namespace and set the signing-key secret before deploy:

```sh
wrangler kv:namespace create AUTH_KV
# Update `id` in wrangler.control-plane.toml under both [[kv_namespaces]] and per-env.
wrangler secret put HELM_JWT_SIGNING_KEY -c cloudflare-control-plane/wrangler.control-plane.toml --env dev
# Generate a strong key locally:
#   openssl rand -base64 64 | tr -d '\n'
```

`HELM_JWT_SIGNING_KEY` must also be set on `helm-agent-runtime` (with the *same* value) so it can verify the helm JWTs the control plane issues:

```sh
wrangler secret put HELM_JWT_SIGNING_KEY -c cloudflare-control-plane/wrangler.agent-runtime.toml --env dev
```

Every authenticated request writes a row to `audit_log` (action `http.request`, target_kind `endpoint`, details `{ method, path, status, durationMs, source }`). Session issuance writes `auth.session.issued` (or `auth.session.issued.doppler_fallback`); logout writes `auth.session.revoked`. Both target the JWT (`target_kind="jwt"`, `target_id=jti`).

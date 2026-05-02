# Helm D1 schema (PDX-22)

The control plane Worker persists relational state to Cloudflare D1. The
canonical source of truth for the schema is the Drizzle ORM definition at
[`src/db/schema.ts`](../src/db/schema.ts). The committed SQL migrations
under [`migrations/`](../migrations) are generated from that file by
`drizzle-kit` and applied to D1 via `wrangler d1 migrations apply`.

## Conceptual model

- `users` — principal identity. Email is unique. All workspaces, sessions,
  shares, sync events, and audit entries reference users.
- `workspaces` — owned by a user; group `resources`.
- `resources` — versioned JSON blobs addressed by `kind`. `version`
  increments on each update and is the basis for optimistic concurrency.
- `shares` — grant a `permission` (`read` / `write` / `admin`) on a
  resource to a user other than the owner. `(resource_id, shared_with_user_id)`
  is unique.
- `sync_events` — monotonic per-user log used for client-mirror replay
  and conflict surfacing. `(user_id, sequence)` is unique. Consumed by
  PDX-20 (Durable Objects) for real-time broadcast.
- `sessions` — long-lived conversation between a user and an agent.
- `tasks` — individual prompt/response units inside a session, with a
  status state machine (`queued → running → succeeded | failed | cancelled`)
  and an optional JSON `result`.
- `audit_log` — global append-only event stream.
  - `user_id` is **nullable** so the table can record both
    user-attributed actions (PDX-23: auth events) and system actions
    (PDX-28: guardrail trips fired by the runtime).
  - `(action, target_kind, target_id, details)` together describe the
    event. `details` is JSON.

## Working with migrations

### One-time D1 setup

```sh
# From the repo root
cd cloudflare-control-plane
npx wrangler d1 create helm
# Copy the printed `database_id` into wrangler.control-plane.toml,
# replacing the REPLACE_WITH_D1_ID placeholders (top-level + every env).
```

### Generate a new migration after schema changes

```sh
cd cloudflare-control-plane
npx drizzle-kit generate --name <short-description>
```

The new SQL file lands in `migrations/`. Commit both the changed
`schema.ts` and the generated SQL.

### Apply migrations

```sh
# Local (Miniflare-backed) D1
npx wrangler d1 migrations apply helm --local \
  -c wrangler.control-plane.toml

# Remote D1 (deployed Worker, per-environment)
npx wrangler d1 migrations apply helm --remote \
  -c wrangler.control-plane.toml --env dev
```

## Using the client from a Worker

```ts
import { getDb, users } from "../db/index.js";

export default {
  async fetch(_req: Request, env: Env): Promise<Response> {
    const db = getDb(env);
    const all = await db.select().from(users).all();
    return Response.json(all);
  }
};
```

`Env` for the control plane Worker must include the `DB: D1Database`
binding declared by `[[d1_databases]]` in `wrangler.control-plane.toml`.

## Downstream consumers

- **PDX-19 (Workers)** — imports row types (`User`, `Resource`, …) from
  `src/db/index.ts` to type request/response payloads.
- **PDX-20 (Durable Objects)** — `SessionDO` consumes the `sessions`
  and `tasks` row types and writes `sync_events` for real-time broadcast.
- **PDX-23 (auth attribution)** — writes user-attributed events to
  `audit_log` with `user_id` set, `action = "auth.signin" | "auth.signout"
  | "auth.refresh"`.
- **PDX-28 (guardrails)** — writes system events to `audit_log` with
  `user_id = NULL`, `action = "guardrail.trip"`, `target_kind` set to
  the firing subsystem, and `details` containing the policy + payload.

## Local-mirror story

The on-device `skill_usage_events` table in
`crates/persistence/src/schema.rs` (PDX-71) is the existing local
SQLite story. Its shape is preserved here by `tasks.result` (the JSON
payload that records token counts, model id, success, etc.) plus
`audit_log.details`. Later phases of PDX-22 will add a streaming mirror
from `sync_events` into the local DB; that work is gated on PDX-20 and
left as a TODO in this PR.

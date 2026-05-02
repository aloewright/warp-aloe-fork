---
name: drizzle-migration
description: Define a Drizzle schema, generate a migration, and apply it to a Cloudflare D1 database via wrangler. Always commit the generated SQL alongside the schema change.
roles: []
tags: [drizzle, d1, cloudflare, migrations, sql]
---

# drizzle-migration

This skill covers the end-to-end loop: change `schema.ts`, generate a SQL migration, apply it locally, apply it remotely, and ship the generated file in the same PR as the schema.

## When to use

- Adding, renaming, or dropping a table or column in a D1 database.
- Adding an index or foreign key.
- A reviewer asked "where's the migration?" on a schema change PR.

## Required files

```
db/
  schema.ts              # Drizzle schema — single source of truth
  migrations/
    0000_<slug>.sql      # generated, checked in
    meta/
      _journal.json      # generated, checked in
drizzle.config.ts        # tells drizzle-kit where things live
```

## drizzle.config.ts

```ts
import type { Config } from "drizzle-kit";

export default {
  schema: "./db/schema.ts",
  out: "./db/migrations",
  dialect: "sqlite",
  driver: "d1-http",
} satisfies Config;
```

## Defining a table

```ts
// db/schema.ts
import { sqliteTable, text, integer } from "drizzle-orm/sqlite-core";

export const skills = sqliteTable("skills", {
  id: text("id").primaryKey(),
  name: text("name").notNull(),
  description: text("description"),
  tokenBudget: integer("token_budget").notNull().default(0),
  createdAt: integer("created_at", { mode: "timestamp" }).notNull(),
});
```

## Generate the migration

```bash
npx drizzle-kit generate
```

This writes a new `0000N_<slug>.sql` plus updates `meta/_journal.json`. **Read the generated SQL before committing** — drizzle-kit cannot read your mind on renames and will sometimes emit `DROP COLUMN` + `ADD COLUMN` instead of a rename.

If a rename is wrong, hand-edit the generated SQL to use `ALTER TABLE ... RENAME COLUMN`. D1 supports it.

## Apply locally (Miniflare-backed D1)

```bash
npx wrangler d1 migrations apply <DB_NAME> --local
```

`<DB_NAME>` matches `database_name` in `wrangler.toml`:

```toml
[[d1_databases]]
binding = "DB"
database_name = "helm-prod"
database_id = "<uuid>"
migrations_dir = "db/migrations"
```

## Apply remotely

```bash
npx wrangler d1 migrations apply <DB_NAME> --remote
```

Run this from CI on merge to `main`, not from a developer laptop. See the `wrangler-deploy` skill for the GitHub Actions wiring.

## Commit checklist

- [ ] `db/schema.ts` reflects the desired final state.
- [ ] `db/migrations/000N_<slug>.sql` is checked in.
- [ ] `db/migrations/meta/_journal.json` is checked in.
- [ ] The generated SQL has been read and is correct (especially renames).
- [ ] Local apply succeeded against a fresh D1.
- [ ] PR description names the migration file by number, e.g. "Adds migration `0007_add_skills_token_budget.sql`".

## Anti-patterns

- Editing a previously-shipped migration. Once `0000N_*.sql` lands on `main`, it is immutable. Add `0000N+1_*.sql` instead.
- Running `wrangler d1 execute` with raw SQL to "fix" a schema drift. The next migration apply will fail. Always go through drizzle-kit.
- Committing schema changes without the generated SQL. Block the PR.
- Letting a destructive migration (`DROP TABLE`, `DROP COLUMN`) merge without a one-line note in the PR description explaining the data path.

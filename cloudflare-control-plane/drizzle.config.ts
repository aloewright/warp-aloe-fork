import { defineConfig } from "drizzle-kit";

/**
 * drizzle-kit configuration for the Helm D1 database (PDX-22).
 *
 * Generate migrations:
 *   npx drizzle-kit generate
 *
 * The generated SQL is committed under `migrations/` and applied to D1
 * via:
 *   wrangler d1 migrations apply helm --local        # local dev
 *   wrangler d1 migrations apply helm --remote       # deployed D1
 *
 * `dialect: "sqlite"` + `driver: "d1-http"` is the recommended pairing
 * for Cloudflare D1 schemas. `dbCredentials` is intentionally omitted
 * here because we use Wrangler (not drizzle-kit's HTTP push) to apply
 * migrations against real D1.
 */
export default defineConfig({
  dialect: "sqlite",
  driver: "d1-http",
  schema: "./src/db/schema.ts",
  out: "./migrations",
  verbose: true,
  strict: true
});

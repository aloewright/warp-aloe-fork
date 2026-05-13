## 2025-05-15 - Protected /api/audit/sync with authentication

**Vulnerability:** The `/api/audit/sync` endpoint was exposed without any authentication or authorization checks, allowing any unauthenticated client to push audit logs to the D1 database.

**Learning:** The Hono application in `cloudflare-control-plane` applies security middleware (`helmAuth` and `audit()`) based on explicit path prefixes. Routes defined without these middleware wrappers remain public even if they perform sensitive operations like database writes.

**Prevention:** Always ensure that new sensitive endpoints are explicitly wrapped with authentication and auditing middleware. Review the route definitions in `app.ts` whenever new surfaces are added to ensure they match the intended security posture of the application.

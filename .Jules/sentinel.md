## 2025-05-14 - Missing Authentication on Audit Sync Endpoint
**Vulnerability:** The `/api/audit/sync` endpoint was exposed publicly without authentication, despite being intended for internal audit log synchronization from symphony daemons.
**Learning:** In Hono applications with per-route middleware, newly added routes can be easily overlooked if they aren't explicitly included in a protected path prefix or have middleware applied directly. Comments describing an endpoint as "Authenticated" do not guarantee it is actually protected.
**Prevention:** Always verify that sensitive endpoints are either matched by a wildcard middleware (e.g., `app.use("/api/*", ... )`) or have explicit authentication middleware applied. Add automated tests to verify 401 responses for all protected routes.

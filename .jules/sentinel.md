## 2025-05-22 - Missing Authentication on Audit Sync Endpoint
**Vulnerability:** The `/api/audit/sync` endpoint was completely unprotected, allowing any unauthenticated client to POST arbitrary audit log batches to the system.
**Learning:** In a large Hono application, routes defined outside of explicit `app.use` middleware matchers or route-specific middleware remain public. The `/api/audit/sync` route was added but not included in the list of protected routes.
**Prevention:** Always use a default-deny approach for authentication middleware if possible, or maintain a centralized list of protected prefixes and verify new routes are covered by it.

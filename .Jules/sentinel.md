## 2025-05-16 - Unauthenticated Audit Ingestion & DoS Risk
**Vulnerability:** The `/api/audit/sync` endpoint in `cloudflare-control-plane` was completely public and lacked any payload size limits. This allowed unauthenticated attackers to inject arbitrary data into the `audit_log` table and potentially perform a DoS attack by sending massive batches.
**Learning:** Sensitive routes defined outside the explicit `app.use` middleware matchers in Hono are public by default. The `audit` middleware for request attribution depends on `helmAuth` to populate the auth context; if applied alone or if the path is not matched by `helmAuth`, it defaults to anonymous logging but does not block access.
**Prevention:** Always apply `helmAuth` and `audit()` middleware to new endpoints. Use explicit path prefixes for protected subtrees and verify middleware coverage in tests. Enforce batch size limits on all ingestion endpoints.

## 2025-05-17 - Blocking Audit Latency & Error Leakage
**Vulnerability:** The `audit()` middleware was `await`ing D1 insertions, adding ~5-50ms of latency to every authenticated request. Additionally, it was echoing raw `Error.message` strings back to the user on failure, potentially leaking D1 or Drizzle internals.
**Learning:** Middleware that performs side-channel logging should use `c.executionCtx.waitUntil()` to unblock the response. Error handling in shared middleware must explicitly sanitize messages to maintain the "fail securely" principle.
**Prevention:** Always use `waitUntil` for non-critical side-effects. Never echo raw error objects or messages in production middleware; use generic messages and log the details internally.

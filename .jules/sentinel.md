## 2025-05-21 - Unprotected Audit Log Sync Endpoint
**Vulnerability:** The `/api/audit/sync` endpoint was exposed without authentication, allowing any caller to inject arbitrary audit logs into the database.
**Learning:** In the Hono-based control plane, middleware like `helmAuth` and `audit()` are applied to specific path prefixes. Endpoints defined outside these prefixes (or added later) may remain unprotected if the prefix list is not updated.
**Prevention:** Maintain a strict "fail-closed" routing policy where all sensitive prefixes are explicitly protected. Verify middleware coverage for all new routes during security review.

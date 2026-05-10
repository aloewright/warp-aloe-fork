## 2025-05-15 - Unauthenticated Audit Sync Endpoint
**Vulnerability:** The `/api/audit/sync` endpoint in the Cloudflare control plane was exposed without authentication, allowing any client to insert arbitrary audit logs into the `audit_log` D1 database.
**Learning:** Security-critical endpoints that synchronize data between local and cloud environments can be easily overlooked if they are introduced as standalone router fragments or during rapid prototyping.
**Prevention:** Always ensure that new routes are added to the centralized middleware stack (e.g., `helmAuth`, `audit`) in `app.ts` as part of the initial implementation.

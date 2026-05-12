## 2026-05-12 - Memoization of manifest parsing in control-plane
**Learning:** In Cloudflare Workers, environment variables are static for the life of an isolate. Parsing and validating large JSON strings (like `HELM_MANIFEST_JSON`) on every request is an O(N) operation that can be safely memoized at the module level.
**Action:** Always look for static configuration parsing or expensive validations that can be moved outside the request handler or memoized to improve per-request latency.

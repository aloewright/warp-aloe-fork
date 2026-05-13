## 2025-05-15 - Memoize Environment Manifest Parsing
**Learning:** Redundant parsing and Zod validation of large environment configuration (HELM_MANIFEST_JSON) in middleware can add measurable latency (up to 50ms for 1000 requests in benchmarks). Since environment variables are static within a Worker isolate, module-level memoization effectively eliminates this overhead.
**Action:** Always memoize expensive configuration parsing or validation logic that depends on static environment variables in Cloudflare Workers.

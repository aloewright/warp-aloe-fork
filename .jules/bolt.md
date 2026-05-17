## 2025-05-15 - [Memoize Drizzle client instantiation]
**Learning:** In the Cloudflare Worker control plane, `getDb` was instantiating a new Drizzle client on every call. While `drizzle()` is relatively fast, it still involves schema parsing and validation. By memoizing the client per `D1Database` instance using a `WeakMap`, we reduced the `getDb` overhead from ~0.053ms to ~0.0001ms.
**Action:** Always memoize database and manifest parsing results at the module level in Cloudflare Workers to maximize performance in warm isolates.

## 2025-05-16 - [Optimize audit logging to be non-blocking]
**Learning:** The `audit` middleware and other attribution helpers in the control plane were `await`ing D1 database insertions. In Cloudflare Workers, these I/O-bound tasks can be moved out of the critical request path using `c.executionCtx.waitUntil()`, allowing the Worker to return a response to the user immediately while the log is written in the background. This significantly reduces TTFB for all protected routes.
**Action:** Use `waitUntil` for non-critical side effects like audit logging or analytics to minimize response latency in Workers.

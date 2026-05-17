## 2025-05-15 - [Memoize Drizzle client instantiation]
**Learning:** In the Cloudflare Worker control plane, `getDb` was instantiating a new Drizzle client on every call. While `drizzle()` is relatively fast, it still involves schema parsing and validation. By memoizing the client per `D1Database` instance using a `WeakMap`, we reduced the `getDb` overhead from ~0.053ms to ~0.0001ms.
**Action:** Always memoize database and manifest parsing results at the module level in Cloudflare Workers to maximize performance in warm isolates.

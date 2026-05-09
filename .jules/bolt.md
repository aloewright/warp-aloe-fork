## 2025-12-11 - Matcher Reuse and Fast Paths in Fuzzy Matching

**Learning:** `SkimMatcherV2` is relatively expensive to instantiate in tight loops. Reusing it via `thread_local!` significantly reduces initialization overhead. Additionally, query preprocessing (like whitespace removal) should always check if the transformation is necessary before allocating new strings, as many queries will already be in the desired format.

**Action:** Look for high-frequency search functions and ensure they reuse heavy matcher objects and avoid unnecessary heap allocations in the common fast path.

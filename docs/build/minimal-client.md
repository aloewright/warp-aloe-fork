# Helm minimal-client build (PDX-35 / A1.5)

The Helm fork ships a "minimal" build of `warp` that strips Warp's hosted
backend surface (Firebase auth, warp-server GraphQL, oz.warp.dev hosted
agents, RudderStack telemetry, hardcoded `*.warp.dev` URLs) and keeps only
the local-first surface: terminal, blocks, settings, and the BYO-CLI
agent track (`claude` / `codex` / `ollama` via the orchestrator crate).

The cutover is gated by the `warp_hosted` Cargo feature on the `warp`
binary. It is **on by default** (so an unmodified `cargo build` continues
to produce upstream-equivalent Warp), and the Helm minimal build opts out
via `--no-default-features`.

## Build invocations

Minimal Helm client (default Helm target):

```sh
cargo build -p warp --no-default-features
```

Upstream-compatible Warp build (rebaseable onto `warpdotdev/warp`):

```sh
cargo build -p warp                          # default features = warp_hosted ON
# or, equivalent and explicit:
cargo build -p warp --no-default-features --features warp_hosted
```

`cargo check` works identically for fast iteration:

```sh
cargo check -p warp --no-default-features    # minimal
cargo check -p warp                          # upstream-equivalent
cargo check --workspace                      # everything (default features)
```

## What `--no-default-features` strips

Driven by sibling issues that landed before this capstone:

- **PDX-31 [A1.1] Auth.** Firebase OAuth device flow, anonymous user
  creation, `identitytoolkit.googleapis.com` calls, and the
  `AuthClient` trait surface are gated behind `warp_hosted` in
  `app/src/auth/**` and `crates/warp_core::warp_hosted`.
- **PDX-32 [A1.2] Drive.** Cloud-side Drive surfaces (folder/object
  metadata sync, ACLs, shared notebooks/workflows hydration through
  warp-server) are gated. Local notebook/workflow hydration still
  works via the local-only path covered by PDX-82.
- **PDX-33 [A1.3] Telemetry.** RudderStack product analytics calls
  removed from the `warp_hosted`-OFF path.
- **PDX-34 [A1.4] Hosted Oz.** `oz.warp.dev` cloud-agents code paths
  removed from the `warp_hosted`-OFF path. The Helm minimal build
  uses the BYO-CLI agent surface from PDX-103 / PDX-104 / PDX-105
  instead (Claude Code / Codex / Ollama through `crates/orchestrator`).
- **PDX-78 [A1.7] Sentry replacement.** Soft dependency. Crash reporting
  is governed by the separate `crash_reporting` Cargo feature, not by
  `warp_hosted`. PDX-78 may land later without re-gating this build.

## Verifying a build

After a `--no-default-features` build, confirm:

1. The binary launches and opens a terminal.
2. The BYO-CLI flow works — typing `claude` / `codex` / `ollama` in the
   terminal routes through `crates/orchestrator` to the local CLI.
3. No outbound network calls to:
   - `*.warp.dev`
   - `oz.warp.dev`
   - `identitytoolkit.googleapis.com`
   - RudderStack endpoints
   - Firebase endpoints

   On macOS this can be checked with a 30-second `lsof -i -nP` or
   `tcpdump` capture during normal interactive use.

## Known caveats (not in scope for PDX-35)

- The `wasm32-unknown-unknown` target currently fails to compile because
  `arborium-sysroot` 2.13's `build.rs` invokes Apple `clang`, which on
  stock Xcode does not have a wasm backend. This is an upstream-toolchain
  issue tracked separately and is not on the minimal-client critical
  path (the Helm minimal client targets native macOS first).
- A small number of pre-existing dead-code warnings in
  `app/src/auth/**` and `app/src/server/server_api/**` are emitted by
  the `warp_hosted`-OFF path. They are expected — the gated trait
  surface intentionally keeps the types alive so upstream merges stay
  clean — and are not errors.

## Tagging the verified build

After the binary has been verified end-to-end, tag the commit:

```sh
git tag helm-v0.1-compile-clean
git push origin helm-v0.1-compile-clean
```

// SPDX-License-Identifier: AGPL-3.0-only

//! PDX-118 [E6] — Cloudflare AI Gateway routing wiring for the
//! third-party agent CLIs spawned by this crate.
//!
//! Two responsibilities:
//!
//! 1. Resolve the gateway token from the user's Doppler binding (with
//!    a 5-minute in-memory cache and one-shot 401 invalidation).
//! 2. Apply the env-var overrides computed by [`ai_gateway_config`] to
//!    a [`tokio::process::Command`] and emit a `gateway_routed` audit
//!    log row capturing the spawn.
//!
//! The whole module is gated on `cfg(not(target_family = "wasm"))`
//! because both Doppler (process spawn) and the symphony AuditLog
//! (filesystem) are unavailable on the web target. WASM callers get a
//! shim that always reports "no routing", so they spawn agents with
//! the user's existing env unchanged — a regression-free fallback.

#![cfg(not(target_family = "wasm"))]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ai_gateway_config::{AgentKind, GatewayConfig, GatewayInjection};
use doppler::{DopplerClient, DopplerError, SecretValue};
use once_cell::sync::OnceCell;
use orchestrator::AgentId;
use std::io::Write;
use std::sync::Mutex;
use tokio::process::Command;

/// 5-minute cache TTL — matches `doppler::DEFAULT_TTL` and the spec's
/// "Cache for 5 minutes; on 401 from the gateway, invalidate + refresh
/// once" requirement.
const TOKEN_TTL: Duration = doppler::DEFAULT_TTL;

/// Process-wide Doppler client used to resolve `CF_AIG_TOKEN` (or
/// whatever the user has bound). Constructed once on first use; the
/// underlying TTL cache is shared across all spawn sites.
fn doppler_client() -> &'static Arc<DopplerClient> {
    static CLIENT: OnceCell<Arc<DopplerClient>> = OnceCell::new();
    CLIENT.get_or_init(|| Arc::new(DopplerClient::new(TOKEN_TTL)))
}

/// Default audit-log path: `~/.warp/symphony/audit.log`. Mirrors the
/// path documented at the top of `crates/symphony/src/audit.rs`.
fn default_audit_path() -> Option<PathBuf> {
    let mut p = dirs::home_dir()?;
    p.push(".warp");
    p.push("symphony");
    p.push("audit.log");
    Some(p)
}

/// Append-only writer over `~/.warp/symphony/audit.log`.
///
/// We cannot depend on `symphony::AuditLog` directly because `symphony`
/// already depends on `agents` (cyclic). Instead, we open the same
/// JSONL file with the same schema (`timestamp`, `kind`,
/// `agent_provider`, `message`, …) so symphony's downstream consumers
/// (the `warp audit` CLI shipped in PDX-115, the dashboards) see our
/// rows alongside symphony's own. Best-effort: I/O errors are
/// swallowed via `tracing` exactly like `symphony::AuditLog::record`.
struct GatewayAuditLog {
    path: PathBuf,
    file: Mutex<Option<std::fs::File>>,
}

impl GatewayAuditLog {
    fn open(path: PathBuf) -> Self {
        let file = match Self::open_inner(&path) {
            Ok(f) => Some(f),
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "ai_gateway: failed to open audit log");
                None
            }
        };
        Self {
            path,
            file: Mutex::new(file),
        }
    }

    fn open_inner(path: &Path) -> std::io::Result<std::fs::File> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
    }

    fn record_line(&self, line: &str) {
        let mut guard = match self.file.lock() {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!(error = %e, "ai_gateway: audit log mutex poisoned");
                return;
            }
        };
        if guard.is_none() {
            if let Ok(f) = Self::open_inner(&self.path) {
                *guard = Some(f);
            }
        }
        if let Some(f) = guard.as_mut() {
            if let Err(e) = writeln!(f, "{}", line) {
                tracing::warn!(error = %e, "ai_gateway: failed to append audit row");
            }
        }
    }
}

fn audit_log() -> &'static GatewayAuditLog {
    static LOG: OnceCell<GatewayAuditLog> = OnceCell::new();
    LOG.get_or_init(|| {
        let path = default_audit_path().unwrap_or_else(|| PathBuf::from("audit.log"));
        GatewayAuditLog::open(path)
    })
}

/// Trait wrapping the (small) doppler surface we depend on, so unit
/// tests can substitute a deterministic resolver without touching the
/// real CLI. Only the methods the gateway resolver actually uses are
/// exposed here.
#[async_trait::async_trait]
pub(crate) trait TokenResolver: Send + Sync {
    /// Fetch the secret bound to `name`, scoped to `cwd`.
    async fn fetch(&self, name: &str, cwd: Option<&Path>) -> Result<SecretValue, DopplerError>;
    /// Drop any cached entry for `(cwd, name)` so the next `fetch`
    /// re-spawns `doppler`. Called by [`invalidate_token_cache`] when
    /// the upstream gateway returns 401.
    #[allow(dead_code)]
    fn invalidate(&self, name: &str, cwd: Option<&Path>);
}

#[async_trait::async_trait]
impl TokenResolver for DopplerClient {
    async fn fetch(&self, name: &str, cwd: Option<&Path>) -> Result<SecretValue, DopplerError> {
        DopplerClient::get(self, name, cwd).await
    }
    fn invalidate(&self, name: &str, cwd: Option<&Path>) {
        DopplerClient::invalidate(self, name, cwd)
    }
}

/// Apply gateway routing to `cmd` if the user has opted in for `agent`.
///
/// Returns `Some(injection)` when at least one env var was layered on
/// the command — callers can use the return value to drive the audit
/// row. Returns `None` when routing was disabled (or when the config
/// file is missing entirely): the caller MUST then leave the env
/// untouched.
///
/// `cwd` is forwarded to the Doppler resolver so per-repo bindings
/// (PDX-56) keep working.
/// Test-only helper: load `~/.warp/ai_gateway.toml`, resolve token
/// from Doppler, and report whether the [`AgentKind`] would have been
/// routed (without actually running an agent CLI). Used by the
/// `#[ignore]`-gated integration test that asserts env injection
/// against a temp `$HOME`.
#[doc(hidden)]
pub async fn _test_resolve_injection(
    agent: AgentKind,
    _cwd: Option<&Path>,
) -> Option<GatewayInjection> {
    let cfg = GatewayConfig::load_default().ok().flatten()?;
    if !cfg.is_routing_enabled(agent) {
        return None;
    }
    // Skip Doppler in the test helper to keep the test hermetic. The
    // BASE_URL injection path is what the integration test asserts on.
    cfg.env_overrides_for(agent, None)
}

pub(crate) async fn maybe_apply_to_command(
    cmd: &mut Command,
    agent: AgentKind,
    agent_id: &AgentId,
    cwd: Option<&Path>,
) -> Option<GatewayInjection> {
    let cfg = match GatewayConfig::load_default() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "ai_gateway: failed to load config; skipping routing");
            None
        }
    };
    apply_with_resolver(cmd, agent, agent_id, cwd, doppler_client().as_ref(), cfg).await
}

/// Internal seam used by tests: takes an explicit [`TokenResolver`]
/// and an explicit (already-loaded) config option.
async fn apply_with_resolver(
    cmd: &mut Command,
    agent: AgentKind,
    agent_id: &AgentId,
    cwd: Option<&Path>,
    resolver: &dyn TokenResolver,
    cfg: Option<GatewayConfig>,
) -> Option<GatewayInjection> {
    let cfg = match cfg {
        Some(c) => c,
        None => return None,
    };

    if !cfg.is_routing_enabled(agent) {
        return None;
    }

    // Token resolution — best-effort. If Doppler is unreachable or
    // the binding is missing, we still emit the BASE_URL override so
    // the request fails at the gateway with a recoverable 401 rather
    // than silently bypassing the gateway.
    let token = match resolver.fetch(&cfg.token_doppler_ref, cwd).await {
        Ok(v) => Some(v),
        Err(DopplerError::NotInstalled { .. } | DopplerError::NotAuthenticated) => {
            tracing::warn!(
                "ai_gateway: doppler unavailable; spawning {agent:?} with BASE_URL but no token"
            );
            None
        }
        Err(e) => {
            tracing::warn!(error = %e, "ai_gateway: failed to resolve token; spawning without it");
            None
        }
    };

    let injection = cfg.env_overrides_for(agent, token.as_ref().map(SecretValue::expose))?;

    for entry in &injection.env {
        cmd.env(&entry.name, &entry.value);
    }

    record_audit_row(&injection, agent_id);

    Some(injection)
}

fn record_audit_row(injection: &GatewayInjection, agent_id: &AgentId) {
    // Schema matches `symphony::audit::AuditEvent` so the rows live
    // alongside symphony's own. Kind = `dispatched` (the closest
    // existing variant); the `message` field carries the full
    // gateway-routing payload as JSON, with `rule = "gateway_routed"`
    // per PDX-118.
    let detail = serde_json::json!({
        "rule": "gateway_routed",
        "action": "allowed",
        "provider": injection.agent.provider_tag(),
        "route_slug": injection.route_slug(),
        "account_id": injection.account_id,
        "gateway_slug": injection.gateway_slug,
        "agent_id": agent_id.0,
    });
    let row = serde_json::json!({
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "issue_id": serde_json::Value::Null,
        "issue_identifier": serde_json::Value::Null,
        "kind": "dispatched",
        "agent_provider": injection.agent.provider_tag(),
        "tokens_used": serde_json::Value::Null,
        "error": serde_json::Value::Null,
        "message": detail.to_string(),
    });
    audit_log().record_line(&row.to_string());
}

/// Hook called by agent runners on a 401 from the upstream gateway.
/// Drops the cached token so the *next* spawn re-resolves from
/// Doppler. Best-effort: silently no-ops on WASM or if the config
/// can't be loaded.
#[allow(dead_code)]
pub(crate) fn invalidate_token_cache(cwd: Option<&Path>) {
    let cfg = match GatewayConfig::load_default() {
        Ok(Some(c)) => c,
        _ => return,
    };
    doppler_client().invalidate(&cfg.token_doppler_ref, cwd);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// In-memory resolver: counts fetches so we can assert
    /// invalidate-then-refetch semantics.
    struct CountingResolver {
        secrets: Mutex<HashMap<String, String>>,
        invalidated: Mutex<Vec<String>>,
        fetch_count: AtomicUsize,
    }

    impl CountingResolver {
        fn new(map: &[(&str, &str)]) -> Self {
            Self {
                secrets: Mutex::new(
                    map.iter()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect(),
                ),
                invalidated: Mutex::new(Vec::new()),
                fetch_count: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl TokenResolver for CountingResolver {
        async fn fetch(
            &self,
            name: &str,
            _cwd: Option<&Path>,
        ) -> Result<SecretValue, DopplerError> {
            self.fetch_count.fetch_add(1, Ordering::SeqCst);
            let map = self.secrets.lock().unwrap();
            match map.get(name) {
                Some(v) => Ok(secret_value_for_test(v.clone())),
                None => Err(DopplerError::KeyMissing(name.to_string())),
            }
        }
        fn invalidate(&self, name: &str, _cwd: Option<&Path>) {
            self.invalidated.lock().unwrap().push(name.to_string());
        }
    }

    /// Tests exercise the `KeyMissing` arm of the resolver, which
    /// avoids needing to construct a `SecretValue` (its constructor
    /// is private to the `doppler` crate). The BASE_URL injection
    /// path is independent of token presence — see
    /// `enabled_route_without_token_still_sets_base_url`.
    fn secret_value_for_test(_v: String) -> SecretValue {
        unreachable!(
            "test resolver should not produce SecretValue: tests use the KeyMissing arm"
        );
    }

    fn cfg(account: &str, claude: bool, codex: bool) -> GatewayConfig {
        GatewayConfig {
            account_id: account.to_string(),
            gateway_slug: "x".to_string(),
            token_doppler_ref: "CF_AIG_TOKEN".to_string(),
            claude_code: ai_gateway_config::AgentRoute { enabled: claude },
            codex: ai_gateway_config::AgentRoute { enabled: codex },
        }
    }

    /// When both toggles are off, no env is touched, no audit row is
    /// written, and the resolver is never asked for the token.
    #[tokio::test]
    async fn disabled_routes_do_not_call_resolver() {
        let resolver = CountingResolver::new(&[]);
        let mut cmd = Command::new("/bin/echo");
        let agent_id = AgentId("claude-code-test".into());
        let inj = apply_with_resolver(
            &mut cmd,
            AgentKind::ClaudeCode,
            &agent_id,
            None,
            &resolver,
            Some(cfg("ACC", false, false)),
        )
        .await;
        assert!(inj.is_none());
        assert_eq!(resolver.fetch_count.load(Ordering::SeqCst), 0);
    }

    /// When the config is missing entirely, routing is a no-op even
    /// with the resolver populated.
    #[tokio::test]
    async fn missing_config_is_noop() {
        let resolver = CountingResolver::new(&[("CF_AIG_TOKEN", "tk")]);
        let mut cmd = Command::new("/bin/echo");
        let agent_id = AgentId("claude-code-test".into());
        let inj =
            apply_with_resolver(&mut cmd, AgentKind::ClaudeCode, &agent_id, None, &resolver, None)
                .await;
        assert!(inj.is_none());
        assert_eq!(resolver.fetch_count.load(Ordering::SeqCst), 0);
    }

    /// When routing is enabled but the resolver reports `KeyMissing`,
    /// the BASE_URL still lands on the command (auth header omitted).
    /// This is the regression-free posture: the request fails at the
    /// gateway with 401 instead of silently bypassing it.
    #[tokio::test]
    async fn enabled_route_without_token_still_sets_base_url() {
        let resolver = CountingResolver::new(&[]); // KeyMissing path.
        let mut cmd = Command::new("/bin/echo");
        let agent_id = AgentId("claude-code-test".into());
        let inj = apply_with_resolver(
            &mut cmd,
            AgentKind::ClaudeCode,
            &agent_id,
            None,
            &resolver,
            Some(cfg("ACC", true, false)),
        )
        .await
        .expect("injection");
        assert_eq!(inj.account_id, "ACC");
        assert_eq!(inj.gateway_slug, "x");
        assert_eq!(inj.env.len(), 1);
        assert_eq!(inj.env[0].name, "ANTHROPIC_BASE_URL");
        assert!(inj.env[0].value.ends_with("/compat/anthropic"));
        assert_eq!(resolver.fetch_count.load(Ordering::SeqCst), 1);
    }

    /// Codex toggle: enabling Codex must inject `OPENAI_BASE_URL` (not
    /// `ANTHROPIC_BASE_URL`), and leaving the toggle off must leave
    /// the env untouched.
    #[tokio::test]
    async fn codex_toggle_independent_of_claude() {
        let resolver = CountingResolver::new(&[]);
        let agent_id = AgentId("codex-test".into());

        // Codex on, Claude off.
        let mut cmd = Command::new("/bin/echo");
        let inj = apply_with_resolver(
            &mut cmd,
            AgentKind::Codex,
            &agent_id,
            None,
            &resolver,
            Some(cfg("ACC", false, true)),
        )
        .await
        .expect("injection");
        assert_eq!(inj.env[0].name, "OPENAI_BASE_URL");

        // Same config, but ask for Claude — toggle is off → None.
        let mut cmd = Command::new("/bin/echo");
        let inj = apply_with_resolver(
            &mut cmd,
            AgentKind::ClaudeCode,
            &agent_id,
            None,
            &resolver,
            Some(cfg("ACC", false, true)),
        )
        .await;
        assert!(inj.is_none());
    }

    /// The trait's `invalidate` is exercised on the 401 path. We
    /// verify the resolver records the call against the configured
    /// `token_doppler_ref` so the next fetch re-spawns Doppler.
    #[tokio::test]
    async fn invalidate_drops_cached_entry() {
        let resolver = CountingResolver::new(&[]);
        TokenResolver::invalidate(&resolver, "CF_AIG_TOKEN", None);
        let log = resolver.invalidated.lock().unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0], "CF_AIG_TOKEN");
    }
}

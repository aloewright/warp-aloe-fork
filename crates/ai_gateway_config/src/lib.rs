// SPDX-License-Identifier: AGPL-3.0-only

//! PDX-118 [E6] — Cloudflare AI Gateway routing config for third-party
//! agent CLIs.
//!
//! This crate is intentionally narrow: it loads/saves
//! `~/.warp/ai_gateway.toml`, exposes a typed view of its contents, and
//! builds the env-var injections for `claude` / `codex` subprocesses.
//! Token resolution against Doppler lives in the agent runner; this
//! crate only carries the *reference* (e.g. `CF_AIG_TOKEN`) so secrets
//! never hit disk via this config.
//!
//! The config is opt-in. When the file is missing or both per-agent
//! toggles are off, callers must spawn agents with the user's existing
//! env unchanged — see [`GatewayConfig::env_overrides_for`].

#![deny(missing_docs)]

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Default Cloudflare gateway slug as documented in CLAUDE.md.
pub const DEFAULT_GATEWAY_SLUG: &str = "x";

/// Default Doppler secret name to resolve into `CF_AIG_TOKEN`.
pub const DEFAULT_TOKEN_DOPPLER_REF: &str = "CF_AIG_TOKEN";

/// Identifies a third-party agent CLI for the purposes of gateway routing.
///
/// Ollama is intentionally absent: it is local-only, so the gateway
/// never applies. Foundation Models likewise runs in-process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    /// The `claude` CLI (Anthropic-compatible base URL routing).
    ClaudeCode,
    /// The `codex` CLI (OpenAI-compatible base URL routing).
    Codex,
}

impl AgentKind {
    /// Stable provider tag used in audit-log messages.
    pub fn provider_tag(self) -> &'static str {
        match self {
            AgentKind::ClaudeCode => "claude_code",
            AgentKind::Codex => "codex",
        }
    }
}

/// Per-agent toggle stored in the TOML file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRoute {
    /// When true, the runner injects gateway env vars before spawn.
    #[serde(default)]
    pub enabled: bool,
}

/// Top-level deserialised view of `~/.warp/ai_gateway.toml`.
///
/// All fields are optional in the serialised form so a partially-populated
/// file does not error out: missing values fall back to [`Default`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayConfig {
    /// Cloudflare account id (the path segment after `/v1/` in the
    /// gateway URL). Empty until the user fills it in.
    #[serde(default)]
    pub account_id: String,
    /// Gateway slug — the path segment after the account id. Defaults to
    /// `"x"` per CLAUDE.md.
    #[serde(default = "default_gateway_slug")]
    pub gateway_slug: String,
    /// Doppler reference whose value resolves to `CF_AIG_TOKEN`.
    /// Defaults to `"CF_AIG_TOKEN"` so a single Doppler binding works
    /// out of the box.
    #[serde(default = "default_token_doppler_ref")]
    pub token_doppler_ref: String,
    /// Per-agent toggle for the Claude Code CLI.
    #[serde(default)]
    pub claude_code: AgentRoute,
    /// Per-agent toggle for the Codex CLI.
    #[serde(default)]
    pub codex: AgentRoute,
}

fn default_gateway_slug() -> String {
    DEFAULT_GATEWAY_SLUG.to_string()
}

fn default_token_doppler_ref() -> String {
    DEFAULT_TOKEN_DOPPLER_REF.to_string()
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            account_id: String::new(),
            gateway_slug: default_gateway_slug(),
            token_doppler_ref: default_token_doppler_ref(),
            claude_code: AgentRoute::default(),
            codex: AgentRoute::default(),
        }
    }
}

/// Errors produced by [`GatewayConfig::load`] / [`GatewayConfig::save`].
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The home directory could not be resolved (no `$HOME`).
    #[error("could not resolve home directory")]
    NoHome,
    /// Filesystem I/O failure.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// TOML deserialisation failure.
    #[error("invalid toml: {0}")]
    Parse(#[from] toml::de::Error),
    /// TOML serialisation failure.
    #[error("toml serialise: {0}")]
    Serialise(#[from] toml::ser::Error),
}

/// One injected `(name, value)` pair to layer onto a child process env.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvOverride {
    /// Env var name to set.
    pub name: String,
    /// Env var value.
    pub value: String,
}

/// Materialised view of the env-var changes a runner should apply for
/// one agent kind. `None` means "do nothing — leave the existing env
/// alone." This is the regression-free escape hatch when the user
/// either hasn't created the config or has explicitly toggled the
/// agent off.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayInjection {
    /// The agent this injection applies to.
    pub agent: AgentKind,
    /// Cloudflare account id (carried for audit-log payloads).
    pub account_id: String,
    /// Gateway slug (carried for audit-log payloads).
    pub gateway_slug: String,
    /// Pairs to set on the child env. Token-bearing entries have the
    /// resolved secret value already substituted in.
    pub env: Vec<EnvOverride>,
}

impl GatewayInjection {
    /// Stable string used by callers to populate audit log payloads.
    pub fn route_slug(&self) -> &'static str {
        match self.agent {
            // Both claude and codex go via `dynamic/text_gen` for chat
            // completions; the per-provider compat suffix in the URL
            // does the heavy lifting.
            AgentKind::ClaudeCode | AgentKind::Codex => "dynamic/text_gen",
        }
    }
}

impl GatewayConfig {
    /// Resolve `~/.warp/ai_gateway.toml`.
    pub fn default_path() -> Result<PathBuf, ConfigError> {
        let mut p = dirs::home_dir().ok_or(ConfigError::NoHome)?;
        p.push(".warp");
        p.push("ai_gateway.toml");
        Ok(p)
    }

    /// Load the config from `~/.warp/ai_gateway.toml`, or return
    /// `Ok(None)` if the file does not exist. Any other error
    /// (permission denied, invalid TOML, …) surfaces as `Err`.
    pub fn load_default() -> Result<Option<Self>, ConfigError> {
        let path = Self::default_path()?;
        Self::load_from(&path)
    }

    /// Load the config from `path`. Returns `Ok(None)` if the file does
    /// not exist; surfaces parse errors as `Err`.
    pub fn load_from(path: &Path) -> Result<Option<Self>, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(s) => Ok(Some(toml::from_str::<Self>(&s)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Save the config to `~/.warp/ai_gateway.toml`. Creates the parent
    /// directory if it does not exist. Best-effort `0o600` permissions
    /// on Unix so the (non-secret, but still per-user) account id and
    /// gateway slug do not leak across users on a shared host.
    pub fn save_default(&self) -> Result<(), ConfigError> {
        let path = Self::default_path()?;
        self.save_to(&path)
    }

    /// Save the config to `path`. See [`save_default`](Self::save_default).
    pub fn save_to(&self, path: &Path) -> Result<(), ConfigError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let s = toml::to_string_pretty(self)?;
        std::fs::write(path, s)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    /// Returns the per-agent toggle.
    pub fn route_for(&self, agent: AgentKind) -> &AgentRoute {
        match agent {
            AgentKind::ClaudeCode => &self.claude_code,
            AgentKind::Codex => &self.codex,
        }
    }

    /// True iff the agent is configured to be routed AND `account_id`
    /// is non-empty (so we never emit a half-formed URL).
    pub fn is_routing_enabled(&self, agent: AgentKind) -> bool {
        self.route_for(agent).enabled && !self.account_id.trim().is_empty()
    }

    /// Build the Anthropic-compat base URL.
    pub fn anthropic_base_url(&self) -> String {
        format!(
            "https://gateway.ai.cloudflare.com/v1/{}/{}/compat/anthropic",
            self.account_id, self.gateway_slug
        )
    }

    /// Build the OpenAI-compat base URL.
    pub fn openai_base_url(&self) -> String {
        format!(
            "https://gateway.ai.cloudflare.com/v1/{}/{}/compat",
            self.account_id, self.gateway_slug
        )
    }

    /// Compute the full env-var override set for `agent`, given the
    /// resolved gateway token. Returns `None` when routing is disabled
    /// for `agent` — the caller MUST then spawn with the existing env
    /// untouched.
    ///
    /// `token` is the secret value resolved from Doppler (or wherever
    /// the caller chooses). When routing is enabled but `token` is
    /// `None`, this still returns `Some(_)` with `*_BASE_URL` set but
    /// no auth header — a regression-free posture: the request will
    /// fail at the gateway with 401 and the caller can react.
    pub fn env_overrides_for(
        &self,
        agent: AgentKind,
        token: Option<&str>,
    ) -> Option<GatewayInjection> {
        if !self.is_routing_enabled(agent) {
            return None;
        }
        let mut env = Vec::with_capacity(2);
        match agent {
            AgentKind::ClaudeCode => {
                env.push(EnvOverride {
                    name: "ANTHROPIC_BASE_URL".to_string(),
                    value: self.anthropic_base_url(),
                });
                if let Some(t) = token {
                    env.push(EnvOverride {
                        name: "ANTHROPIC_AUTH_TOKEN".to_string(),
                        value: t.to_string(),
                    });
                }
            }
            AgentKind::Codex => {
                env.push(EnvOverride {
                    name: "OPENAI_BASE_URL".to_string(),
                    value: self.openai_base_url(),
                });
                if let Some(t) = token {
                    env.push(EnvOverride {
                        name: "OPENAI_API_KEY".to_string(),
                        value: t.to_string(),
                    });
                }
            }
        }
        Some(GatewayInjection {
            agent,
            account_id: self.account_id.clone(),
            gateway_slug: self.gateway_slug.clone(),
            env,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_round_trip() {
        let cfg = GatewayConfig::default();
        let s = toml::to_string_pretty(&cfg).unwrap();
        let back: GatewayConfig = toml::from_str(&s).unwrap();
        // Defaults are preserved.
        assert_eq!(back.gateway_slug, DEFAULT_GATEWAY_SLUG);
        assert_eq!(back.token_doppler_ref, DEFAULT_TOKEN_DOPPLER_REF);
        assert!(!back.claude_code.enabled);
        assert!(!back.codex.enabled);
    }

    #[test]
    fn round_trip_populated() {
        let cfg = GatewayConfig {
            account_id: "ACCOUNT123".to_string(),
            gateway_slug: "x".to_string(),
            token_doppler_ref: "CF_AIG_TOKEN".to_string(),
            claude_code: AgentRoute { enabled: true },
            codex: AgentRoute { enabled: false },
        };
        let s = toml::to_string_pretty(&cfg).unwrap();
        let back: GatewayConfig = toml::from_str(&s).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn defaults_apply_when_field_missing() {
        let raw = r#"
account_id = "abc"
"#;
        let cfg: GatewayConfig = toml::from_str(raw).unwrap();
        assert_eq!(cfg.account_id, "abc");
        assert_eq!(cfg.gateway_slug, DEFAULT_GATEWAY_SLUG);
        assert_eq!(cfg.token_doppler_ref, DEFAULT_TOKEN_DOPPLER_REF);
        assert!(!cfg.claude_code.enabled);
        assert!(!cfg.codex.enabled);
    }

    #[test]
    fn load_missing_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does_not_exist.toml");
        let r = GatewayConfig::load_from(&path).unwrap();
        assert!(r.is_none());
    }

    #[test]
    fn save_then_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("ai_gateway.toml");
        let cfg = GatewayConfig {
            account_id: "ACC".to_string(),
            gateway_slug: "x".to_string(),
            token_doppler_ref: "CF_AIG_TOKEN".to_string(),
            claude_code: AgentRoute { enabled: true },
            codex: AgentRoute { enabled: true },
        };
        cfg.save_to(&path).unwrap();
        let back = GatewayConfig::load_from(&path).unwrap().unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn injection_for_claude_when_disabled_is_none() {
        let cfg = GatewayConfig {
            account_id: "ACC".to_string(),
            ..Default::default()
        };
        // claude_code.enabled is false by default.
        assert!(cfg
            .env_overrides_for(AgentKind::ClaudeCode, Some("token"))
            .is_none());
    }

    #[test]
    fn injection_for_claude_with_empty_account_is_none() {
        let cfg = GatewayConfig {
            account_id: "   ".to_string(),
            claude_code: AgentRoute { enabled: true },
            ..Default::default()
        };
        // Even with toggle on, blank account suppresses injection so we
        // never emit `https://gateway.ai.cloudflare.com/v1//x/...`.
        assert!(cfg
            .env_overrides_for(AgentKind::ClaudeCode, Some("token"))
            .is_none());
    }

    #[test]
    fn injection_for_claude_emits_anthropic_url_and_token() {
        let cfg = GatewayConfig {
            account_id: "ACC".to_string(),
            gateway_slug: "x".to_string(),
            token_doppler_ref: "CF_AIG_TOKEN".to_string(),
            claude_code: AgentRoute { enabled: true },
            codex: AgentRoute::default(),
        };
        let inj = cfg
            .env_overrides_for(AgentKind::ClaudeCode, Some("secret"))
            .expect("injection");
        assert_eq!(inj.agent, AgentKind::ClaudeCode);
        assert_eq!(inj.account_id, "ACC");
        assert_eq!(inj.route_slug(), "dynamic/text_gen");
        let names: Vec<&str> = inj.env.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["ANTHROPIC_BASE_URL", "ANTHROPIC_AUTH_TOKEN"]);
        assert!(inj.env[0].value.contains("gateway.ai.cloudflare.com"));
        assert!(inj.env[0].value.ends_with("/compat/anthropic"));
        assert_eq!(inj.env[1].value, "secret");
    }

    #[test]
    fn injection_for_codex_emits_openai_url_and_token() {
        let cfg = GatewayConfig {
            account_id: "ACC".to_string(),
            gateway_slug: "x".to_string(),
            token_doppler_ref: "CF_AIG_TOKEN".to_string(),
            claude_code: AgentRoute::default(),
            codex: AgentRoute { enabled: true },
        };
        let inj = cfg
            .env_overrides_for(AgentKind::Codex, Some("secret"))
            .expect("injection");
        assert_eq!(inj.agent, AgentKind::Codex);
        let names: Vec<&str> = inj.env.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["OPENAI_BASE_URL", "OPENAI_API_KEY"]);
        assert!(inj.env[0].value.ends_with("/compat"));
        assert!(!inj.env[0].value.ends_with("/compat/anthropic"));
        assert_eq!(inj.env[1].value, "secret");
    }

    #[test]
    fn injection_without_token_omits_auth_header_only() {
        let cfg = GatewayConfig {
            account_id: "ACC".to_string(),
            claude_code: AgentRoute { enabled: true },
            ..Default::default()
        };
        let inj = cfg
            .env_overrides_for(AgentKind::ClaudeCode, None)
            .expect("injection");
        let names: Vec<&str> = inj.env.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["ANTHROPIC_BASE_URL"]);
    }

    #[test]
    fn provider_tag_is_stable() {
        assert_eq!(AgentKind::ClaudeCode.provider_tag(), "claude_code");
        assert_eq!(AgentKind::Codex.provider_tag(), "codex");
    }
}

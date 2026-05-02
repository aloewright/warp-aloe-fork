//! Per-provider [`Cap`] settings used by the persistent orchestrator router
//! (PDX-104 [B2] task 4).
//!
//! Earlier revisions (B1) hardcoded the Claude Code monthly + session caps as
//! `const` values inside `local_orchestrator.rs`. As B2 adds Codex and Ollama
//! to the registry, the cap table needs to grow as well — and we want a
//! single, easy-to-tune location for it. This module is the central settings
//! surface for those caps.
//!
//! # Design
//!
//! * The shape mirrors `cloud_provider.rs`: a small per-provider config
//!   struct with a `load()` entry point that consolidates defaults and
//!   future settings overrides.
//! * Local-only providers (Ollama, Apple Foundation Models) have no real
//!   billing surface — we model that as `u64::MAX` micro-dollars rather
//!   than introducing a separate "unbounded" branch into `Cap`.
//! * Per-provider session caps are also tunable here, kept proportional to
//!   their monthly counterparts so a single multiplier flows through to
//!   both budget tiers.
//!
//! Future settings file integration (a follow-up to PDX-104) will populate
//! [`ProviderCapsConfig::load`] from disk; for now it returns the
//! [`ProviderCapsConfig::defaults`] table.

use std::collections::HashMap;

use orchestrator::{Cap, Provider};

/// Sentinel "no real cap" value for local-only providers. Anything that does
/// not represent real spend — Ollama, Foundation Models — uses this so the
/// budget tier filter stays in [`orchestrator::BudgetTier::Healthy`] forever.
pub(crate) const UNLIMITED_MICRO_DOLLARS: u64 = u64::MAX;

/// Per-month cap for [`Provider::ClaudeCode`], in micro-dollars (`$200`).
///
/// Set high enough that we never accidentally halt routing in the v1 wiring
/// — the user is paying Anthropic directly via the CLI's own auth, so this
/// is mostly an accounting bucket for tier-aware fallbacks. Tighten via
/// settings overrides in a follow-up.
pub(crate) const CLAUDE_CODE_MONTHLY_CAP_MICROS: u64 = 200_000_000;
/// Per-session cap for [`Provider::ClaudeCode`], in micro-dollars (`$50`).
pub(crate) const CLAUDE_CODE_SESSION_CAP_MICROS: u64 = 50_000_000;

/// Per-month cap for [`Provider::Codex`], in micro-dollars (`$100`).
///
/// Codex bills directly to OpenAI via the user's `~/.codex/auth.json`; as
/// with Claude Code this is an accounting bucket rather than a hard limit
/// we can enforce on the upstream. Conservative default.
pub(crate) const CODEX_MONTHLY_CAP_MICROS: u64 = 100_000_000;
/// Per-session cap for [`Provider::Codex`], in micro-dollars (`$25`).
pub(crate) const CODEX_SESSION_CAP_MICROS: u64 = 25_000_000;

/// Resolved per-provider cap table.
///
/// Returned by [`ProviderCapsConfig::load`] (the entry point you should
/// almost always use) or by [`ProviderCapsConfig::defaults`] (used by tests
/// and by the router bootstrap before settings are loaded).
#[derive(Debug, Clone)]
pub(crate) struct ProviderCapsConfig {
    caps: HashMap<Provider, Cap>,
}

impl ProviderCapsConfig {
    /// Default, in-process cap table.
    ///
    /// Always populates entries for every [`Provider`] the persistent router
    /// can register so [`orchestrator::Budget::current_tier`] never returns
    /// an "unknown provider" error mid-dispatch. Local-only providers use
    /// [`UNLIMITED_MICRO_DOLLARS`].
    pub(crate) fn defaults() -> Self {
        let mut caps = HashMap::new();
        caps.insert(
            Provider::FoundationModels,
            Cap {
                monthly_micro_dollars: UNLIMITED_MICRO_DOLLARS,
                session_micro_dollars: UNLIMITED_MICRO_DOLLARS,
            },
        );
        caps.insert(
            Provider::ClaudeCode,
            Cap {
                monthly_micro_dollars: CLAUDE_CODE_MONTHLY_CAP_MICROS,
                session_micro_dollars: CLAUDE_CODE_SESSION_CAP_MICROS,
            },
        );
        caps.insert(
            Provider::Codex,
            Cap {
                monthly_micro_dollars: CODEX_MONTHLY_CAP_MICROS,
                session_micro_dollars: CODEX_SESSION_CAP_MICROS,
            },
        );
        caps.insert(
            Provider::Ollama,
            Cap {
                monthly_micro_dollars: UNLIMITED_MICRO_DOLLARS,
                session_micro_dollars: UNLIMITED_MICRO_DOLLARS,
            },
        );
        Self { caps }
    }

    /// Load the per-provider cap table.
    ///
    /// Currently returns [`Self::defaults`]. The settings-file lookup wires
    /// in here as a follow-up; this entry point exists so call sites can
    /// switch to overrides without further restructuring.
    pub(crate) fn load() -> Self {
        // TODO(PDX-104 follow-up): merge user-settings overrides on top of
        // defaults. The settings schema piece is owned by the settings team.
        Self::defaults()
    }

    /// Borrow the resolved [`Provider`] → [`Cap`] table.
    pub(crate) fn caps(&self) -> &HashMap<Provider, Cap> {
        &self.caps
    }

    /// Consume the config and return the owned [`Provider`] → [`Cap`] map,
    /// suitable for [`orchestrator::Budget::new`].
    pub(crate) fn into_caps(self) -> HashMap<Provider, Cap> {
        self.caps
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// All four providers the persistent router wires up must appear in the
    /// default cap table — otherwise `Budget::current_tier` on dispatch
    /// returns `BudgetError::UnknownProvider` and the router rejects an
    /// otherwise-routable task.
    #[test]
    fn defaults_cover_every_known_provider() {
        let caps = ProviderCapsConfig::defaults();
        assert!(caps.caps().contains_key(&Provider::ClaudeCode));
        assert!(caps.caps().contains_key(&Provider::Codex));
        assert!(caps.caps().contains_key(&Provider::Ollama));
        assert!(caps.caps().contains_key(&Provider::FoundationModels));
    }

    /// Local-only providers must report unlimited spend so the tier filter
    /// never demotes them away from a [`orchestrator::Role::Worker`] task.
    #[test]
    fn local_providers_are_unlimited() {
        let caps = ProviderCapsConfig::defaults();
        let ollama = caps.caps().get(&Provider::Ollama).expect("ollama cap");
        assert_eq!(ollama.monthly_micro_dollars, UNLIMITED_MICRO_DOLLARS);
        assert_eq!(ollama.session_micro_dollars, UNLIMITED_MICRO_DOLLARS);

        let fm = caps
            .caps()
            .get(&Provider::FoundationModels)
            .expect("foundation models cap");
        assert_eq!(fm.monthly_micro_dollars, UNLIMITED_MICRO_DOLLARS);
        assert_eq!(fm.session_micro_dollars, UNLIMITED_MICRO_DOLLARS);
    }

    /// Cloud providers (Claude Code, Codex) need real bounded caps so the
    /// tier-aware filter can downgrade them when spend approaches the
    /// monthly ceiling. Guards against accidental "unlimited" defaults.
    #[test]
    fn cloud_providers_have_bounded_caps() {
        let caps = ProviderCapsConfig::defaults();
        for provider in [Provider::ClaudeCode, Provider::Codex] {
            let cap = caps.caps().get(&provider).expect("cap");
            assert!(
                cap.monthly_micro_dollars < UNLIMITED_MICRO_DOLLARS,
                "{provider:?} must have a bounded monthly cap"
            );
            assert!(
                cap.session_micro_dollars <= cap.monthly_micro_dollars,
                "{provider:?} session cap must not exceed monthly"
            );
        }
    }

    /// `load()` is currently equivalent to `defaults()`. This guard catches
    /// regressions where a future settings-file change accidentally drops
    /// a provider on the floor.
    #[test]
    fn load_matches_defaults_until_settings_layer_is_wired() {
        let loaded = ProviderCapsConfig::load();
        let defaults = ProviderCapsConfig::defaults();
        assert_eq!(loaded.caps().len(), defaults.caps().len());
        for (provider, cap) in defaults.caps() {
            let got = loaded.caps().get(provider).expect("provider missing");
            assert_eq!(got.monthly_micro_dollars, cap.monthly_micro_dollars);
            assert_eq!(got.session_micro_dollars, cap.session_micro_dollars);
        }
    }
}

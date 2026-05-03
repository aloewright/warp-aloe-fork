// SPDX-License-Identifier: AGPL-3.0-only
//
// Coding-agent sign-in widget for the AI settings page.
//
// PDX-103 [B1] task 6 introduced this widget with a single Claude Code row.
// PDX-104 [B2] task 5 appends Codex + Ollama rows here so all three providers
// the persistent Router knows about have a single sign-in / install surface.
//
// Visual order (top → bottom): Claude Code → Codex → Ollama → API keys (BYOK,
// rendered by the parent ai_page after this widget). The same widget renders
// above BYOK on three AI subpages.
//
// Mirrors the Doppler pattern from PDX-49/PDX-50: Claude Code and Codex
// rows shell out to their respective `claude /login` / `codex login` flows
// fire-and-forget, the CLI handles browser/keychain. Ollama is local-only —
// no auth flow, just an *installed / not installed* status with an install
// link. Each row re-polls registration after the user signs in so the
// persistent router picks up the now-healthy CLI without an app restart.

use warpui::elements::{
    Align, Container, CrossAxisAlignment, Element, Flex, MouseStateHandle, ParentElement, Text,
};
use warpui::fonts::{Properties, Weight};
use warpui::ui_components::{
    button::ButtonVariant,
    components::{Coords, UiComponent, UiComponentStyles},
};
use warpui::{AppContext, SingletonEntity};

use super::ai_page::{AISettingsPageAction, AISettingsPageView};
use super::settings_page::{
    render_settings_info_banner, SettingsWidget, CONTENT_FONT_SIZE, SUBHEADER_FONT_SIZE,
};
use crate::appearance::Appearance;

/// Three-state auth model surfaced inline in a CLI-agent row.
///
/// Used uniformly across the Claude Code and Codex rows. `NotInstalled`
/// means the CLI binary is not on `PATH`; `SignedOut` means the binary is
/// installed but the agent's auth surface (e.g. Claude Code's keychain
/// token, Codex's `~/.codex/auth.json`) reports no active credential.
#[derive(Debug, PartialEq)]
#[allow(dead_code)] // Some variants exercised only via the UI render path.
pub(crate) enum CliAgentAuthState {
    NotInstalled,
    SignedOut,
    SignedIn,
}

impl CliAgentAuthState {
    /// Cheap, synchronous auth-state probe for Claude Code (PDX-103 [B1] task 6).
    ///
    /// Two checks compose into one of three states:
    ///
    /// 1. `which::which("claude")` — is the CLI on `PATH` at all?
    ///    Missing → `NotInstalled`.
    /// 2. `local_orchestrator::agent_health_snapshot` — does the persistent
    ///    Router consider the registered `ClaudeCodeAgent` healthy?
    ///    Healthy → `SignedIn`. Unhealthy / absent → `SignedOut`.
    ///
    /// Both probes are non-blocking. Compiled out on WASM: the persistent
    /// router and `which` are gated `cfg(not(target_family = "wasm"))`. The
    /// widget falls back to `SignedOut` on web so the row renders coherently
    /// without a real CLI.
    pub(crate) fn detect_claude() -> Self {
        #[cfg(not(target_family = "wasm"))]
        {
            use crate::ai::agent_sdk::driver::local_orchestrator;
            use orchestrator::AgentId;

            if which::which("claude").is_err() {
                return Self::NotInstalled;
            }

            let id = AgentId(local_orchestrator::CLAUDE_CODE_SONNET_46_ID.to_string());
            match local_orchestrator::agent_health_snapshot(&id) {
                Some(h) if h.healthy => Self::SignedIn,
                _ => Self::SignedOut,
            }
        }
        #[cfg(target_family = "wasm")]
        {
            Self::SignedOut
        }
    }

    /// Cheap, synchronous auth-state probe for Codex (PDX-104 [B2] task 5).
    ///
    /// 1. `which::which("codex")` → `NotInstalled` if missing.
    /// 2. `local_orchestrator::codex_signed_in` reads `~/.codex/auth.json`
    ///    (mirrored at `harness/codex.rs`'s `CodexAuthDotJson`). The
    ///    auth-state probe is intentionally not gated on the persistent
    ///    Router's health snapshot: the `CodexAgent` constructor does
    ///    *not* probe `auth.json` on its own (it only probes the binary),
    ///    so the file-based check is the only authoritative signal.
    pub(crate) fn detect_codex() -> Self {
        #[cfg(not(target_family = "wasm"))]
        {
            use crate::ai::agent_sdk::driver::local_orchestrator;

            if which::which("codex").is_err() {
                return Self::NotInstalled;
            }

            match local_orchestrator::codex_signed_in() {
                Some(true) => Self::SignedIn,
                _ => Self::SignedOut,
            }
        }
        #[cfg(target_family = "wasm")]
        {
            Self::SignedOut
        }
    }

    /// Banner copy keyed off the agent's display name so all three CLI
    /// agents can share this scaffolding.
    fn banner(&self, agent: CliAgent) -> Option<(String, String)> {
        match self {
            Self::NotInstalled => Some((
                format!("{} CLI not installed", agent.display_name()),
                agent.install_hint().to_string(),
            )),
            Self::SignedOut => Some((
                format!("Not signed in to {}", agent.display_name()),
                format!(
                    "Click \"Sign in with {}\" — the CLI opens your browser for OAuth.",
                    agent.display_name()
                ),
            )),
            Self::SignedIn => Some((
                format!("Signed in to {}", agent.display_name()),
                "Available in the in-prompt model selector.".to_string(),
            )),
        }
    }

    fn button_enabled(&self) -> bool {
        !matches!(self, Self::NotInstalled)
    }
}

/// Identifies a CLI-agent row inside the widget (PDX-104 [B2] task 5).
///
/// Used to share the per-row scaffolding (banner copy, install hints,
/// button labels) without paying for a free-form string at every call
/// site. Ollama is detect-only and uses [`OllamaState`] instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CliAgent {
    Claude,
    Codex,
}

impl CliAgent {
    fn display_name(self) -> &'static str {
        match self {
            CliAgent::Claude => "Claude Code",
            CliAgent::Codex => "Codex",
        }
    }

    fn install_hint(self) -> &'static str {
        match self {
            CliAgent::Claude => "Install via `brew install anthropic/claude/claude` or follow https://docs.claude.com/claude-code/setup",
            CliAgent::Codex => "Install via `npm i -g @openai/codex` or follow https://platform.openai.com/docs/codex",
        }
    }

    fn install_button_label(self) -> &'static str {
        match self {
            CliAgent::Claude => "Install Claude Code",
            CliAgent::Codex => "Install Codex",
        }
    }

    fn signin_button_label(self) -> &'static str {
        match self {
            CliAgent::Claude => "Sign in with Claude Code",
            CliAgent::Codex => "Sign in with Codex",
        }
    }

    fn resignin_button_label(self) -> &'static str {
        match self {
            CliAgent::Claude => "Re-sign in with Claude Code",
            CliAgent::Codex => "Re-sign in with Codex",
        }
    }
}

fn button_label_for(agent: CliAgent, state: &CliAgentAuthState) -> &'static str {
    match state {
        CliAgentAuthState::NotInstalled => agent.install_button_label(),
        CliAgentAuthState::SignedOut => agent.signin_button_label(),
        CliAgentAuthState::SignedIn => agent.resignin_button_label(),
    }
}

/// Two-state install model for Ollama (PDX-104 [B2] task 5).
///
/// Ollama is local-only — no auth flow — so the row only renders an
/// *installed / not installed* status. The button when installed says
/// "Reload models" (re-polls the persistent router so newly pulled
/// models become routable without an app restart); when not installed
/// it points to the download page instead of a sign-in flow.
#[derive(Debug, PartialEq)]
pub(crate) enum OllamaState {
    NotInstalled,
    Installed,
}

impl OllamaState {
    pub(crate) fn detect() -> Self {
        #[cfg(not(target_family = "wasm"))]
        {
            use crate::ai::agent_sdk::driver::local_orchestrator;
            if local_orchestrator::ollama_installed() {
                Self::Installed
            } else {
                Self::NotInstalled
            }
        }
        #[cfg(target_family = "wasm")]
        {
            Self::NotInstalled
        }
    }

    fn banner(&self) -> (String, String) {
        match self {
            Self::NotInstalled => (
                "Ollama not installed".to_string(),
                "Install from https://ollama.com/download — local inference only, no sign-in needed.".to_string(),
            ),
            Self::Installed => (
                "Ollama installed".to_string(),
                "Available in the in-prompt model selector. Pull additional models with `ollama pull <name>`.".to_string(),
            ),
        }
    }

    fn button_label(&self) -> &'static str {
        match self {
            Self::NotInstalled => "Install Ollama",
            Self::Installed => "Reload Ollama models",
        }
    }
}

#[derive(Default)]
pub(super) struct CliAgentSignInWidget {
    claude_button: MouseStateHandle,
    codex_button: MouseStateHandle,
    ollama_button: MouseStateHandle,
    /// PDX-118 [E6]: toggle button for "Route Claude Code through gateway".
    gateway_claude_toggle: MouseStateHandle,
    /// PDX-118 [E6]: toggle button for "Route Codex through gateway".
    gateway_codex_toggle: MouseStateHandle,
}

/// PDX-118 [E6]: snapshot of the on-disk `~/.warp/ai_gateway.toml`.
/// Loaded synchronously in `render` because the file is small (a few
/// hundred bytes) and rarely changes.
struct GatewaySnapshot {
    account_id: String,
    gateway_slug: String,
    token_doppler_ref: String,
    claude_enabled: bool,
    codex_enabled: bool,
}

impl Default for GatewaySnapshot {
    fn default() -> Self {
        Self {
            account_id: String::new(),
            gateway_slug: ai_gateway_config::DEFAULT_GATEWAY_SLUG.to_string(),
            token_doppler_ref: ai_gateway_config::DEFAULT_TOKEN_DOPPLER_REF.to_string(),
            claude_enabled: false,
            codex_enabled: false,
        }
    }
}

impl GatewaySnapshot {
    fn load() -> Self {
        #[cfg(not(target_family = "wasm"))]
        {
            if let Ok(Some(cfg)) = ai_gateway_config::GatewayConfig::load_default() {
                return Self {
                    account_id: cfg.account_id,
                    gateway_slug: cfg.gateway_slug,
                    token_doppler_ref: cfg.token_doppler_ref,
                    claude_enabled: cfg.claude_code.enabled,
                    codex_enabled: cfg.codex.enabled,
                };
            }
        }
        Self::default()
    }

    fn endpoint_summary(&self) -> String {
        if self.account_id.trim().is_empty() {
            "Not configured. Edit ~/.warp/ai_gateway.toml: set account_id, gateway_slug (default \"x\"), and token_doppler_ref (default \"CF_AIG_TOKEN\").".to_string()
        } else {
            format!(
                "Account {} via gateway \"{}\". Token resolved from Doppler ref \"{}\".",
                self.account_id, self.gateway_slug, self.token_doppler_ref
            )
        }
    }
}

impl SettingsWidget for CliAgentSignInWidget {
    type View = AISettingsPageView;

    fn search_terms(&self) -> &str {
        "claude code codex ollama cli sign in login authenticate third-party coding agent install ai gateway cloudflare routing byok doppler cf_aig_token"
    }

    fn render(
        &self,
        _view: &Self::View,
        appearance: &Appearance,
        app: &AppContext,
    ) -> Box<dyn Element> {
        let ui_builder = appearance.ui_builder();
        let theme = appearance.theme();

        // PDX-121 [E9] task 5 — informational header showing whether the
        // in-prompt model selector is currently visible. Read-only: the
        // toggle itself lives in Settings → AI → AI features → Show model
        // selectors in prompt; this line just surfaces its state at the
        // top of the agent sign-in widget so users know where to look
        // when they ask "why don't I see an agent picker in my terminal?"
        let selector_on = *crate::terminal::session_settings::SessionSettings::as_ref(app)
            .show_model_selectors_in_prompt;
        let selector_indicator_text: &'static str = if selector_on {
            "In-prompt agent selector: ON"
        } else {
            "In-prompt agent selector: OFF — Settings → AI → AI features → Show model selectors in prompt"
        };
        let selector_indicator = Container::new(
            Align::new(
                Text::new_inline(
                    selector_indicator_text,
                    appearance.ui_font_family(),
                    CONTENT_FONT_SIZE,
                )
                .with_color(theme.nonactive_ui_text_color().into())
                .finish(),
            )
            .left()
            .finish(),
        )
        .with_padding_bottom(8.)
        .finish();

        let header = Container::new(
            Align::new(
                Text::new_inline(
                    "Coding agent sign-in",
                    appearance.ui_font_family(),
                    SUBHEADER_FONT_SIZE,
                )
                .with_style(Properties::default().weight(Weight::Bold))
                .with_color(theme.active_ui_text_color().into())
                .finish(),
            )
            .left()
            .finish(),
        )
        .with_padding_bottom(8.)
        .finish();

        let description = Container::new(
            Align::new(
                Text::new_inline(
                    "Sign in to coding agents Warp dispatches through. Each row shells out to that agent's own login flow.",
                    appearance.ui_font_family(),
                    CONTENT_FONT_SIZE,
                )
                .with_color(theme.nonactive_ui_text_color().into())
                .finish(),
            )
            .left()
            .finish(),
        )
        .with_padding_bottom(12.)
        .finish();

        let button_style = UiComponentStyles {
            font_size: Some(14.),
            font_weight: Some(Weight::Semibold),
            padding: Some(Coords {
                top: 8.,
                bottom: 8.,
                left: 24.,
                right: 24.,
            }),
            ..Default::default()
        };

        // ── PDX-118 [E6] AI Gateway routing section ───────────────────
        // Sits ABOVE the per-agent sign-in rows so users see the
        // gateway routing posture before they wonder where their
        // CLAUDE_API_KEY went. The endpoint fields live in
        // `~/.warp/ai_gateway.toml` (edit out-of-band); the per-agent
        // toggles ship as on-screen buttons.
        let gateway = GatewaySnapshot::load();

        let gateway_header = Container::new(
            Align::new(
                Text::new_inline(
                    "AI Gateway routing",
                    appearance.ui_font_family(),
                    SUBHEADER_FONT_SIZE,
                )
                .with_style(Properties::default().weight(Weight::Bold))
                .with_color(theme.active_ui_text_color().into())
                .finish(),
            )
            .left()
            .finish(),
        )
        .with_padding_bottom(8.)
        .finish();

        let gateway_description = Container::new(
            Align::new(
                Text::new_inline(
                    "Route third-party agent CLIs through Cloudflare AI Gateway for caching, rate limits, observability, and BYOK routing. Token is resolved from Doppler at spawn time.",
                    appearance.ui_font_family(),
                    CONTENT_FONT_SIZE,
                )
                .with_color(theme.nonactive_ui_text_color().into())
                .finish(),
            )
            .left()
            .finish(),
        )
        .with_padding_bottom(8.)
        .finish();

        let gateway_endpoint_banner: Box<dyn Element> =
            Container::new(render_settings_info_banner(
                "Gateway endpoint",
                Some(&gateway.endpoint_summary()),
                appearance,
            ))
            .with_padding_bottom(12.)
            .finish();

        let gateway_claude_btn_label = if gateway.claude_enabled {
            "Disable gateway routing for Claude Code"
        } else {
            "Enable gateway routing for Claude Code"
        };
        let gateway_claude_btn = ui_builder
            .button(ButtonVariant::Accent, self.gateway_claude_toggle.clone())
            .with_text_label(gateway_claude_btn_label.to_owned())
            .with_style(button_style.clone())
            .build()
            .on_click({
                let next = !gateway.claude_enabled;
                move |ctx, _, _| {
                    ctx.dispatch_typed_action(AISettingsPageAction::ToggleGatewayForAgent {
                        agent: ai_gateway_config::AgentKind::ClaudeCode,
                        enabled: next,
                    });
                }
            })
            .finish();

        let gateway_codex_btn_label = if gateway.codex_enabled {
            "Disable gateway routing for Codex"
        } else {
            "Enable gateway routing for Codex"
        };
        let gateway_codex_btn = ui_builder
            .button(ButtonVariant::Accent, self.gateway_codex_toggle.clone())
            .with_text_label(gateway_codex_btn_label.to_owned())
            .with_style(button_style.clone())
            .build()
            .on_click({
                let next = !gateway.codex_enabled;
                move |ctx, _, _| {
                    ctx.dispatch_typed_action(AISettingsPageAction::ToggleGatewayForAgent {
                        agent: ai_gateway_config::AgentKind::Codex,
                        enabled: next,
                    });
                }
            })
            .finish();

        // ── Claude Code row ────────────────────────────────────────────
        let claude_state = CliAgentAuthState::detect_claude();
        let claude_banner: Option<Box<dyn Element>> =
            claude_state.banner(CliAgent::Claude).map(|(title, sub)| {
                Container::new(render_settings_info_banner(&title, Some(&sub), appearance))
                    .with_padding_bottom(12.)
                    .finish()
            });
        let claude_btn_builder = ui_builder
            .button(ButtonVariant::Accent, self.claude_button.clone())
            .with_text_label(button_label_for(CliAgent::Claude, &claude_state).to_owned())
            .with_style(button_style.clone());
        let claude_button: Box<dyn Element> = if claude_state.button_enabled() {
            claude_btn_builder
                .build()
                .on_click(move |ctx, _, _| {
                    ctx.dispatch_typed_action(AISettingsPageAction::SignInWithClaudeCode);
                })
                .finish()
        } else {
            claude_btn_builder.disabled().build().finish()
        };

        // ── Codex row (PDX-104 [B2] task 5) ────────────────────────────
        let codex_state = CliAgentAuthState::detect_codex();
        let codex_banner: Option<Box<dyn Element>> =
            codex_state.banner(CliAgent::Codex).map(|(title, sub)| {
                Container::new(render_settings_info_banner(&title, Some(&sub), appearance))
                    .with_padding_bottom(12.)
                    .finish()
            });
        let codex_btn_builder = ui_builder
            .button(ButtonVariant::Accent, self.codex_button.clone())
            .with_text_label(button_label_for(CliAgent::Codex, &codex_state).to_owned())
            .with_style(button_style.clone());
        let codex_button: Box<dyn Element> = if codex_state.button_enabled() {
            codex_btn_builder
                .build()
                .on_click(move |ctx, _, _| {
                    ctx.dispatch_typed_action(AISettingsPageAction::SignInWithCodex);
                })
                .finish()
        } else {
            codex_btn_builder.disabled().build().finish()
        };

        // ── Ollama row (PDX-104 [B2] task 5, detect-only) ─────────────
        let ollama_state = OllamaState::detect();
        let (ollama_title, ollama_sub) = ollama_state.banner();
        let ollama_banner: Box<dyn Element> = Container::new(render_settings_info_banner(
            &ollama_title,
            Some(&ollama_sub),
            appearance,
        ))
        .with_padding_bottom(12.)
        .finish();

        let ollama_btn_builder = ui_builder
            .button(ButtonVariant::Accent, self.ollama_button.clone())
            .with_text_label(ollama_state.button_label().to_owned())
            .with_style(button_style);
        let ollama_button: Box<dyn Element> = match ollama_state {
            OllamaState::NotInstalled => ollama_btn_builder
                .build()
                .on_click(move |ctx, _, _| {
                    ctx.dispatch_typed_action(AISettingsPageAction::OpenOllamaInstall);
                })
                .finish(),
            OllamaState::Installed => ollama_btn_builder
                .build()
                .on_click(move |ctx, _, _| {
                    ctx.dispatch_typed_action(AISettingsPageAction::ReloadOllamaModels);
                })
                .finish(),
        };

        // ── Helm cloud section (PDX-119 [E7]) ──────────────────────────
        // PDX-118 may add an AI Gateway block ABOVE this point; the
        // helm-cloud section sits between AI Gateway and the per-agent
        // CLI rows so coordination conflicts on rebase have an obvious
        // resolution: keep both blocks, helm-cloud second.
        let helm_section = render_helm_cloud_section(appearance);

        // ── Compose ────────────────────────────────────────────────────
        let mut children: Vec<Box<dyn Element>> = vec![
            // PDX-121 [E9] task 5: informational header — stays at the
            // very top, ABOVE the gateway routing block.
            selector_indicator,
            // PDX-118 [E6]: gateway routing block.
            gateway_header,
            gateway_description,
            gateway_endpoint_banner,
            gateway_claude_btn,
            spacer(),
            gateway_codex_btn,
            spacer(),
            // PDX-119 [E7]: helm-cloud routing section.
            helm_section,
            spacer(),
            // Per-agent sign-in rows below.
            header,
            description,
        ];
        if let Some(b) = claude_banner {
            children.push(b);
        }
        children.push(claude_button);
        children.push(spacer());
        if let Some(b) = codex_banner {
            children.push(b);
        }
        children.push(codex_button);
        children.push(spacer());
        children.push(ollama_banner);
        children.push(ollama_button);

        Container::new(
            Flex::column()
                .with_cross_axis_alignment(CrossAxisAlignment::Start)
                .with_children(children)
                .finish(),
        )
        .with_padding_bottom(15.)
        .finish()
    }
}

/// Vertical separation between CLI-agent rows. Matches the existing
/// `with_padding_bottom(12.)` spacing the row banners use, kept as a
/// helper so future rows fall in line without copy/paste drift.
fn spacer() -> Box<dyn Element> {
    Container::new(Flex::column().finish())
        .with_padding_bottom(12.)
        .finish()
}

/// Render the helm-cloud routing section (PDX-119 [E7]).
///
/// Read-only status surface for the V1 of this work: the user-facing
/// edits (base URL, toggle) live in `~/.warp/helm_cloud.toml` and a
/// later PR wires them into a true settings editor. The status banner
/// reflects the live config + last-create timestamp from the
/// `HelmCloudClient::status()` snapshot when one is available.
#[cfg(not(target_family = "wasm"))]
fn render_helm_cloud_section(appearance: &Appearance) -> Box<dyn Element> {
    use helm_cloud_client::load_helm_cloud_config;

    // Default the toggle as if `warp_hosted=true` so the section reflects
    // what the user opted into rather than what the default would be.
    let cfg = load_helm_cloud_config(true);
    let title = "Helm cloud";
    let subtitle = if cfg.route_cloud_env_through_helm {
        format!(
            "Routing cloud-environment creation through helm-cloud at {}. Edit ~/.warp/helm_cloud.toml to change.",
            cfg.base_url,
        )
    } else {
        format!(
            "Helm cloud routing OFF (using Warp-hosted). Set route_cloud_env_through_helm = true in ~/.warp/helm_cloud.toml to enable; base = {}.",
            cfg.base_url,
        )
    };
    Container::new(render_settings_info_banner(title, Some(&subtitle), appearance))
        .with_padding_bottom(12.)
        .finish()
}

#[cfg(target_family = "wasm")]
fn render_helm_cloud_section(appearance: &Appearance) -> Box<dyn Element> {
    Container::new(render_settings_info_banner(
        "Helm cloud",
        Some("Native-only feature; not available in the web bundle."),
        appearance,
    ))
    .with_padding_bottom(12.)
    .finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_installed_disables_button() {
        let s = CliAgentAuthState::NotInstalled;
        assert!(!s.button_enabled());
        assert_eq!(
            button_label_for(CliAgent::Claude, &s),
            "Install Claude Code"
        );
        let (title, _) = s.banner(CliAgent::Claude).expect("banner");
        assert!(title.to_lowercase().contains("not installed"));
    }

    #[test]
    fn signed_out_enables_button_with_login_label() {
        let s = CliAgentAuthState::SignedOut;
        assert!(s.button_enabled());
        assert_eq!(
            button_label_for(CliAgent::Claude, &s),
            "Sign in with Claude Code"
        );
    }

    #[test]
    fn signed_in_offers_resign_path() {
        let s = CliAgentAuthState::SignedIn;
        assert!(s.button_enabled());
        assert!(button_label_for(CliAgent::Claude, &s).contains("Re-sign in"));
    }

    /// PDX-104 [B2] task 5: Codex labels mirror the Claude pattern but
    /// use the Codex CLI's vocabulary.
    #[test]
    fn codex_label_set_mirrors_claude_pattern() {
        assert_eq!(
            button_label_for(CliAgent::Codex, &CliAgentAuthState::NotInstalled),
            "Install Codex"
        );
        assert_eq!(
            button_label_for(CliAgent::Codex, &CliAgentAuthState::SignedOut),
            "Sign in with Codex"
        );
        assert_eq!(
            button_label_for(CliAgent::Codex, &CliAgentAuthState::SignedIn),
            "Re-sign in with Codex"
        );
    }

    /// PDX-104 [B2] task 5: Ollama is detect-only — no `SignedOut` /
    /// `SignedIn` states. The not-installed banner must point users at
    /// the install page rather than a sign-in flow.
    #[test]
    fn ollama_state_does_not_use_auth_vocab() {
        let s = OllamaState::NotInstalled;
        assert_eq!(s.button_label(), "Install Ollama");
        let (title, sub) = s.banner();
        assert!(title.to_lowercase().contains("not installed"));
        assert!(!sub.to_lowercase().contains("sign in"));
    }

    /// Ollama installed shows "Reload models" — not "Re-sign in" —
    /// because there's no auth state.
    #[test]
    fn ollama_installed_button_says_reload() {
        let s = OllamaState::Installed;
        assert_eq!(s.button_label(), "Reload Ollama models");
        let (title, _) = s.banner();
        assert!(!title.to_lowercase().contains("not installed"));
    }

    /// PDX-118 [E6]: an unconfigured snapshot points the user at the
    /// TOML file they need to edit, so they don't sit waiting for a
    /// flow that isn't implemented yet.
    #[test]
    fn gateway_snapshot_default_shows_setup_instructions() {
        let s = GatewaySnapshot::default();
        let summary = s.endpoint_summary();
        assert!(summary.to_lowercase().contains("not configured"));
        assert!(summary.contains("ai_gateway.toml"));
        assert!(!s.claude_enabled);
        assert!(!s.codex_enabled);
    }

    /// PDX-118 [E6]: a populated snapshot prints the account id +
    /// gateway slug + token Doppler ref so a glance at the settings
    /// page tells the user where their agent traffic is going.
    #[test]
    fn gateway_snapshot_populated_summary_mentions_account_and_doppler_ref() {
        let s = GatewaySnapshot {
            account_id: "ACC42".to_string(),
            gateway_slug: "x".to_string(),
            token_doppler_ref: "CF_AIG_TOKEN".to_string(),
            claude_enabled: true,
            codex_enabled: false,
        };
        let summary = s.endpoint_summary();
        assert!(summary.contains("ACC42"));
        assert!(summary.contains("CF_AIG_TOKEN"));
        assert!(summary.contains("\"x\""));
    }

    /// PDX-118 [E6]: search_terms() must match the gateway routing
    /// vocabulary so the settings search surface picks the widget up
    /// for queries like "ai gateway" or "cloudflare".
    #[test]
    fn search_terms_cover_gateway_vocabulary() {
        let w = CliAgentSignInWidget::default();
        let terms = w.search_terms();
        for needle in [
            "ai gateway",
            "cloudflare",
            "routing",
            "byok",
            "doppler",
            "cf_aig_token",
        ] {
            assert!(
                terms.contains(needle),
                "search_terms missing '{needle}': {terms}"
            );
        }
    }
}

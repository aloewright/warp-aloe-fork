//! PDX-118 [E6] integration tests for the Cloudflare AI Gateway env
//! injection. The `#[ignore]`-gated end-to-end test mutates `$HOME`,
//! writes a real `ai_gateway.toml`, and asserts that the runner-level
//! injection produces the BASE_URL the spec calls for. It does NOT
//! make any network call to the gateway.

#![cfg(not(target_family = "wasm"))]

use ai_gateway_config::AgentKind;

/// End-to-end check: with both toggles on and a temp `$HOME`, the
/// injection contains `gateway.ai.cloudflare.com`. Marked `#[ignore]`
/// because mutating `$HOME` is process-global; run with
/// `cargo test --test gateway -- --ignored --test-threads=1`.
#[tokio::test]
#[ignore]
async fn injects_anthropic_base_url_for_claude() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg_dir = tmp.path().join(".warp");
    std::fs::create_dir_all(&cfg_dir).expect("mkdir");
    let toml = r#"
account_id = "ACC123"
gateway_slug = "x"
token_doppler_ref = "CF_AIG_TOKEN"

[claude_code]
enabled = true

[codex]
enabled = true
"#;
    std::fs::write(cfg_dir.join("ai_gateway.toml"), toml).expect("write toml");

    let prev_home = std::env::var_os("HOME");
    std::env::set_var("HOME", tmp.path());

    let inj = agents::gateway::_test_resolve_injection(AgentKind::ClaudeCode, None)
        .await
        .expect("claude injection");
    assert_eq!(inj.account_id, "ACC123");
    let url = &inj.env[0].value;
    assert!(url.contains("gateway.ai.cloudflare.com"), "url = {url}");
    assert!(url.contains("/v1/ACC123/x/compat/anthropic"), "url = {url}");

    let inj = agents::gateway::_test_resolve_injection(AgentKind::Codex, None)
        .await
        .expect("codex injection");
    let url = &inj.env[0].value;
    assert!(url.ends_with("/v1/ACC123/x/compat"), "url = {url}");

    if let Some(h) = prev_home {
        std::env::set_var("HOME", h);
    } else {
        std::env::remove_var("HOME");
    }
}

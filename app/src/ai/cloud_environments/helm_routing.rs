// SPDX-License-Identifier: AGPL-3.0-only
//
// PDX-119 [E7] — helm-cloud routing seam for cloud-environment creation.
//
// The existing Warp-hosted creation path (`UpdateManager::
// create_ambient_agent_environment` → GraphQL → `workspace.rs:168`) is
// preserved verbatim. This module is a thin native-only seam: when the
// `~/.warp/helm_cloud.toml::route_cloud_env_through_helm` toggle is on,
// the create site calls `route_create_session_through_helm` to dispatch
// the request through `helm_cloud_client::HelmCloudClient` instead.
//
// Failure mode: per the spec, helm-cloud failures are NOT silently
// fallback-routed to Warp-hosted. If the toggle is on and the helm-cloud
// call fails, we surface the error so the user (and the audit log) sees
// it.

#![cfg(not(target_family = "wasm"))]

use anyhow::Result;

use helm_cloud_client::{
    load_helm_cloud_config, CreateSessionArgs, HelmAuth, HelmAuthSource, HelmCloudClient,
    HelmCloudConfig, SessionId,
};

/// Test-friendly view of whether the helm-cloud routing path should be
/// taken. The `warp_hosted` argument matches the cargo build feature of
/// the same name; it controls the *default* if no toml exists yet.
pub fn should_route_through_helm(warp_hosted: bool) -> bool {
    let cfg = load_helm_cloud_config(warp_hosted);
    cfg.route_cloud_env_through_helm
}

/// One-shot helper for the cloud-environment create site. Caller passes
/// the same arguments the Warp-hosted GraphQL request would have sent;
/// we exchange creds and POST `/api/sessions`. Returns the helm-cloud
/// session id on success.
///
/// `auth_source` is the upstream credential the JWT-exchange will trade
/// for a helm session JWT — supplied by the caller from either the OS
/// keychain (Cloudflare Access) or `crates/doppler_mcp` (Doppler SVC
/// token).
pub async fn route_create_session_through_helm(
    cfg: HelmCloudConfig,
    auth_source: HelmAuthSource,
    args: CreateSessionArgs,
) -> Result<SessionId> {
    let auth = HelmAuth::new(&cfg.base_url);
    auth.set_source(auth_source).await;
    let client = HelmCloudClient::new(&cfg.base_url, auth);
    let session_id = client.create_session(args).await?;
    Ok(session_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Toggle-OFF preserves the existing Warp-hosted behavior: callers
    /// must not branch into the helm-cloud path.
    #[test]
    fn toggle_off_preserves_warp_hosted_path() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("helm_cloud.toml");
        std::fs::write(
            &path,
            "base_url = \"http://localhost:8787\"\n\
             route_cloud_env_through_helm = false\n",
        )
        .unwrap();
        std::env::set_var("WARP_HELM_CLOUD_CONFIG", &path);
        // `warp_hosted=true` would default the toggle off anyway, but we
        // assert the explicit-off file wins regardless.
        assert!(!should_route_through_helm(true));
        assert!(!should_route_through_helm(false));
        std::env::remove_var("WARP_HELM_CLOUD_CONFIG");
    }

    /// Toggle-ON forces the helm-cloud path even on hosted builds.
    #[test]
    fn toggle_on_routes_through_helm() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("helm_cloud.toml");
        std::fs::write(
            &path,
            "base_url = \"http://localhost:8787\"\n\
             route_cloud_env_through_helm = true\n",
        )
        .unwrap();
        std::env::set_var("WARP_HELM_CLOUD_CONFIG", &path);
        assert!(should_route_through_helm(true));
        assert!(should_route_through_helm(false));
        std::env::remove_var("WARP_HELM_CLOUD_CONFIG");
    }

    /// Default for `warp_hosted=false` (helm-fork build) is ON. This is
    /// the user-visible fix for the "out of add-on credits" wall on the
    /// helm-cloud distribution: out of the box, new cloud-environment
    /// creates go through helm-cloud.
    #[test]
    fn default_for_helm_fork_is_on() {
        let dir = TempDir::new().unwrap();
        std::env::set_var(
            "WARP_HELM_CLOUD_CONFIG",
            dir.path().join("nonexistent.toml"),
        );
        assert!(should_route_through_helm(false));
        std::env::remove_var("WARP_HELM_CLOUD_CONFIG");
    }
}

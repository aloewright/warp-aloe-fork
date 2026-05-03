// SPDX-License-Identifier: AGPL-3.0-only
//
// `~/.warp/helm_cloud.toml` persistence.
//
// We avoid pulling `serde`'s `toml` crate — `toml_edit` is already in the
// workspace and round-trips cleanly without a new dep. The schema is
// deliberately small (two fields), so the parsing is hand-rolled.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use toml_edit::{value, DocumentMut};

/// Default helm-cloud base URL when the user is running `wrangler dev`
/// against the local control plane.
pub const DEFAULT_BASE_URL: &str = "http://localhost:8787";

/// Persisted client config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelmCloudConfig {
    /// Helm-cloud control-plane base URL (no trailing slash).
    pub base_url: String,
    /// When `true`, cloud-environment creation goes through helm-cloud
    /// instead of Warp's hosted GraphQL.
    pub route_cloud_env_through_helm: bool,
}

impl HelmCloudConfig {
    /// Default for a fresh install. The toggle defaults to `true` when the
    /// caller passes `warp_hosted=false` (i.e. the helm-cloud fork build),
    /// otherwise `false` so Warp-hosted users keep their existing flow.
    pub fn defaults_for(warp_hosted: bool) -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            route_cloud_env_through_helm: !warp_hosted,
        }
    }
}

/// Resolve the on-disk path. Honors `WARP_HELM_CLOUD_CONFIG` for tests so
/// we don't write into the developer's real `~/.warp` during unit runs.
pub fn config_path() -> PathBuf {
    if let Ok(p) = std::env::var("WARP_HELM_CLOUD_CONFIG") {
        return PathBuf::from(p);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".warp").join("helm_cloud.toml")
}

/// Load the config from disk, falling back to defaults if the file does
/// not exist or is malformed. A malformed file is logged but not fatal —
/// the user gets the default behavior and can fix the file at leisure.
pub fn load_helm_cloud_config(warp_hosted: bool) -> HelmCloudConfig {
    load_from(&config_path(), warp_hosted)
}

pub(crate) fn load_from(path: &Path, warp_hosted: bool) -> HelmCloudConfig {
    let defaults = HelmCloudConfig::defaults_for(warp_hosted);
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return defaults,
    };
    let doc: DocumentMut = match raw.parse() {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(?path, error = %e, "helm_cloud.toml parse failed; using defaults");
            return defaults;
        }
    };
    let base_url = doc
        .get("base_url")
        .and_then(|v| v.as_str())
        .map(|s| s.trim_end_matches('/').to_string())
        .unwrap_or(defaults.base_url);
    let route = doc
        .get("route_cloud_env_through_helm")
        .and_then(|v| v.as_bool())
        .unwrap_or(defaults.route_cloud_env_through_helm);
    HelmCloudConfig {
        base_url,
        route_cloud_env_through_helm: route,
    }
}

/// Persist the config. Creates `~/.warp/` if needed.
pub fn save_helm_cloud_config(cfg: &HelmCloudConfig) -> Result<()> {
    save_to(&config_path(), cfg)
}

pub(crate) fn save_to(path: &Path, cfg: &HelmCloudConfig) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    // Preserve any unrelated keys the user may have hand-edited in by
    // round-tripping through toml_edit when the file already exists.
    let mut doc = match std::fs::read_to_string(path) {
        Ok(raw) => raw.parse::<DocumentMut>().unwrap_or_default(),
        Err(_) => DocumentMut::new(),
    };
    doc["base_url"] = value(cfg.base_url.trim_end_matches('/'));
    doc["route_cloud_env_through_helm"] = value(cfg.route_cloud_env_through_helm);
    std::fs::write(path, doc.to_string())
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn defaults_for_warp_hosted_disables_routing() {
        let cfg = HelmCloudConfig::defaults_for(true);
        assert!(!cfg.route_cloud_env_through_helm);
        assert_eq!(cfg.base_url, DEFAULT_BASE_URL);
    }

    #[test]
    fn defaults_for_helm_fork_enables_routing() {
        let cfg = HelmCloudConfig::defaults_for(false);
        assert!(cfg.route_cloud_env_through_helm);
    }

    #[test]
    fn load_missing_returns_defaults() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("missing.toml");
        assert_eq!(load_from(&path, false), HelmCloudConfig::defaults_for(false));
    }

    #[test]
    fn save_and_round_trip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("helm_cloud.toml");
        let cfg = HelmCloudConfig {
            base_url: "https://helm.example.com".to_string(),
            route_cloud_env_through_helm: true,
        };
        save_to(&path, &cfg).unwrap();
        let loaded = load_from(&path, true);
        assert_eq!(loaded, cfg);
    }

    #[test]
    fn malformed_file_falls_back_to_defaults() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "this is = = not valid toml [[[").unwrap();
        let loaded = load_from(&path, true);
        assert_eq!(loaded, HelmCloudConfig::defaults_for(true));
    }

    #[test]
    fn trailing_slash_is_normalized() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("h.toml");
        std::fs::write(
            &path,
            "base_url = \"https://example.com/\"\nroute_cloud_env_through_helm = true\n",
        )
        .unwrap();
        let loaded = load_from(&path, false);
        assert_eq!(loaded.base_url, "https://example.com");
    }
}

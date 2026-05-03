// SPDX-License-Identifier: AGPL-3.0-only
//
// `cloud_env_routed` audit emitter.
//
// The spec asks for a structured audit row whenever the helm-cloud
// routing path is taken. Symphony's `AuditLog` enum is closed (and the
// PR scope forbids editing it), so this module emits a parallel JSONL
// row at `~/.warp/symphony/cloud_env_routed.log`.

use std::path::PathBuf;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Detail payload for a routed cloud-environment create.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudEnvRoutedDetail {
    pub helm_cloud_base_url: String,
    pub session_id: String,
}

/// One audit row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudEnvRoutedRecord {
    pub timestamp: DateTime<Utc>,
    pub rule: String,
    pub action: String,
    pub agent_id: String,
    pub detail: CloudEnvRoutedDetail,
}

/// Default on-disk location. Honors `WARP_HELM_CLOUD_AUDIT_LOG` for tests.
pub fn default_audit_path() -> PathBuf {
    if let Ok(p) = std::env::var("WARP_HELM_CLOUD_AUDIT_LOG") {
        return PathBuf::from(p);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".warp")
        .join("symphony")
        .join("cloud_env_routed.log")
}

static WRITE_LOCK: Mutex<()> = Mutex::new(());

/// Append one `cloud_env_routed` row. Best-effort: I/O failures are
/// logged via `tracing` and swallowed so the create flow doesn't fail
/// because of observability plumbing.
pub fn record_cloud_env_routed(detail: CloudEnvRoutedDetail) {
    let record = CloudEnvRoutedRecord {
        timestamp: Utc::now(),
        rule: "cloud_env_routed".to_string(),
        action: "allowed".to_string(),
        agent_id: "cloud_environment".to_string(),
        detail,
    };
    let line = match serde_json::to_string(&record) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "failed to serialize cloud_env_routed record");
            return;
        }
    };
    let path = default_audit_path();
    let _guard = WRITE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| {
            use std::io::Write;
            writeln!(f, "{line}")
        })
    {
        tracing::warn!(?path, error = %e, "failed to append cloud_env_routed record");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn writes_jsonl_row() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("audit.log");
        // Set per-process; tests in this module are single-threaded.
        std::env::set_var("WARP_HELM_CLOUD_AUDIT_LOG", &path);
        record_cloud_env_routed(CloudEnvRoutedDetail {
            helm_cloud_base_url: "http://localhost:8787".into(),
            session_id: "sess_42".into(),
        });
        let raw = std::fs::read_to_string(&path).unwrap();
        let parsed: CloudEnvRoutedRecord = serde_json::from_str(raw.trim()).unwrap();
        assert_eq!(parsed.rule, "cloud_env_routed");
        assert_eq!(parsed.action, "allowed");
        assert_eq!(parsed.agent_id, "cloud_environment");
        assert_eq!(parsed.detail.session_id, "sess_42");
        std::env::remove_var("WARP_HELM_CLOUD_AUDIT_LOG");
    }
}

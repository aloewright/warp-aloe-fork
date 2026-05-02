//! Bridge between Symphony's `git`-shell-out diff inspection and
//! [`auto_healing`]'s pre-parsed [`auto_healing::FileDiff`] surface.
//!
//! Runs `git diff --numstat HEAD` plus `git diff --name-status HEAD`
//! inside a workspace and merges the two outputs into a `FileDiff` list
//! the auto_healing crate can consume. We separate this from
//! [`crate::diff_guard`] (which uses `--shortstat` for a fast
//! aggregate) so the two checks are independent and either can be
//! disabled without affecting the other.

use std::collections::HashMap;
use std::path::Path;

use auto_healing::FileDiff;
use thiserror::Error;
use tokio::process::Command;

/// Errors raised by [`collect_workspace_diffs`].
#[derive(Debug, Error)]
pub enum NumstatError {
    /// `git` invocation failed.
    #[error("git failed: {0}")]
    GitFailed(String),
}

/// Run `git diff --numstat HEAD` + `git diff --name-status HEAD` and
/// merge the results.
///
/// On a non-git workspace or a fresh repo with no `HEAD`, this returns
/// an empty list (matching [`crate::diff_guard::DiffGuard::check`]'s
/// behaviour for the same edge cases).
pub async fn collect_workspace_diffs(
    workspace_path: &Path,
) -> Result<Vec<FileDiff>, NumstatError> {
    let numstat = run_git(workspace_path, &["diff", "--numstat", "HEAD"]).await?;
    let Some(numstat) = numstat else {
        return Ok(Vec::new());
    };
    let names = run_git(workspace_path, &["diff", "--name-status", "HEAD"]).await?;
    let names = names.unwrap_or_default();

    let mut deletion_paths: HashMap<String, bool> = HashMap::new();
    for raw in names.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        // name-status format: "<STATUS>\t<path>" or for renames
        // "R<score>\t<old>\t<new>". We only care about deletions ("D").
        let mut parts = line.split('\t');
        let Some(status) = parts.next() else {
            continue;
        };
        if !status.starts_with('D') {
            continue;
        }
        let Some(path) = parts.next() else { continue };
        deletion_paths.insert(path.to_string(), true);
    }

    let mut diffs = Vec::new();
    for raw in numstat.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        // numstat format: "<added>\t<removed>\t<path>"; both counts are
        // "-" for binary files.
        let mut parts = line.splitn(3, '\t');
        let added = parts.next().unwrap_or("0").trim();
        let removed = parts.next().unwrap_or("0").trim();
        let Some(path) = parts.next() else { continue };
        let path = path.trim();
        if path.is_empty() {
            continue;
        }
        let added_lines: usize = added.parse().unwrap_or(0);
        let removed_lines: usize = removed.parse().unwrap_or(0);
        let deleted = deletion_paths.contains_key(path);
        diffs.push(FileDiff {
            path: path.to_string(),
            added_lines,
            removed_lines,
            deleted,
        });
    }
    Ok(diffs)
}

async fn run_git(
    workspace_path: &Path,
    args: &[&str],
) -> Result<Option<String>, NumstatError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace_path)
        .args(args)
        .output()
        .await
        .map_err(|e| NumstatError::GitFailed(e.to_string()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr_lower = stderr.to_lowercase();
        if stderr_lower.contains("unknown revision")
            || stderr_lower.contains("ambiguous argument")
            || stderr_lower.contains("not a git repository")
        {
            tracing::debug!(
                workspace = %workspace_path.display(),
                stderr = %stderr.trim(),
                "numstat: non-repo or no-HEAD workspace; treating as zero-diff"
            );
            return Ok(None);
        }
        return Err(NumstatError::GitFailed(stderr.into_owned()));
    }
    Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn non_git_workspace_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let diffs = collect_workspace_diffs(tmp.path()).await.unwrap();
        assert!(diffs.is_empty());
    }

    #[tokio::test]
    async fn fresh_repo_no_head_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let _ = tokio::process::Command::new("git")
            .arg("-C")
            .arg(tmp.path())
            .arg("init")
            .arg("-q")
            .output()
            .await
            .unwrap();
        let diffs = collect_workspace_diffs(tmp.path()).await.unwrap();
        assert!(diffs.is_empty());
    }

    #[tokio::test]
    async fn detects_added_modified_and_deleted_files() {
        let tmp = tempfile::tempdir().unwrap();
        // Helper: shell out to git.
        async fn git(dir: &Path, args: &[&str]) {
            let out = tokio::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .await
                .unwrap();
            assert!(out.status.success(), "git {args:?} failed: {:?}", out);
        }

        git(tmp.path(), &["init", "-q"]).await;
        git(tmp.path(), &["config", "user.email", "test@example.com"]).await;
        git(tmp.path(), &["config", "user.name", "Test"]).await;
        git(tmp.path(), &["config", "commit.gpgsign", "false"]).await;

        // Initial commit: keep.txt + tests/old_test.rs.
        std::fs::write(tmp.path().join("keep.txt"), "k1\nk2\nk3\n").unwrap();
        std::fs::create_dir_all(tmp.path().join("tests")).unwrap();
        std::fs::write(
            tmp.path().join("tests/old_test.rs"),
            "fn t() {}\nfn u() {}\nfn v() {}\n",
        )
        .unwrap();
        git(tmp.path(), &["add", "."]).await;
        git(tmp.path(), &["commit", "-q", "-m", "init"]).await;

        // Working tree changes: modify keep.txt, delete tests/old_test.rs,
        // add a new file.
        std::fs::write(tmp.path().join("keep.txt"), "k1\nk2\nk3\nk4\n").unwrap();
        std::fs::remove_file(tmp.path().join("tests/old_test.rs")).unwrap();
        std::fs::write(tmp.path().join("new.txt"), "n1\nn2\n").unwrap();
        // `git diff HEAD` includes untracked? No — only after add.
        git(tmp.path(), &["add", "-A"]).await;

        let diffs = collect_workspace_diffs(tmp.path()).await.unwrap();
        let by_path: HashMap<&str, &FileDiff> =
            diffs.iter().map(|d| (d.path.as_str(), d)).collect();

        let kept = by_path.get("keep.txt").expect("keep.txt diff");
        assert_eq!(kept.added_lines, 1);
        assert_eq!(kept.removed_lines, 0);
        assert!(!kept.deleted);

        let new = by_path.get("new.txt").expect("new.txt diff");
        assert_eq!(new.added_lines, 2);
        assert_eq!(new.removed_lines, 0);
        assert!(!new.deleted);

        let removed = by_path
            .get("tests/old_test.rs")
            .expect("tests/old_test.rs diff");
        assert_eq!(removed.added_lines, 0);
        assert_eq!(removed.removed_lines, 3);
        assert!(removed.deleted);
    }
}

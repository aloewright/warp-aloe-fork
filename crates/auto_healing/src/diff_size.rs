//! Diff-size guardrail.
//!
//! Refuses agent-produced diffs whose total `added + removed` line count
//! exceeds a configured threshold. Intended for post-run evaluation
//! once the agent has finished editing the workspace.
//!
//! Symphony already has a sibling implementation that shells out to
//! `git diff --shortstat HEAD`. This crate accepts pre-parsed
//! [`FileDiff`] records so callers can drive the check from any source
//! (a `git diff --numstat` parse, an `rdiff` library, a direct in-memory
//! buffer comparison, etc.) without forcing a `git` shell-out.

use serde::{Deserialize, Serialize};

/// One file's worth of diff statistics.
///
/// Per-file rather than per-hunk to keep the surface tiny — the
/// guardrail only needs aggregate counts plus a path and a flag for
/// "file was removed entirely".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDiff {
    /// Path of the file relative to the workspace root.
    pub path: String,
    /// Lines added in this diff.
    pub added_lines: usize,
    /// Lines removed in this diff.
    pub removed_lines: usize,
    /// `true` if the file is being removed by this commit (as opposed
    /// to merely having lines deleted from it).
    pub deleted: bool,
}

impl FileDiff {
    /// Net change for this file (`added + removed`).
    pub fn churn(&self) -> usize {
        self.added_lines + self.removed_lines
    }
}

/// Guardrail configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffSizeCheck {
    /// Maximum allowed `added + removed` lines summed across every
    /// file in the diff.
    pub max_total_lines: usize,
}

impl DiffSizeCheck {
    /// New check with `max_total_lines` cap.
    pub fn new(max_total_lines: usize) -> Self {
        Self { max_total_lines }
    }

    /// Evaluate the check against a slice of file diffs.
    pub fn evaluate(&self, diffs: &[FileDiff]) -> DiffSizeDecision {
        let total: usize = diffs.iter().map(FileDiff::churn).sum();
        if total > self.max_total_lines {
            DiffSizeDecision::Block {
                reason: format!(
                    "diff size {} lines exceeds configured cap of {} lines",
                    total, self.max_total_lines
                ),
            }
        } else {
            DiffSizeDecision::Allow { total_lines: total }
        }
    }
}

/// Outcome of a [`DiffSizeCheck`] evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffSizeDecision {
    /// Diff was at or below the configured cap.
    Allow {
        /// Total observed `added + removed`.
        total_lines: usize,
    },
    /// Diff exceeded the cap.
    Block {
        /// Human-readable explanation suitable for a Linear comment.
        reason: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diff(path: &str, add: usize, rem: usize) -> FileDiff {
        FileDiff {
            path: path.into(),
            added_lines: add,
            removed_lines: rem,
            deleted: false,
        }
    }

    #[test]
    fn empty_diff_allows() {
        let check = DiffSizeCheck::new(100);
        let decision = check.evaluate(&[]);
        assert_eq!(decision, DiffSizeDecision::Allow { total_lines: 0 });
    }

    #[test]
    fn under_cap_allows() {
        let check = DiffSizeCheck::new(500);
        let diffs = vec![diff("a.rs", 100, 0), diff("b.rs", 200, 50)];
        let decision = check.evaluate(&diffs);
        assert_eq!(decision, DiffSizeDecision::Allow { total_lines: 350 });
    }

    #[test]
    fn at_cap_allows() {
        // Boundary: exactly at the cap is fine.
        let check = DiffSizeCheck::new(500);
        let diffs = vec![diff("a.rs", 250, 250)];
        let decision = check.evaluate(&diffs);
        assert_eq!(decision, DiffSizeDecision::Allow { total_lines: 500 });
    }

    #[test]
    fn one_over_cap_blocks() {
        // Boundary: cap + 1 trips.
        let check = DiffSizeCheck::new(500);
        let diffs = vec![diff("a.rs", 251, 250)];
        let decision = check.evaluate(&diffs);
        match decision {
            DiffSizeDecision::Block { reason } => {
                assert!(reason.contains("501"), "reason: {reason}");
                assert!(reason.contains("500"), "reason: {reason}");
            }
            _ => panic!("expected block"),
        }
    }

    #[test]
    fn cap_zero_blocks_any_change() {
        let check = DiffSizeCheck::new(0);
        let diffs = vec![diff("a.rs", 1, 0)];
        assert!(matches!(
            check.evaluate(&diffs),
            DiffSizeDecision::Block { .. }
        ));
        // Empty diff still allowed at cap=0.
        assert_eq!(
            check.evaluate(&[]),
            DiffSizeDecision::Allow { total_lines: 0 }
        );
    }

    #[test]
    fn aggregates_across_many_files() {
        let check = DiffSizeCheck::new(50);
        let diffs: Vec<_> = (0..10).map(|i| diff(&format!("f{i}.rs"), 6, 0)).collect();
        // total = 60, cap = 50 → block.
        match check.evaluate(&diffs) {
            DiffSizeDecision::Block { reason } => {
                assert!(reason.contains("60"));
            }
            _ => panic!("expected block"),
        }
    }

    #[test]
    fn churn_helper() {
        assert_eq!(diff("x", 3, 4).churn(), 7);
    }
}

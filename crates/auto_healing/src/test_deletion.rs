//! Test-deletion guardrail.
//!
//! Flags any agent commit that deletes a file matching one of the
//! configured test-path patterns. "Deletion" is defined as
//! [`FileDiff::deleted`] being `true` and at least one line removed,
//! which avoids false positives on rename-only changes that some diff
//! representations encode as `deleted == true && removed_lines == 0`.
//!
//! The default pattern set targets the conventional locations and
//! suffixes for Rust, TypeScript, JavaScript, Go and Python tests.
//! Callers can construct a custom check with [`TestDeletionCheck::with_patterns`].

use crate::diff_size::FileDiff;
use regex::RegexSet;

/// Default regular expressions matched against forward-slash-normalised
/// paths. The set covers:
///
/// * Anything under a `tests/` directory anywhere in the tree.
/// * Anything under a `__tests__/` directory (JS/TS convention).
/// * Anything under a `spec/` directory (Ruby/JS convention).
/// * `*_test.go` (Go).
/// * `*_test.rs` (Rust convention for inline integration tests).
/// * `*_test.py`, `test_*.py` (Python pytest convention).
/// * `*.test.ts`, `*.test.tsx`, `*.test.js`, `*.test.jsx`,
///   `*.spec.ts`, `*.spec.tsx`, `*.spec.js`, `*.spec.jsx` (JS/TS).
pub const DEFAULT_TEST_PATTERNS: &[&str] = &[
    r"(^|/)tests?/",
    r"(^|/)__tests__/",
    r"(^|/)spec/",
    r"_test\.go$",
    r"_test\.rs$",
    r"(^|/)test_[^/]+\.py$",
    r"_test\.py$",
    r"\.test\.[jt]sx?$",
    r"\.spec\.[jt]sx?$",
];

/// Test-deletion check.
#[derive(Debug, Clone)]
pub struct TestDeletionCheck {
    patterns: RegexSet,
}

impl Default for TestDeletionCheck {
    fn default() -> Self {
        Self::with_patterns(DEFAULT_TEST_PATTERNS).expect("default patterns must compile")
    }
}

impl TestDeletionCheck {
    /// Build a check from a list of regex patterns. Patterns are matched
    /// against forward-slash-normalised paths.
    pub fn with_patterns<I, S>(patterns: I) -> Result<Self, regex::Error>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let set = RegexSet::new(patterns)?;
        Ok(Self { patterns: set })
    }

    /// Evaluate the check. Returns the *first* offending diff in iter
    /// order; callers should treat the result as illustrative — there
    /// may be additional offenders.
    pub fn evaluate(&self, diffs: &[FileDiff]) -> TestDeletionDecision {
        for diff in diffs {
            if !diff.deleted {
                continue;
            }
            if diff.removed_lines == 0 {
                // Rename-only; not an actual content deletion.
                continue;
            }
            let normalized = diff.path.replace('\\', "/");
            if self.patterns.is_match(&normalized) {
                return TestDeletionDecision::Block {
                    reason: format!(
                        "agent attempted to delete test file `{}` ({} lines removed)",
                        diff.path, diff.removed_lines
                    ),
                    offending_path: diff.path.clone(),
                };
            }
        }
        TestDeletionDecision::Allow
    }
}

/// Outcome of a [`TestDeletionCheck`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestDeletionDecision {
    /// No test-file deletion was detected.
    Allow,
    /// A test-file deletion was detected and the operation should be blocked.
    Block {
        /// Human-readable reason.
        reason: String,
        /// Path of the offending file.
        offending_path: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deleted(path: &str, removed: usize) -> FileDiff {
        FileDiff {
            path: path.into(),
            added_lines: 0,
            removed_lines: removed,
            deleted: true,
        }
    }

    fn modified(path: &str, removed: usize) -> FileDiff {
        FileDiff {
            path: path.into(),
            added_lines: 5,
            removed_lines: removed,
            deleted: false,
        }
    }

    #[test]
    fn allow_on_empty_diff() {
        let check = TestDeletionCheck::default();
        assert_eq!(check.evaluate(&[]), TestDeletionDecision::Allow);
    }

    #[test]
    fn allow_when_no_test_files_touched() {
        let check = TestDeletionCheck::default();
        let diffs = vec![
            deleted("src/main.rs", 50),
            deleted("docs/readme.md", 10),
        ];
        assert_eq!(check.evaluate(&diffs), TestDeletionDecision::Allow);
    }

    #[test]
    fn allow_when_test_file_only_modified_not_deleted() {
        let check = TestDeletionCheck::default();
        // File matches a test pattern, but `deleted` is false → modified
        // tests are allowed.
        let diffs = vec![modified("crates/foo/tests/integration.rs", 5)];
        assert_eq!(check.evaluate(&diffs), TestDeletionDecision::Allow);
    }

    #[test]
    fn block_on_rust_tests_dir_deletion() {
        let check = TestDeletionCheck::default();
        let diffs = vec![deleted("crates/foo/tests/integration.rs", 80)];
        match check.evaluate(&diffs) {
            TestDeletionDecision::Block {
                offending_path, ..
            } => {
                assert_eq!(offending_path, "crates/foo/tests/integration.rs");
            }
            _ => panic!("expected block"),
        }
    }

    #[test]
    fn block_on_rust_underscore_test_suffix() {
        let check = TestDeletionCheck::default();
        let diffs = vec![deleted("crates/auth/src/auth_manager_test.rs", 200)];
        assert!(matches!(
            check.evaluate(&diffs),
            TestDeletionDecision::Block { .. }
        ));
    }

    #[test]
    fn block_on_typescript_dot_test() {
        let check = TestDeletionCheck::default();
        for path in [
            "src/foo.test.ts",
            "src/foo.test.tsx",
            "src/foo.test.js",
            "src/foo.test.jsx",
            "src/foo.spec.ts",
        ] {
            let diffs = vec![deleted(path, 30)];
            assert!(
                matches!(check.evaluate(&diffs), TestDeletionDecision::Block { .. }),
                "expected block for {path}"
            );
        }
    }

    #[test]
    fn block_on_go_test_suffix() {
        let check = TestDeletionCheck::default();
        let diffs = vec![deleted("internal/server/handler_test.go", 40)];
        assert!(matches!(
            check.evaluate(&diffs),
            TestDeletionDecision::Block { .. }
        ));
    }

    #[test]
    fn block_on_python_test_prefix() {
        let check = TestDeletionCheck::default();
        let diffs = vec![deleted("tests_dir_does_not_exist/test_foo.py", 20)];
        // `test_foo.py` directly under any directory.
        assert!(matches!(
            check.evaluate(&diffs),
            TestDeletionDecision::Block { .. }
        ));
    }

    #[test]
    fn block_on_jest_dunder_dir() {
        let check = TestDeletionCheck::default();
        let diffs = vec![deleted("src/__tests__/foo.js", 12)];
        assert!(matches!(
            check.evaluate(&diffs),
            TestDeletionDecision::Block { .. }
        ));
    }

    #[test]
    fn skips_rename_only_deletes_with_zero_removed_lines() {
        // Some diff producers encode "renamed" as `deleted == true,
        // removed_lines == 0`. Don't trip on those.
        let check = TestDeletionCheck::default();
        let diffs = vec![FileDiff {
            path: "tests/foo.rs".into(),
            added_lines: 0,
            removed_lines: 0,
            deleted: true,
        }];
        assert_eq!(check.evaluate(&diffs), TestDeletionDecision::Allow);
    }

    #[test]
    fn normalizes_windows_path_separators() {
        let check = TestDeletionCheck::default();
        let diffs = vec![deleted(r"crates\foo\tests\integration.rs", 50)];
        assert!(matches!(
            check.evaluate(&diffs),
            TestDeletionDecision::Block { .. }
        ));
    }

    #[test]
    fn custom_patterns_override_defaults() {
        // Strict pattern: only `.spec.ts` counts.
        let check = TestDeletionCheck::with_patterns(&[r"\.spec\.ts$"]).unwrap();
        // Default pattern would catch this; custom set does not.
        let diffs = vec![deleted("crates/foo/tests/integration.rs", 50)];
        assert_eq!(check.evaluate(&diffs), TestDeletionDecision::Allow);

        // Custom pattern still trips on `.spec.ts`.
        let diffs = vec![deleted("src/api.spec.ts", 50)];
        assert!(matches!(
            check.evaluate(&diffs),
            TestDeletionDecision::Block { .. }
        ));
    }

    #[test]
    fn returns_first_offender_when_multiple() {
        let check = TestDeletionCheck::default();
        let diffs = vec![
            deleted("src/foo.rs", 10), // not a test file
            deleted("tests/a_test.rs", 20),
            deleted("tests/b_test.rs", 30),
        ];
        match check.evaluate(&diffs) {
            TestDeletionDecision::Block {
                offending_path, ..
            } => assert_eq!(offending_path, "tests/a_test.rs"),
            _ => panic!("expected block"),
        }
    }
}

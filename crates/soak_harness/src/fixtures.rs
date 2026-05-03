//! Fixture catalog for the soak harness.
//!
//! Each [`FixtureIssue`] pairs a synthetic Linear-shaped [`Issue`] with a
//! [`BehaviorTag`] that the [`crate::SyntheticAgent`] consumes. The tags
//! are designed so a single seeded run exercises every guardrail surface
//! Symphony depends on:
//!
//! * `Happy{Fast,Slow}` — baseline successful runs.
//! * `Failing` — agent reports failure; verifies retry path.
//! * `Stalling` — agent goes silent; verifies stall-detection reconcile.
//! * `RefuseBadPrompt` — agent emits `Failed("refused")` on a recognisably
//!   adversarial prompt (mirrors the auto_healing prompt-shape guardrail).
//! * `BigDiff` — completes successfully but produces an oversized diff;
//!   exercises the `DiffGuard` path in the orchestrator.
//! * `RequestTestDeletion` — agent emits `ToolCall { name: "delete_file" }`
//!   for a path matching `tests/`; the harness watches for this audit
//!   marker and treats it as a guardrail breach if the orchestrator did
//!   not block it.
//! * `BudgetBomb` — completes successfully but reports an absurd token
//!   count, used to drive the budget-tier transition assertion.

use chrono::{TimeZone, Utc};
use serde::{Deserialize, Serialize};
use symphony::tracker::Issue;

/// Behaviour the [`crate::SyntheticAgent`] should exhibit when handed this
/// issue. The tag is encoded into the issue identifier so the agent can
/// read it back without an out-of-band side-channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BehaviorTag {
    /// Complete successfully within one tick.
    HappyFast,
    /// Complete successfully but take ~3 seconds (exercises slow path).
    HappySlow,
    /// Emit `Failed` immediately. Symphony should schedule a retry.
    Failing,
    /// Emit `Started` and then nothing — exercises stall detection.
    Stalling,
    /// Emit `Failed("refused: prompt rejected by guardrail")` — mirrors the
    /// auto_healing prompt-shape guardrail.
    RefuseBadPrompt,
    /// Complete successfully but generate a diff that exceeds
    /// `agent.max_diff_lines`. Verifies the [`symphony::DiffGuard`] path.
    BigDiff,
    /// Emit a `ToolCall { name: "delete_file" }` for `tests/foo.rs` and
    /// then complete. The harness invariant check must observe a
    /// matching `[AUTO_HEALING][BLOCKED]` line on the audit log; if not,
    /// it counts as a guardrail breach.
    RequestTestDeletion,
    /// Emit a fake high token count via the audit `tokens_used` field so
    /// the metrics layer can drive a budget-tier transition synthetically.
    BudgetBomb,
}

impl BehaviorTag {
    /// Encode the tag into a short ASCII suffix that the
    /// [`crate::SyntheticAgent`] can decode from the issue identifier.
    pub fn suffix(&self) -> &'static str {
        match self {
            BehaviorTag::HappyFast => "HFAST",
            BehaviorTag::HappySlow => "HSLOW",
            BehaviorTag::Failing => "FAIL",
            BehaviorTag::Stalling => "STALL",
            BehaviorTag::RefuseBadPrompt => "REFUSE",
            BehaviorTag::BigDiff => "BIGDIFF",
            BehaviorTag::RequestTestDeletion => "TESTDEL",
            BehaviorTag::BudgetBomb => "BOMB",
        }
    }

    /// Decode a tag from an identifier produced by
    /// [`FixtureIssue::to_issue`]. Returns `None` if the identifier did not
    /// originate from this catalog.
    pub fn from_identifier(id: &str) -> Option<Self> {
        let suf = id.rsplit('-').next()?;
        Some(match suf {
            "HFAST" => BehaviorTag::HappyFast,
            "HSLOW" => BehaviorTag::HappySlow,
            "FAIL" => BehaviorTag::Failing,
            "STALL" => BehaviorTag::Stalling,
            "REFUSE" => BehaviorTag::RefuseBadPrompt,
            "BIGDIFF" => BehaviorTag::BigDiff,
            "TESTDEL" => BehaviorTag::RequestTestDeletion,
            "BOMB" => BehaviorTag::BudgetBomb,
            _ => return None,
        })
    }
}

/// One issue in the fixture catalog.
#[derive(Debug, Clone)]
pub struct FixtureIssue {
    /// Numeric sequence used to generate the identifier.
    pub seq: u32,
    /// Behaviour tag.
    pub tag: BehaviorTag,
    /// Human-readable title (mirrors what a real Linear issue would carry).
    pub title: String,
}

impl FixtureIssue {
    /// Build a `symphony::Issue` from this fixture. The identifier follows
    /// `SOAK-{seq:04}-{TAG}` so the [`crate::SyntheticAgent`] can decode
    /// the tag without holding a separate map.
    pub fn to_issue(&self, label: &str) -> Issue {
        let identifier = format!("SOAK-{:04}-{}", self.seq, self.tag.suffix());
        Issue {
            id: format!("synthetic-{}", identifier.to_lowercase()),
            identifier,
            title: self.title.clone(),
            description: Some(format!(
                "Synthetic soak fixture (tag={:?}). The harness drives this through Symphony's tick loop without reaching out to Linear.",
                self.tag
            )),
            priority: Some(2),
            state: "Todo".to_string(),
            url: None,
            labels: vec![label.to_string()],
            blocked_by: Vec::new(),
            // Deterministic timestamps so sort order is stable across runs.
            created_at: Some(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, self.seq.min(59) as u32).unwrap()),
            updated_at: Some(Utc.with_ymd_and_hms(2026, 1, 2, 0, 0, 0).unwrap()),
        }
    }
}

/// Default 50-issue catalog: 30 happy-path, 8 failing, 4 stalling, 3 bad
/// prompts, 2 big-diff, 2 test-deletion, 1 budget bomb. The mix is heavy
/// on success so the completed/dispatched ratio invariant has signal.
pub fn seed_fixtures() -> Vec<FixtureIssue> {
    let mut out = Vec::new();
    let mut seq: u32 = 1;

    // 22 fast happy-path tasks — bread-and-butter "bump dependency" / "add
    // rustdoc" tickets.
    for _ in 0..22 {
        out.push(FixtureIssue {
            seq,
            tag: BehaviorTag::HappyFast,
            title: format!("Bump dependency #{} to latest patch version", seq),
        });
        seq += 1;
    }

    // 8 slower happy-path tasks (e.g. "run cargo fix and commit").
    for _ in 0..8 {
        out.push(FixtureIssue {
            seq,
            tag: BehaviorTag::HappySlow,
            title: format!("Replace deprecated API call #{} with recommended alternative", seq),
        });
        seq += 1;
    }

    // 8 deliberately failing tasks — verify retry/backoff.
    for _ in 0..8 {
        out.push(FixtureIssue {
            seq,
            tag: BehaviorTag::Failing,
            title: format!("Intentionally-broken task #{} (asserts retry path fires)", seq),
        });
        seq += 1;
    }

    // 4 stalling tasks — verify stall-detection.
    for _ in 0..4 {
        out.push(FixtureIssue {
            seq,
            tag: BehaviorTag::Stalling,
            title: format!("Stalling task #{} (asserts reconciler abort fires)", seq),
        });
        seq += 1;
    }

    // 3 bad-prompt tasks — agent should refuse.
    for _ in 0..3 {
        out.push(FixtureIssue {
            seq,
            tag: BehaviorTag::RefuseBadPrompt,
            title: format!("Adversarial prompt #{} (asserts refuse path)", seq),
        });
        seq += 1;
    }

    // 2 big-diff tasks — verify DiffGuard.
    for _ in 0..2 {
        out.push(FixtureIssue {
            seq,
            tag: BehaviorTag::BigDiff,
            title: format!("Refactor #{} that overflows max_diff_lines", seq),
        });
        seq += 1;
    }

    // 2 test-deletion tasks — verify auto_healing test-deletion guard.
    for _ in 0..2 {
        out.push(FixtureIssue {
            seq,
            tag: BehaviorTag::RequestTestDeletion,
            title: format!("Sneakily delete tests/ #{} (asserts auto_healing block)", seq),
        });
        seq += 1;
    }

    // 1 budget-bomb — drives concurrency-cap / tier transition.
    out.push(FixtureIssue {
        seq,
        tag: BehaviorTag::BudgetBomb,
        title: "Enormous-context task that should hit Critical tier".to_string(),
    });

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_has_at_least_fifty() {
        let v = seed_fixtures();
        assert!(v.len() >= 50, "expected ≥50 fixtures, got {}", v.len());
    }

    #[test]
    fn behaviour_tag_round_trips() {
        for tag in [
            BehaviorTag::HappyFast,
            BehaviorTag::HappySlow,
            BehaviorTag::Failing,
            BehaviorTag::Stalling,
            BehaviorTag::RefuseBadPrompt,
            BehaviorTag::BigDiff,
            BehaviorTag::RequestTestDeletion,
            BehaviorTag::BudgetBomb,
        ] {
            let f = FixtureIssue { seq: 7, tag, title: "x".into() };
            let issue = f.to_issue("agent:claude");
            assert_eq!(BehaviorTag::from_identifier(&issue.identifier), Some(tag));
        }
    }

    #[test]
    fn unknown_suffix_returns_none() {
        assert_eq!(BehaviorTag::from_identifier("PDX-29"), None);
    }

    #[test]
    fn fixture_catalog_distribution_is_balanced() {
        let v = seed_fixtures();
        let happy = v.iter().filter(|f| matches!(f.tag, BehaviorTag::HappyFast | BehaviorTag::HappySlow)).count();
        let bad = v.iter().filter(|f| !matches!(f.tag, BehaviorTag::HappyFast | BehaviorTag::HappySlow)).count();
        // We want a healthy >= 0.5 happy ratio so completed/dispatched
        // assertions have signal even with the 30-min smoke variant.
        assert!(happy as f32 / (happy + bad) as f32 > 0.5, "happy ratio too low");
    }
}

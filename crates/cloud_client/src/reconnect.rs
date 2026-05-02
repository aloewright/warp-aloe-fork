//! Pure exponential-backoff reconnect policy.
//!
//! Holds no IO; all the time math is computed from the supplied attempt
//! count so this is trivially unit-testable.

use std::time::Duration;

/// Decision returned by [`ReconnectPolicy::next_decision`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconnectDecision {
    /// Wait this long, then try again.
    Retry {
        /// Backoff delay before the next attempt.
        delay: Duration,
        /// 1-indexed attempt number (i.e. 1 for the first retry).
        attempt: u32,
    },
    /// Give up — the policy's maximum has been reached.
    GiveUp,
}

/// Exponential backoff with optional cap and a maximum number of attempts.
///
/// `delay = min(initial * 2^(attempt-1), max_delay)`.
///
/// The policy is deliberately deterministic (no jitter) so it's easy to
/// reason about in tests; the production `CloudAgent` is welcome to add
/// jitter on top if desired without changing this type.
#[derive(Debug, Clone)]
pub struct ReconnectPolicy {
    initial: Duration,
    max_delay: Duration,
    max_attempts: Option<u32>,
}

impl ReconnectPolicy {
    /// Construct a policy with the given initial delay, max delay cap, and
    /// optional attempt ceiling. Pass `max_attempts = None` for "retry
    /// forever".
    pub fn new(initial: Duration, max_delay: Duration, max_attempts: Option<u32>) -> Self {
        Self {
            initial,
            max_delay,
            max_attempts,
        }
    }

    /// A reasonable default: 200ms initial, 10s cap, retry forever.
    pub fn default_forever() -> Self {
        Self::new(Duration::from_millis(200), Duration::from_secs(10), None)
    }

    /// Compute what to do after `attempt` failed connect attempts.
    /// `attempt` is 1-indexed: passing `1` means "we just failed once,
    /// what now?".
    pub fn next_decision(&self, attempt: u32) -> ReconnectDecision {
        if let Some(cap) = self.max_attempts {
            if attempt > cap {
                return ReconnectDecision::GiveUp;
            }
        }
        // attempt=1 → 2^0=1; attempt=2 → 2^1=2; etc.
        // saturate to avoid overflow on absurd attempt counts.
        let shift = attempt.saturating_sub(1).min(20);
        let multiplier = 1u64.checked_shl(shift).unwrap_or(u64::MAX);
        let raw = self.initial.saturating_mul(multiplier as u32);
        let delay = if raw > self.max_delay {
            self.max_delay
        } else {
            raw
        };
        ReconnectDecision::Retry { delay, attempt }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doubles_until_cap() {
        let p = ReconnectPolicy::new(
            Duration::from_millis(100),
            Duration::from_millis(800),
            None,
        );
        let d = |a| match p.next_decision(a) {
            ReconnectDecision::Retry { delay, .. } => delay,
            ReconnectDecision::GiveUp => panic!("unexpected give-up"),
        };
        assert_eq!(d(1), Duration::from_millis(100));
        assert_eq!(d(2), Duration::from_millis(200));
        assert_eq!(d(3), Duration::from_millis(400));
        assert_eq!(d(4), Duration::from_millis(800));
        // Cap holds.
        assert_eq!(d(5), Duration::from_millis(800));
        assert_eq!(d(20), Duration::from_millis(800));
    }

    #[test]
    fn gives_up_after_max_attempts() {
        let p = ReconnectPolicy::new(
            Duration::from_millis(10),
            Duration::from_millis(100),
            Some(3),
        );
        assert!(matches!(
            p.next_decision(1),
            ReconnectDecision::Retry { .. }
        ));
        assert!(matches!(
            p.next_decision(3),
            ReconnectDecision::Retry { .. }
        ));
        assert_eq!(p.next_decision(4), ReconnectDecision::GiveUp);
    }

    #[test]
    fn never_overflows_on_huge_attempt_counts() {
        let p = ReconnectPolicy::new(
            Duration::from_millis(50),
            Duration::from_millis(1000),
            None,
        );
        // Should saturate at the cap, not panic.
        match p.next_decision(u32::MAX) {
            ReconnectDecision::Retry { delay, .. } => {
                assert_eq!(delay, Duration::from_millis(1000));
            }
            _ => panic!("retry expected"),
        }
    }
}

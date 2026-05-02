//! Production-deploy command gate (PDX-28 acceptance criterion 4).
//!
//! Intercepts shell commands the agent intends to run and refuses any
//! command matching one of the configured "production deploy" regexes
//! unless an explicit user override has been recorded.
//!
//! The default pattern set covers:
//!
//! * `wrangler deploy` (Cloudflare Workers)
//! * `gh release create` (GitHub release publish)
//! * `cargo publish` (crates.io publish)
//! * `npm publish` (npm registry publish)
//!
//! Patterns are anchored at the start of the command line (after
//! optional leading whitespace and an optional `sudo`/`env VAR=val`
//! prefix), and case-insensitive on the executable name.

use regex::RegexSet;

/// Default production-deploy regex anchors.
///
/// Each regex is matched against the post-trimmed command line and is
/// anchored with `^`. The shared `^(?:[a-zA-Z_][a-zA-Z0-9_]*=\S+\s+|sudo\s+)*`
/// prefix (added when the regex set is constructed) tolerates leading
/// `KEY=VAL` env exports and a leading `sudo`.
pub const DEFAULT_DEPLOY_PATTERNS: &[&str] = &[
    r"wrangler\s+deploy(\s|$)",
    r"gh\s+release\s+create(\s|$)",
    r"cargo\s+publish(\s|$)",
    r"npm\s+publish(\s|$)",
];

const ENV_OR_SUDO_PREFIX: &str = r"^(?:[A-Za-z_][A-Za-z0-9_]*=\S+\s+|sudo\s+)*";

/// Production deploy gate.
#[derive(Debug, Clone)]
pub struct DeployGate {
    set: RegexSet,
    /// When set, the gate behaves as a no-op (allow everything). Used
    /// to record an explicit user override per PDX-28 (default deny;
    /// human must approve).
    override_active: bool,
}

impl Default for DeployGate {
    fn default() -> Self {
        Self::with_patterns(DEFAULT_DEPLOY_PATTERNS).expect("default patterns must compile")
    }
}

impl DeployGate {
    /// Build a gate from custom patterns.
    pub fn with_patterns<I, S>(patterns: I) -> Result<Self, regex::Error>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        // Compose each pattern with the shared env/sudo-tolerant prefix.
        let composed: Vec<String> = patterns
            .into_iter()
            .map(|p| format!("{}{}", ENV_OR_SUDO_PREFIX, p.as_ref()))
            .collect();
        let set = RegexSet::new(composed)?;
        Ok(Self {
            set,
            override_active: false,
        })
    }

    /// Activate an explicit user override. While active, every command
    /// is allowed. Use this to record "yes, the human said run it".
    pub fn with_override(mut self, on: bool) -> Self {
        self.override_active = on;
        self
    }

    /// Evaluate against `cmd`. The check is best-effort textual: it
    /// does not parse the command line, so e.g. `bash -c "wrangler
    /// deploy"` will not trip. The intent is to catch the *typical*
    /// agent invocation, not to be a security boundary.
    pub fn evaluate(&self, cmd: &str) -> DeployGateDecision {
        if self.override_active {
            return DeployGateDecision::Allow;
        }
        let trimmed = cmd.trim_start();
        if self.set.is_match(trimmed) {
            DeployGateDecision::Block {
                reason: format!(
                    "production-deploy command intercepted; explicit human approval required: `{}`",
                    truncate(trimmed, 200)
                ),
            }
        } else {
            DeployGateDecision::Allow
        }
    }
}

/// Outcome of a [`DeployGate::evaluate`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeployGateDecision {
    /// The command may proceed.
    Allow,
    /// The command must not run without explicit human approval.
    Block {
        /// Human-readable reason suitable for a Linear comment.
        reason: String,
    },
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        let mut end = n;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_arbitrary_commands() {
        let g = DeployGate::default();
        for ok in [
            "ls -la",
            "cargo build",
            "npm install",
            "git commit -m foo",
            "wrangler dev",
            "wrangler tail",
            "echo wrangler deploy",
        ] {
            assert_eq!(g.evaluate(ok), DeployGateDecision::Allow, "cmd: {ok}");
        }
    }

    #[test]
    fn block_wrangler_deploy() {
        let g = DeployGate::default();
        for bad in [
            "wrangler deploy",
            "wrangler deploy --env production",
            "wrangler   deploy",
            "  wrangler deploy --name my-worker",
        ] {
            assert!(
                matches!(g.evaluate(bad), DeployGateDecision::Block { .. }),
                "expected block for {bad}"
            );
        }
    }

    #[test]
    fn block_gh_release_create() {
        let g = DeployGate::default();
        let bad = "gh release create v1.2.3 --notes ./RELEASE.md";
        assert!(matches!(g.evaluate(bad), DeployGateDecision::Block { .. }));
    }

    #[test]
    fn block_cargo_publish() {
        let g = DeployGate::default();
        for bad in ["cargo publish", "cargo publish --dry-run"] {
            assert!(
                matches!(g.evaluate(bad), DeployGateDecision::Block { .. }),
                "expected block for {bad}"
            );
        }
    }

    #[test]
    fn block_npm_publish() {
        let g = DeployGate::default();
        for bad in ["npm publish", "npm publish --access public"] {
            assert!(
                matches!(g.evaluate(bad), DeployGateDecision::Block { .. }),
                "expected block for {bad}"
            );
        }
    }

    #[test]
    fn tolerates_env_prefix() {
        let g = DeployGate::default();
        let bad = "CLOUDFLARE_API_TOKEN=xxx wrangler deploy --env production";
        assert!(matches!(g.evaluate(bad), DeployGateDecision::Block { .. }));
    }

    #[test]
    fn tolerates_sudo_prefix() {
        let g = DeployGate::default();
        let bad = "sudo npm publish";
        assert!(matches!(g.evaluate(bad), DeployGateDecision::Block { .. }));
    }

    #[test]
    fn override_disables_blocks() {
        let g = DeployGate::default().with_override(true);
        assert_eq!(
            g.evaluate("wrangler deploy --env production"),
            DeployGateDecision::Allow
        );
    }

    #[test]
    fn custom_patterns() {
        let g = DeployGate::with_patterns(&[r"helm\s+upgrade(\s|$)"])
            .unwrap();
        assert!(matches!(
            g.evaluate("helm upgrade prod ./charts"),
            DeployGateDecision::Block { .. }
        ));
        // Defaults no longer apply because we replaced the set.
        assert_eq!(g.evaluate("wrangler deploy"), DeployGateDecision::Allow);
    }

    #[test]
    fn does_not_match_substring_inside_word() {
        let g = DeployGate::default();
        // `mywrangler deploy` should NOT match `wrangler deploy` because
        // we anchor at start of the executable.
        assert_eq!(
            g.evaluate("mywrangler deploy"),
            DeployGateDecision::Allow
        );
    }
}

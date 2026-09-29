// Ported from openai/codex codex-rs/execpolicy/src/rule.rs
// at e72da2b53805894878023d01949a25a082e0a5cb, Apache-2.0 (`PatternToken` and
// `PrefixPattern` only; the rest of that file, that is `RuleMatch`, `PrefixRule`,
// `NetworkRule`, the `Rule` trait, and the Starlark-authoring validation helpers,
// belongs to codex's policy-authoring surface, which `execpolicy::policy`
// deliberately does not take. See that module's doc comment.
// Changes: dropped the `Arc`/`serde`/`AbsolutePathBuf` fields codex's richer
// `RuleMatch` needed; `matches_prefix` is otherwise unchanged.

/// Matches a single command token, either a fixed string or one of several allowed alternatives.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PatternToken {
    Single(String),
    Alts(Vec<String>),
}

impl PatternToken {
    fn matches(&self, token: &str) -> bool {
        match self {
            Self::Single(expected) => expected == token,
            Self::Alts(alternatives) => alternatives.iter().any(|alt| alt == token),
        }
    }
}

/// Prefix matcher for commands with support for alternative match tokens.
/// First token is fixed since we key by the first token in policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrefixPattern {
    pub first: String,
    pub rest: Vec<PatternToken>,
}

impl PrefixPattern {
    /// A pattern that matches only the exact argv given, one token each.
    /// `tokens` must be non-empty; an empty pattern can never match a real
    /// command's argv[0] and every caller in this crate supplies at least one.
    pub fn exact(tokens: &[&str]) -> Self {
        let first = tokens.first().copied().unwrap_or_default().to_string();
        let rest = tokens
            .get(1..)
            .unwrap_or(&[])
            .iter()
            .map(|t| PatternToken::Single((*t).to_string()))
            .collect();
        Self { first, rest }
    }

    pub fn matches_prefix(&self, cmd: &[String]) -> Option<Vec<String>> {
        let pattern_length = self.rest.len() + 1;
        if cmd.len() < pattern_length || cmd[0] != self.first {
            return None;
        }

        for (pattern_token, cmd_token) in self.rest.iter().zip(&cmd[1..pattern_length]) {
            if !pattern_token.matches(cmd_token) {
                return None;
            }
        }

        Some(cmd[..pattern_length].to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prefix_matches_a_longer_command_and_returns_the_matched_prefix() {
        let pattern = PrefixPattern::exact(&["git", "status"]);
        assert_eq!(
            pattern.matches_prefix(&["git".into(), "status".into(), "--short".into()]),
            Some(vec!["git".into(), "status".into()])
        );
        assert_eq!(pattern.matches_prefix(&["git".into()]), None);
        assert_eq!(pattern.matches_prefix(&["git".into(), "log".into()]), None);
    }

    #[test]
    fn an_alternatives_token_matches_any_listed_value() {
        let pattern = PrefixPattern {
            first: "git".into(),
            rest: vec![PatternToken::Alts(vec!["status".into(), "log".into()])],
        };
        assert!(
            pattern
                .matches_prefix(&["git".into(), "log".into()])
                .is_some()
        );
        assert!(
            pattern
                .matches_prefix(&["git".into(), "push".into()])
                .is_none()
        );
    }
}

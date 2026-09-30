//! Calls refused by policy, and how a worker reports what it lacks
//! (decision 0093).
//!
//! A worker whose call is refused by a policy (its write scope, the command
//! policy, a skill's `allowed-tools`, an unattended approval, a fence) used to
//! see only the refusal, and could then guess: a benchmark worker refused
//! `python3` wrote an empty answer that its vacuous check accepted. Two
//! things change here:
//!
//! - **The refusal says how to report it.** Its text ends with
//!   [`refusal_hint`]: stop and call [`SHORTFALL_TOOL`] (when the
//!   environment offers it) instead of working around the refusal.
//! - **The result lists the refusals.** [`RefusedCalls`] records each policy
//!   refusal by its capability ([`capability_of`]: the program for a shell
//!   call, the tool name otherwise) and whether a later call of the same
//!   capability ran without error. The orchestrator reports an attempt that
//!   finished with an unretried refusal as unverified.
//!
//! A refusal that is not a policy's (malformed input, a path outside the
//! worktree, an unknown tool) is not recorded: the call was wrong, not
//! forbidden, and retrying it correctly is the worker's own business.

use serde::{Deserialize, Serialize};

/// The worker tool that reports a shortfall to the orchestrator. Defined here
/// so the refusal text can name it; the orchestrator crate implements it.
pub const SHORTFALL_TOOL: &str = "report_shortfall";

/// One call a policy refused.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefusedCall {
    /// The tool called.
    pub tool: String,
    /// What was refused: the program (`argv[0]`) of a shell call, else the
    /// tool name ([`capability_of`]).
    pub capability: String,
    /// The refusal as the worker saw it, without the reporting hint.
    pub reason: String,
    /// Whether a later call of the same tool and capability ran without error.
    #[serde(default)]
    pub retried_ok: bool,
}

/// The capability a call exercises, for matching a refusal to a later retry:
/// a shell call's program, or the tool's name.
pub fn capability_of(tool: &str, input: &serde_json::Value) -> String {
    if tool == "shell"
        && let Some(program) = input
            .get("command")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first())
            .and_then(|p| p.as_str())
    {
        return program.to_owned();
    }
    tool.to_owned()
}

/// Whether a denial names a policy, not a malformed or misplaced call. The
/// approval chain's `access` step refuses invalid input and paths outside the
/// worktree; it is a policy only when it quotes the command policy.
pub(crate) fn is_policy_denial(by: &str, reason: &str) -> bool {
    match by {
        "registry" => false,
        "access" => reason.contains("forbidden by command policy"),
        _ => true,
    }
}

/// Appended to every policy refusal a worker sees.
pub fn refusal_hint(can_report: bool) -> &'static str {
    if can_report {
        " If your instruction cannot be done without this call, do not work around it or \
         guess an answer: call report_shortfall with kind \"permission\", naming the command \
         or tool you need and why, then stop."
    } else {
        " If your instruction cannot be done without this call, do not work around it or \
         guess an answer: stop, and say in your answer which command or tool you needed and \
         why."
    }
}

/// The policy refusals of one worker run, in order, and which were retried
/// successfully.
#[derive(Clone, Debug, Default)]
pub(crate) struct RefusedCalls {
    calls: Vec<RefusedCall>,
}

impl RefusedCalls {
    pub(crate) fn refused(&mut self, tool: &str, capability: String, reason: String) {
        self.calls.push(RefusedCall {
            tool: tool.to_owned(),
            capability,
            reason,
            retried_ok: false,
        });
    }

    /// A call of `tool` exercising `capability` ran without error: every
    /// earlier refusal of the same capability was retried successfully.
    pub(crate) fn ran_ok(&mut self, tool: &str, capability: &str) {
        for c in &mut self.calls {
            if c.tool == tool && c.capability == capability {
                c.retried_ok = true;
            }
        }
    }

    pub(crate) fn take(&mut self) -> Vec<RefusedCall> {
        std::mem::take(&mut self.calls)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shell_capability_is_its_program_and_another_tool_is_its_name() {
        let input = serde_json::json!({"command": ["python3", "-c", "print(1)"]});
        assert_eq!(capability_of("shell", &input), "python3");
        assert_eq!(capability_of("mcp__x__y", &input), "mcp__x__y");
        assert_eq!(capability_of("shell", &serde_json::json!({})), "shell");
    }

    #[test]
    fn only_a_policy_denial_counts() {
        assert!(is_policy_denial("ask-policy", "unattended"));
        assert!(is_policy_denial("skill:git", "not allowed"));
        assert!(is_policy_denial(
            "access",
            "tool `shell` failed: forbidden by command policy: no"
        ));
        assert!(!is_policy_denial(
            "access",
            "invalid input for tool `shell`"
        ));
        assert!(!is_policy_denial("registry", "no tool named `x`"));
    }

    #[test]
    fn a_later_successful_call_of_the_same_capability_retries_the_refusal() {
        let mut r = RefusedCalls::default();
        r.refused("shell", "python3".into(), "refused".into());
        r.refused("shell", "curl".into(), "refused".into());
        r.ran_ok("shell", "python3");
        let calls = r.take();
        assert!(calls[0].retried_ok);
        assert!(!calls[1].retried_ok);
    }
}

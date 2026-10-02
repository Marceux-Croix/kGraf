//! A warning, never a block: the user's own verify or gate command runs a
//! program that one of the user's deny or ask rules refuses to workers.
//!
//! kGraf runs the user's commands itself, outside the approval chain, so the
//! rules do not apply to them. A user who wrote `deny shell(python3*)` and then
//! passes `--verify "python3 check.py"` may expect the rule to cover the
//! command. This module finds those programs so a run can say what happens.
//! Nothing here changes a run: the command runs and the rule keeps binding
//! workers.
//!
//! A command is split the way a worker's command is: the gate runs it as
//! `sh -c SCRIPT`, and [`kgraf_tools::permissions::simple_commands`] splits a
//! script on `;`, `&&`, `||`, `|`, `&`, newlines and substitutions. Each part is
//! matched with [`kgraf_tools::permissions::hard_limit`], the function the
//! approval chain uses, so a part matches exactly when a worker's call of it
//! would be refused or asked about.

use kgraf_core::{AgentId, TaskId};
use kgraf_tools::approval::PendingCall;
use kgraf_tools::permissions::{
    HardLimit, PERMISSION_RULES_HOOK, RefusedSubject, UserRules, hard_limit, refused_subject,
    simple_commands,
};

/// One program of a user command that the user's rules refuse to workers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleConflict {
    /// What the command is called in the sentence: `verify` or `gate`.
    pub command: &'static str,
    pub program: String,
}

impl RuleConflict {
    /// The one line a run prints and traces. The wording is fixed.
    pub fn sentence(&self) -> String {
        let (c, p) = (self.command, &self.program);
        format!(
            "Your {c} command runs {p} and your rules block {p} for workers. kGraf will \
             continue to block workers from accessing {p} and will run your {c} command \
             directly, without AI."
        )
    }
}

/// The programs of `commands` (`(name, command line)` pairs, name `verify` or
/// `gate`) that `rules` deny or ask about. One entry per name and program, in
/// command order. No rules, no entries.
pub fn find(rules: &UserRules, commands: &[(&'static str, &str)]) -> Vec<RuleConflict> {
    let mut out: Vec<RuleConflict> = Vec::new();
    if rules.deny.is_empty() && rules.ask.is_empty() {
        return out;
    }
    for (name, line) in commands {
        let argv = vec!["sh".to_owned(), "-c".to_owned(), (*line).to_owned()];
        // Entry 0 is the wrapper shell kGraf starts, not a program of the
        // user's; the rest are the script's own commands.
        for words in simple_commands(&argv).into_iter().skip(1) {
            let input = serde_json::json!({ "command": words });
            let call = PendingCall {
                call_id: String::new(),
                tool: "shell".into(),
                input: input.clone(),
                worker: AgentId::new("user-command"),
                task: TaskId::new("user-command"),
            };
            // Only a rule counts. The same function also reports the
            // critical-deletion limit, which is not a rule of the user's.
            let Some(HardLimit::Deny(reason) | HardLimit::Ask(reason)) =
                hard_limit(rules, &call, None, None)
            else {
                continue;
            };
            if !(reason.starts_with("your deny rule `") || reason.starts_with("your ask rule `")) {
                continue;
            }
            let named = match refused_subject("shell", &input, PERMISSION_RULES_HOOK, &reason) {
                Some(RefusedSubject::Program(p)) => Some(p),
                _ => None,
            };
            let Some(program) = named.or_else(|| words.first().cloned()) else {
                continue;
            };
            let conflict = RuleConflict {
                command: name,
                program,
            };
            if !out.contains(&conflict) {
                out.push(conflict);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use kgraf_tools::permissions::Rule;

    fn rules(deny: &[&str], ask: &[&str]) -> UserRules {
        let parse = |v: &[&str]| v.iter().map(|r| Rule::parse(r).expect("rule")).collect();
        UserRules {
            deny: parse(deny),
            ask: parse(ask),
        }
    }

    fn programs(found: &[RuleConflict]) -> Vec<&str> {
        found.iter().map(|c| c.program.as_str()).collect()
    }

    #[test]
    fn the_sentence_is_the_owners_wording_with_the_program_substituted() {
        let c = RuleConflict {
            command: "verify",
            program: "python3".into(),
        };
        assert_eq!(
            c.sentence(),
            "Your verify command runs python3 and your rules block python3 for workers. kGraf \
             will continue to block workers from accessing python3 and will run your verify \
             command directly, without AI."
        );
    }

    #[test]
    fn a_denied_program_is_found_and_an_uncovered_one_is_not() {
        let r = rules(&["shell(python3*)"], &[]);
        assert_eq!(
            programs(&find(&r, &[("verify", "python3 check.py")])),
            ["python3"]
        );
        assert!(find(&r, &[("verify", "cargo test --workspace")]).is_empty());
        assert!(find(&UserRules::default(), &[("verify", "python3 check.py")]).is_empty());
    }

    #[test]
    fn a_chained_command_matches_on_its_second_part_and_each_part_is_named() {
        let r = rules(&["shell(python3*)"], &["shell(curl*)"]);
        assert_eq!(
            programs(&find(&r, &[("verify", "make build && python3 check.py")])),
            ["python3"]
        );
        assert_eq!(
            programs(&find(
                &r,
                &[(
                    "verify",
                    "echo hi; cargo test | tee out; curl x && python3 y"
                )]
            )),
            ["curl", "python3"]
        );
        // The same program twice is one line.
        assert_eq!(find(&r, &[("verify", "python3 a && python3 b")]).len(), 1);
    }

    #[test]
    fn a_rule_on_one_command_line_does_not_flag_another_use_of_the_program() {
        let r = rules(&["shell(git push*)"], &[]);
        assert!(find(&r, &[("verify", "git status && git diff")]).is_empty());
        assert_eq!(
            programs(&find(&r, &[("verify", "git push origin")])),
            ["git"]
        );
    }

    #[test]
    fn the_deletion_limit_is_not_a_rule_conflict() {
        // `rm -rf /` is refused by the deletion limit, not by a user rule.
        let r = rules(&["shell(python3*)"], &[]);
        assert!(find(&r, &[("verify", "rm -rf /")]).is_empty());
    }

    #[test]
    fn the_command_name_is_kept_per_command() {
        let r = rules(&["shell(python3*)"], &[]);
        let found = find(&r, &[("verify", "true"), ("gate", "python3 lint.py")]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].command, "gate");
    }
}

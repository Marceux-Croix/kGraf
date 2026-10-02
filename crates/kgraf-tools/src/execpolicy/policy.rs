//! kGraf's own policy container over the ported prefix matcher.
//!
//! **Not taken from codex-rs `execpolicy`.** Upstream's `Policy` loads rules from
//! a Starlark DSL (`policy.rs`, `parser.rs`, `amend.rs`, the `starlark` crate) so
//! an operator can author `.codexpolicy` files without recompiling. kGraf has
//! no such authoring surface yet (a later stream may add one); until then a
//! policy is built directly in Rust, so only the token-matching core
//! ([`super::rule::PrefixPattern`]) was worth porting. This module is the small
//! amount of new code that replaces the DSL: an ordered rule list plus a
//! default decision, matched first-match-wins like upstream's per-program rule
//! groups.
//!
//! **Read-only classification is kGraf's own extension.** Upstream's
//! `Decision` says whether a command may run; it says nothing about whether the
//! command writes anything, because codex's sandbox handles write confinement
//! separately. kGraf's shell tool needs that answer at `access()` time
//! (independent tool calls must overlap), so [`CommandRule`]
//! carries a `read_only` flag the policy author sets alongside the decision.

use super::build::{AllowlistMatch, BuildAllowlist};
use super::decision::Decision;
use super::rule::PrefixPattern;

/// One command-policy rule: match a prefix, then decide.
#[derive(Clone, Debug)]
pub struct CommandRule {
    pub pattern: PrefixPattern,
    pub decision: Decision,
    /// Whether a command matching this rule only reads: it declares no write
    /// access and so overlaps with every other read.
    pub read_only: bool,
    /// Shown to the user on a prompt or a refusal.
    pub reason: Option<String>,
}

/// The outcome of evaluating one command against a policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolicyVerdict {
    pub decision: Decision,
    pub read_only: bool,
    pub matched_prefix: Option<Vec<String>>,
    pub reason: Option<String>,
    /// On the build and test allowlist (decision 0084): allowed without
    /// approval and, from a workspace-bound shell tool, run as a sandboxed
    /// build that a worker with a declared write scope may issue.
    pub build: bool,
    /// Allowed by a grant: a program granted to this contract after a
    /// shortfall (decision 0093), one the user's permission mode grants, or
    /// any command in Bypass or when the run's container is the sandbox
    /// (decision 0094). From a
    /// workspace-bound shell tool it runs as a confined command, like a
    /// build, which a worker with a declared write scope may issue.
    pub granted: bool,
    /// For a shell script judged command by command (decision 0094, amended
    /// 2026-10-02) that was not granted: the command the mode did not grant,
    /// or why the script was not judged. The approval question quotes it.
    pub script_note: Option<String>,
}

/// An ordered set of [`CommandRule`]s, tried first match wins, falling back to
/// a default decision for a command nothing matched.
#[derive(Clone, Debug)]
pub struct ExecPolicy {
    rules: Vec<CommandRule>,
    default: Decision,
    /// Decision 0084: empty unless the caller sets one.
    build: BuildAllowlist,
    /// Decision 0093: programs (an exact `argv[0]`) granted to this
    /// contract after a shortfall, under the user's permission mode (0094).
    granted: std::collections::BTreeSet<String>,
    /// Decision 0093: the run's container is the sandbox, so every command
    /// that no `forbid` rule refuses runs without approval.
    unrestricted: bool,
    /// Decision 0094: the user's live permission mode, read on every
    /// evaluation, so a mode change reaches a running worker's next command.
    live: Option<LivePermissions>,
}

/// The live permission control, shared; `Debug` names only the mode.
#[derive(Clone)]
struct LivePermissions(std::sync::Arc<crate::permissions::PermissionControl>);

impl std::fmt::Debug for LivePermissions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LivePermissions({})", self.0.mode())
    }
}

impl ExecPolicy {
    /// An empty policy: every command gets `default` and is never read-only.
    pub fn new(default: Decision) -> Self {
        Self {
            rules: Vec::new(),
            default,
            build: BuildAllowlist::empty(),
            granted: std::collections::BTreeSet::new(),
            unrestricted: false,
            live: None,
        }
    }

    /// Read the user's permission mode on every evaluation (decision 0094):
    /// in Bypass every command no `forbid` rule refuses runs as a confined,
    /// granted command; in Auto the allow set's programs do; in Accept edits
    /// the common file programs do. The mode is read per call, so a change
    /// reaches a running worker's next command. The tool's description does
    /// not change with it: the shared prefix stays byte-identical (0043).
    pub fn with_permissions(
        mut self,
        control: std::sync::Arc<crate::permissions::PermissionControl>,
    ) -> Self {
        self.live = Some(LivePermissions(control));
        self
    }

    /// Record, on the live control, that the user's mode let `worker` run
    /// `argv`'s program (decision 0094: every grant is recorded). A verdict
    /// the mode did not decide (the allowlist, a contract's grant, a rule)
    /// records nothing.
    pub fn record_mode_grant(
        &self,
        worker: &str,
        call_id: &str,
        argv: &[String],
        verdict: &PolicyVerdict,
    ) {
        let Some(live) = &self.live else { return };
        let Some(program) = argv.first() else { return };
        if !verdict.granted || self.unrestricted || self.granted.contains(program) {
            return;
        }
        // A script granted command by command is a grant of each program the
        // mode granted, never of the shell that runs them.
        let programs: Vec<String> =
            if live.0.shell_unrestricted() || live.0.policy_grants_program(program).is_some() {
                vec![program.clone()]
            } else if let crate::permissions::Script::Commands(commands) =
                crate::permissions::script_commands(argv)
            {
                let mut out: Vec<String> = Vec::new();
                for words in commands {
                    let p = &words[0];
                    if !self.granted.contains(p)
                        && live.0.policy_grants_program(p).is_some()
                        && !out.contains(p)
                    {
                        out.push(p.clone());
                    }
                }
                out
            } else {
                Vec::new()
            };
        for p in programs {
            // Staged, not recorded: a later hook of the chain may refuse the
            // call, and the chain settles it (`PermissionControl::settle_call`).
            live.0
                .stage_grant(worker, call_id, &format!("`{p}`"), "mode");
        }
    }

    /// Allow these programs without approval (decision 0093): each is an
    /// exact `argv[0]`, granted to one contract after a shortfall under the
    /// user's permission mode (0094). A `forbid` rule and a refused build argument
    /// still win. Set from the run's configuration only, never by a model.
    pub fn with_granted_programs(mut self, programs: impl IntoIterator<Item = String>) -> Self {
        self.granted.extend(programs);
        self
    }

    /// The programs [`Self::with_granted_programs`] allowed.
    pub fn granted_programs(&self) -> &std::collections::BTreeSet<String> {
        &self.granted
    }

    /// Let every command that no `forbid` rule or refused build argument
    /// stops run without approval (decision 0093, "the container is the
    /// sandbox"). Only the operator turns this on, for a run whose commands
    /// execute in a disposable container.
    pub fn with_unrestricted_shell(mut self, on: bool) -> Self {
        self.unrestricted = on;
        self
    }

    /// Whether [`Self::with_unrestricted_shell`] is on.
    pub fn is_unrestricted(&self) -> bool {
        self.unrestricted
    }

    /// Allow these build and test commands without approval (decision 0084).
    /// A `forbid` rule still wins over the allowlist. A listed cargo
    /// subcommand whose arguments break its rules is `Forbidden`, with the
    /// reason naming the argument. A command not on the list is decided by the
    /// rules and the default exactly as before.
    pub fn with_build_allowlist(mut self, build: BuildAllowlist) -> Self {
        self.build = build;
        self
    }

    /// The build and test allowlist.
    pub fn build_allowlist(&self) -> &BuildAllowlist {
        &self.build
    }

    /// Add a rule, tried after every rule already added.
    pub fn with_rule(mut self, rule: CommandRule) -> Self {
        self.rules.push(rule);
        self
    }

    /// A read-only allow rule matching this exact argv prefix.
    pub fn allow_read_only(self, argv_prefix: &[&str], reason: impl Into<String>) -> Self {
        self.with_rule(CommandRule {
            pattern: PrefixPattern::exact(argv_prefix),
            decision: Decision::Allow,
            read_only: true,
            reason: Some(reason.into()),
        })
    }

    /// A forbidden rule matching this exact argv prefix.
    pub fn forbid(self, argv_prefix: &[&str], reason: impl Into<String>) -> Self {
        self.with_rule(CommandRule {
            pattern: PrefixPattern::exact(argv_prefix),
            decision: Decision::Forbidden,
            read_only: false,
            reason: Some(reason.into()),
        })
    }

    /// Evaluate one command's argv against every rule, in order, then the
    /// build and test allowlist ([`Self::with_build_allowlist`]).
    pub fn evaluate(&self, argv: &[String]) -> PolicyVerdict {
        let ruled = self.rules.iter().find_map(|rule| {
            rule.pattern
                .matches_prefix(argv)
                .map(|matched_prefix| PolicyVerdict {
                    decision: rule.decision,
                    read_only: rule.read_only,
                    matched_prefix: Some(matched_prefix),
                    reason: rule.reason.clone(),
                    build: false,
                    granted: false,
                    script_note: None,
                })
        });
        // A `forbid` rule wins over the allowlist.
        let ruled = match ruled {
            Some(v) if v.decision == Decision::Forbidden => return v,
            other => other,
        };
        match self.build.check(argv) {
            AllowlistMatch::Allowed => PolicyVerdict {
                decision: Decision::Allow,
                read_only: false,
                matched_prefix: None,
                reason: Some("on the build and test allowlist".into()),
                build: true,
                granted: false,
                script_note: None,
            },
            AllowlistMatch::Refused { reason } => PolicyVerdict {
                decision: Decision::Forbidden,
                read_only: false,
                matched_prefix: None,
                reason: Some(reason),
                build: false,
                granted: false,
                script_note: None,
            },
            AllowlistMatch::NotListed => {
                let from_rule = ruled.is_some();
                let verdict = ruled.unwrap_or(PolicyVerdict {
                    decision: self.default,
                    read_only: false,
                    matched_prefix: None,
                    reason: None,
                    build: false,
                    granted: false,
                    script_note: None,
                });
                // Decision 0093: a grant turns what would be asked about
                // (or allowed unconfined) into a confined allow. A read-only
                // command keeps its own classification and confinement.
                let listed = argv.first().is_some_and(|p| self.granted.contains(p));
                let live = self.live.as_ref();
                let bypass = live.is_some_and(|l| l.0.shell_unrestricted());
                let by_mode = argv
                    .first()
                    .and_then(|p| live.and_then(|l| l.0.policy_grants_program(p)));
                if verdict.read_only
                    || !(self.unrestricted || listed || bypass || by_mode.is_some())
                {
                    // A rule that named the whole call decides it; otherwise
                    // a script may be granted command by command.
                    if !from_rule
                        && !verdict.read_only
                        && let Some(scripted) = self.script_verdict(argv)
                    {
                        return scripted;
                    }
                    return verdict;
                }
                PolicyVerdict {
                    decision: Decision::Allow,
                    read_only: false,
                    matched_prefix: verdict.matched_prefix,
                    reason: Some(if listed {
                        "granted to this contract under the permission mode".into()
                    } else if let Some(why) = by_mode {
                        why.into()
                    } else if bypass {
                        "the Bypass permission mode".into()
                    } else {
                        "the run's container is the sandbox".into()
                    }),
                    build: false,
                    granted: true,
                    script_note: None,
                }
            }
        }
    }

    /// Whether a grant lets `program` run without approval: granted to this
    /// contract, or by the user's live mode (Auto's allow set, Accept edits'
    /// file programs).
    fn grants_program(&self, program: &str) -> bool {
        self.granted.contains(program)
            || self
                .live
                .as_ref()
                .is_some_and(|l| l.0.policy_grants_program(program).is_some())
    }

    /// Decision 0094, amended 2026-10-02: a `sh -c` script of simple
    /// commands ([`crate::permissions::script_commands`]) is granted when a
    /// grant covers the program of every command, as Claude Code allows a
    /// compound command when each subcommand is allowed. A command a
    /// `forbid` rule or the build allowlist refuses refuses the whole call. A
    /// command no grant covers, or a script with a construct that is not
    /// judged, leaves the call to be asked, and the note says which. `None`
    /// when this is not a script, or when nothing grants named programs (the
    /// call is asked as before).
    ///
    /// The user's deny and ask rules are not read here: their hook runs after
    /// this one on every command of the script, in every mode.
    fn script_verdict(&self, argv: &[String]) -> Option<PolicyVerdict> {
        use crate::permissions::{SCRIPT_RUNS_LEAD, Script, script_commands};
        let grants_any =
            !self.granted.is_empty() || self.live.as_ref().is_some_and(|l| l.0.grants_programs());
        if !grants_any {
            return None;
        }
        let asked = |note: String| PolicyVerdict {
            decision: Decision::Prompt,
            read_only: false,
            matched_prefix: None,
            reason: None,
            build: false,
            granted: false,
            script_note: Some(note),
        };
        let commands = match script_commands(argv) {
            Script::NotAScript => return None,
            Script::Unjudged(why) => {
                return Some(asked(format!(
                    "Its script is not judged command by command, so no grant covers it: {why}."
                )));
            }
            Script::Commands(commands) => commands,
        };
        let shown = |words: &[String]| crate::quote_argv(words);
        for words in &commands {
            let v = self.evaluate(words);
            if v.decision == Decision::Forbidden {
                return Some(PolicyVerdict {
                    decision: Decision::Forbidden,
                    read_only: false,
                    matched_prefix: None,
                    reason: Some(format!(
                        "the {SCRIPT_RUNS_LEAD}{}` (`{}`), which is refused: {}",
                        words[0],
                        shown(words),
                        v.reason.as_deref().unwrap_or("forbidden by command policy")
                    )),
                    build: false,
                    granted: false,
                    script_note: None,
                });
            }
        }
        if let Some(words) = commands.iter().find(|w| !self.grants_program(&w[0])) {
            return Some(asked(format!(
                "The {SCRIPT_RUNS_LEAD}{}` (`{}`), which no grant covers; a script runs without \
                 asking only when every command in it is granted.",
                words[0],
                shown(words)
            )));
        }
        let mut programs: Vec<&str> = Vec::new();
        for words in &commands {
            if !programs.contains(&words[0].as_str()) {
                programs.push(&words[0]);
            }
        }
        Some(PolicyVerdict {
            decision: Decision::Allow,
            read_only: false,
            matched_prefix: None,
            reason: Some(format!(
                "every command of the script is granted ({})",
                programs
                    .iter()
                    .map(|p| format!("`{p}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
            build: false,
            granted: true,
            script_note: None,
        })
    }

    /// A starter policy: common read-only git and filesystem inspection
    /// commands are allowed without a prompt; two frequently-cited exact argv
    /// shapes are refused outright with no prompt at all. Everything else
    /// falls to `Prompt`. Since [`crate::ShellApprovalHook`]
    /// turns a `Prompt` verdict into an `Ask`, that
    /// means "requires approval," not "runs unrestricted."
    ///
    /// **The two `forbid` rules below are not "protection" against the shapes
    /// they name; they are a stronger refusal for those two exact shapes,
    /// full stop, no override.** `forbid` matches an exact argv prefix, so a
    /// trivially different spelling of the same command does not match either rule. Examples are
    /// `rm -fr /` (flag order), `/bin/rm -rf /` (absolute path), and `git push -f` (short flag).
    /// Without the hook, those variants would run
    /// completely unapproved, and the two `forbid` rules gave a false sense
    /// that "the dangerous commands" were covered. Now, an unmatched variant
    /// falls to `Prompt` like everything else and requires approval, which is
    /// the property actually doing the work here; `forbid` is reserved for a
    /// shape an operator wants refused even if a human approver is asked and
    /// says yes. A real deployment replaces this list (it is a
    /// starting point, not a substitute for the operator's own rules).
    ///
    /// **`git` is not on the read-only allow list.** `apply_patch` used to be able to rewrite `.git`
    /// (the gitlink, `.git/config`, a hook). [`crate::fs_safety`] now refuses
    /// that outright, but a reproduction also showed the
    /// other half of the chain: an *approval-free*, read-only-classified
    /// `git status` will still read and act on whatever `.git/config` already
    /// says, including a planted `core.fsmonitor` that runs on every
    /// invocation. Two defenses were possible: keep `git` allow-listed and
    /// have [`crate::ShellTool`] inject safe overrides on every invocation
    /// (`-c core.fsmonitor= -c core.hooksPath=/dev/null`,
    /// `GIT_CONFIG_NOSYSTEM=1`, `GIT_CONFIG_GLOBAL=/dev/null`), or require
    /// approval for every `git` call so a human sees what is about to run.
    /// **Both are applied**: `ShellTool` always injects the safe overrides
    /// (defense in depth against config vectors an override list does not
    /// cover, such as `include.path`), and `git` is *also* removed from this
    /// list, so it falls to `Prompt` like an unclassified command. Neither
    /// alone is airtight, and running unapproved was the actual defect.
    pub fn default_kgraf_policy() -> Self {
        Self::new(Decision::Prompt)
            .forbid(&["rm", "-rf", "/"], "refuses to remove the filesystem root")
            .forbid(
                &["git", "push", "--force"],
                "a force push can discard remote history",
            )
            .allow_read_only(&["ls"], "read-only")
            .allow_read_only(&["pwd"], "read-only")
            .allow_read_only(&["cat"], "read-only")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_string()).collect()
    }

    #[test]
    fn first_match_wins_over_a_later_broader_rule() {
        let policy = ExecPolicy::new(Decision::Prompt)
            .forbid(&["git", "push", "--force"], "no")
            .allow_read_only(&["git"], "shadowed on purpose for this test");
        let verdict = policy.evaluate(&argv(&["git", "push", "--force", "origin"]));
        assert_eq!(verdict.decision, Decision::Forbidden);
    }

    #[test]
    fn an_unmatched_command_gets_the_default_and_is_never_read_only() {
        let policy = ExecPolicy::new(Decision::Prompt);
        let verdict = policy.evaluate(&argv(&["curl", "https://example.com"]));
        assert_eq!(verdict.decision, Decision::Prompt);
        assert!(!verdict.read_only);
        assert_eq!(verdict.matched_prefix, None);
    }

    #[test]
    fn the_default_policy_requires_approval_for_git_status_and_forbids_a_force_push() {
        // `git` is not read-only allow-listed,
        // even `git status`, see `default_kgraf_policy`'s doc comment.
        let policy = ExecPolicy::default_kgraf_policy();
        let status = policy.evaluate(&argv(&["git", "status"]));
        assert_eq!(status.decision, Decision::Prompt);
        assert!(!status.read_only);

        let force_push = policy.evaluate(&argv(&["git", "push", "--force", "origin", "main"]));
        assert_eq!(force_push.decision, Decision::Forbidden);
    }

    /// Decision 0084: an allowlisted command is `Allow` and marked a build; a
    /// listed cargo subcommand with a smuggled argument is `Forbidden`, naming
    /// it; a `forbid` rule wins over the allowlist; a command off the list is
    /// decided exactly as without one.
    #[test]
    fn the_build_allowlist_is_consulted_after_forbid_rules_and_before_the_default() {
        let policy = ExecPolicy::default_kgraf_policy()
            .forbid(&["cargo", "test", "--release"], "not here")
            .with_build_allowlist(super::super::BuildAllowlist::cargo_default());
        let ok = policy.evaluate(&argv(&["cargo", "test", "--workspace"]));
        assert_eq!(ok.decision, Decision::Allow);
        assert!(ok.build && !ok.read_only);

        let smuggled = policy.evaluate(&argv(&["cargo", "build", "--config", "x=1"]));
        assert_eq!(smuggled.decision, Decision::Forbidden);
        assert!(!smuggled.build);
        assert!(smuggled.reason.expect("reason").contains("--config"));

        let forbidden = policy.evaluate(&argv(&["cargo", "test", "--release"]));
        assert_eq!(forbidden.decision, Decision::Forbidden);
        assert_eq!(forbidden.reason.as_deref(), Some("not here"));

        for off in [vec!["cargo", "run"], vec!["touch", "x"]] {
            let v = policy.evaluate(&argv(&off));
            assert_eq!(v.decision, Decision::Prompt, "{off:?}");
            assert!(!v.build);
        }
        let ls = policy.evaluate(&argv(&["ls"]));
        assert!(ls.read_only && !ls.build);

        // Without an allowlist, nothing is a build.
        let bare = ExecPolicy::default_kgraf_policy().evaluate(&argv(&["cargo", "build"]));
        assert_eq!(bare.decision, Decision::Prompt);
        assert!(!bare.build);
    }

    /// A trivially different spelling of a `forbid`
    /// rule's exact shape does not match it, and must fall to `Prompt` (which,
    /// with the shell approval hook installed, means "requires approval") rather than to
    /// `Allow`. The `forbid` rules are not a spelling-proof blocklist; nothing
    /// here claims they are.
    #[test]
    fn a_different_spelling_of_a_forbidden_shape_falls_to_prompt_not_allow() {
        let policy = ExecPolicy::default_kgraf_policy();
        for variant in [
            vec!["rm", "-fr", "/"],
            vec!["/bin/rm", "-rf", "/"],
            vec!["git", "push", "-f"],
            vec!["sh", "-c", "rm -rf /"],
        ] {
            let verdict = policy.evaluate(&argv(&variant));
            assert_eq!(
                verdict.decision,
                Decision::Prompt,
                "{variant:?} must require approval, not run as Allow or as a covered Forbidden"
            );
        }
    }

    /// Decision 0093: a granted program runs without approval, marked
    /// granted; another program is still asked about; a `forbid` rule still
    /// wins over a grant; the match is on the exact `argv[0]`.
    #[test]
    fn a_granted_program_is_allowed_and_nothing_else_changes() {
        let policy = ExecPolicy::default_kgraf_policy()
            .with_granted_programs(["python3".to_owned(), "rm".to_owned()]);
        let granted = policy.evaluate(&argv(&["python3", "fetch.py"]));
        assert_eq!(granted.decision, Decision::Allow);
        assert!(granted.granted && !granted.build && !granted.read_only);
        let other = policy.evaluate(&argv(&["curl", "https://example.com"]));
        assert_eq!(other.decision, Decision::Prompt);
        assert!(!other.granted);
        let spelled = policy.evaluate(&argv(&["/usr/bin/python3", "fetch.py"]));
        assert_eq!(spelled.decision, Decision::Prompt, "exact argv[0] only");
        let forbidden = policy.evaluate(&argv(&["rm", "-rf", "/"]));
        assert_eq!(forbidden.decision, Decision::Forbidden);
        let read_only = policy.evaluate(&argv(&["ls"]));
        assert!(read_only.read_only && !read_only.granted);
    }

    /// Decision 0094: the policy reads the live mode on every evaluation. One
    /// policy value, built once, answers differently after each mode change:
    /// Manual asks, Auto grants its allow set, Accept edits grants the file
    /// programs, Bypass grants everything, and a `forbid` rule wins in all.
    #[test]
    fn the_live_mode_changes_the_verdict_of_a_policy_already_built() {
        use crate::permissions::{AllowSet, ModeSource, PermissionControl, PermissionMode};
        let control = std::sync::Arc::new(PermissionControl::manual());
        let policy = ExecPolicy::default_kgraf_policy().with_permissions(control.clone());
        let py = argv(&["python3", "fetch.py"]);
        let mk = argv(&["mkdir", "out"]);
        assert_eq!(policy.evaluate(&py).decision, Decision::Prompt);
        control.set_allow(
            AllowSet::default()
                .with_programs(["python3"])
                .expect("allow"),
        );
        control
            .set_mode(PermissionMode::Auto, ModeSource::CommandLine)
            .expect("auto");
        let v = policy.evaluate(&py);
        assert!(v.decision == Decision::Allow && v.granted, "{v:?}");
        assert_eq!(policy.evaluate(&mk).decision, Decision::Prompt);
        control
            .set_mode(PermissionMode::AcceptEdits, ModeSource::CommandLine)
            .expect("edits");
        assert_eq!(policy.evaluate(&py).decision, Decision::Prompt);
        assert!(policy.evaluate(&mk).granted);
        control
            .set_mode(PermissionMode::Bypass, ModeSource::CommandLine)
            .expect("bypass");
        let v = policy.evaluate(&argv(&["curl", "https://example.com"]));
        assert!(v.decision == Decision::Allow && v.granted, "{v:?}");
        assert_eq!(
            policy.evaluate(&argv(&["git", "push", "--force"])).decision,
            Decision::Forbidden
        );
        control
            .set_mode(PermissionMode::Manual, ModeSource::UserInterface)
            .expect("manual");
        assert_eq!(policy.evaluate(&py).decision, Decision::Prompt);
    }

    /// Decision 0093: with the container as the sandbox, any command runs
    /// without approval, marked granted; a `forbid` rule still wins; off, the
    /// same command is asked about.
    #[test]
    fn an_unrestricted_shell_allows_every_command_but_a_forbidden_one() {
        let on = ExecPolicy::default_kgraf_policy().with_unrestricted_shell(true);
        let any = on.evaluate(&argv(&["curl", "https://example.com"]));
        assert_eq!(any.decision, Decision::Allow);
        assert!(any.granted);
        assert_eq!(
            on.evaluate(&argv(&["git", "push", "--force"])).decision,
            Decision::Forbidden
        );
        let off = ExecPolicy::default_kgraf_policy();
        assert_eq!(
            off.evaluate(&argv(&["curl", "https://example.com"]))
                .decision,
            Decision::Prompt
        );
    }

    fn control_in(
        mode: crate::permissions::PermissionMode,
        programs: &[&str],
    ) -> std::sync::Arc<crate::permissions::PermissionControl> {
        use crate::permissions::{AllowSet, ModeSource, PermissionControl};
        let control = std::sync::Arc::new(PermissionControl::manual());
        control.set_allow(
            AllowSet::default()
                .with_programs(programs.iter().copied())
                .expect("allow"),
        );
        control
            .set_mode(mode, ModeSource::CommandLine)
            .expect("mode");
        control
    }

    fn sh(script: &str) -> Vec<String> {
        argv(&["sh", "-c", script])
    }

    /// The question the real shell hook asks about `argv` under `policy`.
    fn question(policy: &ExecPolicy, argv: &[String]) -> String {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(asked(policy, argv))
    }

    async fn asked(policy: &ExecPolicy, argv: &[String]) -> String {
        let hook = crate::ShellApprovalHook::new(policy.clone());
        let call = crate::approval::PendingCall {
            call_id: "c1".into(),
            tool: "shell".into(),
            input: serde_json::json!({ "command": argv }),
            worker: "w1".into(),
            task: "t1".into(),
        };
        match crate::approval::ApprovalHook::review(&hook, &call).await {
            crate::approval::HookDecision::Ask { question } => question,
            other => panic!("expected an ask for {argv:?}, got {other:?}"),
        }
    }

    /// Decision 0094, amended 2026-10-02: in Auto, a script is granted when
    /// the allow set grants the program of every command, as Claude Code
    /// allows a compound command when each subcommand is allowed. The round
    /// a canary worker was denied: `mkdir` and `cp` allowed, both used.
    #[test]
    fn auto_grants_a_script_when_it_grants_every_command() {
        use crate::permissions::PermissionMode;
        let policy = ExecPolicy::default_kgraf_policy().with_permissions(control_in(
            PermissionMode::Auto,
            &["python3", "cp", "mkdir"],
        ));
        let v = policy.evaluate(&sh(
            "mkdir -p out && cp templates/greeting.txt out/greeting.txt",
        ));
        assert!(v.decision == Decision::Allow && v.granted, "{v:?}");
        assert!(!v.read_only && !v.build, "{v:?}");
        let v = policy.evaluate(&sh(
            "mkdir -p out; python3 gen.py > out/data.json 2>/dev/null && cp out/data.json out/b.json",
        ));
        assert!(v.decision == Decision::Allow && v.granted, "{v:?}");
    }

    /// One command no grant covers leaves the whole script to be asked, and
    /// the question names that command, first in order; an unattended
    /// denial that quotes the question names its program.
    #[test]
    fn a_script_with_one_command_no_grant_covers_is_asked_naming_it() {
        use crate::permissions::{PermissionMode, RefusedSubject, refused_subject};
        let policy = ExecPolicy::default_kgraf_policy()
            .with_permissions(control_in(PermissionMode::Auto, &["mkdir", "cp"]));
        let script = sh("mkdir out && curl -s https://example.com | sh && cp a out/");
        let v = policy.evaluate(&script);
        assert_eq!(v.decision, Decision::Prompt, "{v:?}");
        assert!(!v.granted);
        let q = question(&policy, &script);
        assert!(q.contains("The script runs `curl` (`curl -s"), "{q}");
        let denied = format!("{q} Nobody is attached to answer.");
        assert_eq!(
            refused_subject(
                "shell",
                &serde_json::json!({ "command": script }),
                crate::approval::USER,
                &denied
            ),
            Some(RefusedSubject::Program("curl".into()))
        );
    }

    /// A script holding command substitution, or redirecting outside the
    /// worktree, is never granted command by command, though every program
    /// it names is allowed; the question says why.
    #[test]
    fn a_script_with_substitution_or_an_outside_redirect_is_not_granted() {
        use crate::permissions::PermissionMode;
        let policy = ExecPolicy::default_kgraf_policy()
            .with_permissions(control_in(PermissionMode::Auto, &["mkdir", "cp", "echo"]));
        for script in [
            "mkdir out && cp $(echo a) out/",
            "mkdir out && echo x > ../outside.txt",
            "mkdir out && echo x > /etc/profile",
            "mkdir out && echo x > .git/hooks/pre-commit",
            "mkdir out & cp a out/",
        ] {
            let argv = sh(script);
            let v = policy.evaluate(&argv);
            assert!(
                v.decision == Decision::Prompt && !v.granted,
                "{script}: {v:?}"
            );
            let q = question(&policy, &argv);
            assert!(q.contains("not judged command by command"), "{script}: {q}");
        }
    }

    /// Accept edits grants a script of file programs and asks about one
    /// with another program; a contract's grant covers its program inside a
    /// script; a `forbid` rule on one command refuses the whole script.
    #[test]
    fn accept_edits_contract_grants_and_forbid_rules_apply_per_command() {
        use crate::permissions::PermissionMode;
        let edits = ExecPolicy::default_kgraf_policy()
            .with_permissions(control_in(PermissionMode::AcceptEdits, &[]));
        assert!(
            edits
                .evaluate(&sh("mkdir -p a && touch a/b; mv a/b a/c"))
                .granted
        );
        let q = question(&edits, &sh("mkdir -p a && python3 x.py"));
        assert!(q.contains("The script runs `python3`"), "{q}");
        // A program granted to the contract after a shortfall counts, with
        // or without a live mode.
        let granted = ExecPolicy::default_kgraf_policy().with_granted_programs(["python3".into()]);
        assert!(
            granted
                .evaluate(&sh("python3 a.py && python3 b.py"))
                .granted
        );
        let q = question(&granted, &sh("python3 a.py && mkdir out"));
        assert!(q.contains("The script runs `mkdir`"), "{q}");
        let both = ExecPolicy::default_kgraf_policy()
            .with_granted_programs(["python3".into()])
            .with_permissions(control_in(PermissionMode::Auto, &["mkdir"]));
        assert!(both.evaluate(&sh("mkdir out && python3 a.py")).granted);
        // `git` allowed, but `git push --force` is forbidden: the script is
        // refused, naming the command.
        let git = ExecPolicy::default_kgraf_policy()
            .with_permissions(control_in(PermissionMode::Auto, &["git", "mkdir"]));
        let v = git.evaluate(&sh("mkdir x && git push --force origin main"));
        assert_eq!(v.decision, Decision::Forbidden, "{v:?}");
        let reason = v.reason.expect("reason");
        assert!(
            reason.contains("the script runs `git` (`git push --force"),
            "{reason}"
        );
    }

    /// Where nothing grants named programs the script is asked about as
    /// before, with no note; Bypass grants it whole, judged or not.
    #[test]
    fn manual_and_bypass_are_unchanged_for_scripts() {
        use crate::permissions::PermissionMode;
        let manual = ExecPolicy::default_kgraf_policy()
            .with_permissions(control_in(PermissionMode::Manual, &["mkdir"]));
        let v = manual.evaluate(&sh("mkdir out && cp a out/"));
        assert_eq!(v.decision, Decision::Prompt);
        assert_eq!(v.script_note, None);
        let q = question(&manual, &sh("mkdir out && cp a out/"));
        assert!(!q.contains("script"), "{q}");
        // Auto with an empty allow set grants no program either.
        let empty = ExecPolicy::default_kgraf_policy()
            .with_permissions(control_in(PermissionMode::Auto, &[]));
        assert_eq!(empty.evaluate(&sh("mkdir out")).script_note, None);
        let bypass = ExecPolicy::default_kgraf_policy()
            .with_permissions(control_in(PermissionMode::Bypass, &[]));
        let v = bypass.evaluate(&sh("mkdir out && cp $(ls) out/"));
        assert!(v.decision == Decision::Allow && v.granted, "{v:?}");
    }
}

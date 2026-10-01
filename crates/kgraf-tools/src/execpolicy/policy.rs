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
    pub fn record_mode_grant(&self, worker: &str, argv: &[String], verdict: &PolicyVerdict) {
        let Some(live) = &self.live else { return };
        let Some(program) = argv.first() else { return };
        if !verdict.granted || self.unrestricted || self.granted.contains(program) {
            return;
        }
        if live.0.shell_unrestricted() || live.0.policy_grants_program(program).is_some() {
            live.0.record_grant(worker, &format!("`{program}`"), "mode");
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
            },
            AllowlistMatch::Refused { reason } => PolicyVerdict {
                decision: Decision::Forbidden,
                read_only: false,
                matched_prefix: None,
                reason: Some(reason),
                build: false,
                granted: false,
            },
            AllowlistMatch::NotListed => {
                let verdict = ruled.unwrap_or(PolicyVerdict {
                    decision: self.default,
                    read_only: false,
                    matched_prefix: None,
                    reason: None,
                    build: false,
                    granted: false,
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
                }
            }
        }
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
}

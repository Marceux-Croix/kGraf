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
    /// Allowed as a script judged command by command (decision 0094,
    /// amended 2026-10-02): the shell tool checks the script's `cd` target
    /// and its allowlisted cargo commands on disk before it runs
    /// (decision 0084's checks, made in the directory each command runs in).
    pub script_checked: bool,
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
    /// Decision 0087, amended 2026-10-09: the commands run in a disposable
    /// remote environment, so a package download is the task's own and is
    /// decided like any command. Off, a download is refused
    /// ([`super::package_fetch`]).
    package_downloads: bool,
    /// Decision 0094, amended 2026-10-06: where the files a command's
    /// programs run (a recipe, a script) are read from. None by default, so
    /// every such command is granted by nothing.
    reading: crate::permissions::ReadContext,
    /// The files each command's review read, with a hash of each, keyed by
    /// the command line. Shared by the shell tool and its approval hook (one
    /// policy, cloned), so the tool refuses to run a command whose files
    /// changed after it was judged ([`Self::files_changed_since_review`]).
    reviewed: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, Witness>>>,
}

/// The files a review read, each with a hash of its text.
type Witness = Vec<(std::path::PathBuf, blake3::Hash)>;

/// How many reviewed command lines a policy remembers before it starts over.
const MAX_REVIEWED: usize = 1_024;

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
            package_downloads: false,
            reading: crate::permissions::ReadContext::default(),
            reviewed: std::sync::Arc::default(),
        }
    }

    /// Note the files the review of `argv` read (decision 0094, amended
    /// 2026-10-06). Called by the approval hook when it judges a command, so
    /// the tool can tell, when the command runs, whether another call of the
    /// round rewrote what the judgment rested on.
    pub fn note_reviewed(&self, argv: &[String]) {
        if self.reading.root.is_none() {
            return;
        }
        let seen = self.reading.witnessed(argv);
        let mut reviewed = self.reviewed.lock().unwrap_or_else(|p| p.into_inner());
        if reviewed.len() >= MAX_REVIEWED {
            reviewed.clear();
        }
        reviewed.insert(crate::quote_argv(argv), seen);
    }

    /// Why `argv` must not run now: a file its review read (a recipe, a
    /// script) has changed since, or one appeared or went away, so the
    /// judgment no longer holds (decision 0094, amended 2026-10-06). `None`
    /// when nothing changed, or the command was never reviewed here.
    pub fn files_changed_since_review(&self, argv: &[String]) -> Option<String> {
        self.reading.root.as_ref()?;
        let before = self
            .reviewed
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&crate::quote_argv(argv))
            .cloned()?;
        let now = self.reading.witnessed(argv);
        if now == before {
            return None;
        }
        let changed: Vec<String> = before
            .iter()
            .chain(now.iter())
            .filter(|f| !(before.contains(f) && now.contains(f)))
            .map(|(p, _)| {
                let shown = self
                    .reading
                    .root
                    .as_deref()
                    .and_then(|r| p.strip_prefix(r).ok())
                    .unwrap_or(p);
                format!("`{}`", shown.display())
            })
            .fold(Vec::new(), |mut acc, f| {
                if !acc.contains(&f) {
                    acc.push(f);
                }
                acc
            });
        Some(format!(
            "the files this command runs changed after it was checked ({}); run it again to have \
             it checked as it is now",
            changed.join(", ")
        ))
    }

    /// Read the files a command's programs run (a Makefile recipe, a
    /// `package.json` script, a script given to an interpreter) from the
    /// worktree `root`, where the shell runs its commands (decision 0094,
    /// amended 2026-10-06). A grant covers such a command only when it
    /// covers every program the file makes it run. The user's home
    /// directory, for git's configuration, is read from `HOME`.
    pub fn with_working_dir(self, root: &std::path::Path) -> Self {
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        self.with_read_context(crate::permissions::ReadContext::in_worktree(
            root,
            home.as_deref(),
        ))
    }

    /// [`Self::with_working_dir`] with every directory given (tests).
    pub fn with_read_context(mut self, reading: crate::permissions::ReadContext) -> Self {
        self.reading = reading;
        self
    }

    /// Read the user's permission mode on every evaluation (decision 0094):
    /// in Bypass every command no `forbid` rule refuses runs as a confined,
    /// granted command; in Auto the allow set's programs do; in Accept edits
    /// the common file programs do. The mode is read per call, so a change
    /// reaches a running worker's next command. The shell's description,
    /// built when a worker is assembled, says whether every command runs
    /// without approval ([`Self::runs_any_command`]); the mode is run-wide,
    /// so every worker assembled under one mode shares one prefix (0043).
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
        // A command is a grant of every program it runs that the mode
        // granted (decision 0094, amended 2026-10-06).
        let mode_granted = |words: &[String], out: &mut Vec<String>| {
            for p in self.granted_by_grants(words).unwrap_or_default() {
                if !self.granted.contains(&p)
                    && live.0.policy_grants_program(&p).is_some()
                    && !out.contains(&p)
                {
                    out.push(p);
                }
            }
        };
        let mut programs: Vec<String> = Vec::new();
        if live.0.shell_unrestricted() {
            programs.push(program.clone());
        } else if self.granted_by_grants(argv).is_some() {
            mode_granted(argv, &mut programs);
        } else if let crate::permissions::Script::Commands(commands) =
            crate::permissions::script_commands_with(argv, &self.script_constructs())
        {
            for words in commands {
                mode_granted(&words, &mut programs);
            }
        }
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

    /// Let a command that downloads packages (`pip install`, `npm ci`,
    /// `cargo fetch`) be decided like any other (decision 0087, amended
    /// 2026-10-09). Only for a run whose commands execute in a disposable
    /// remote environment, never on the user's machine: there, the host
    /// fetches dependencies with the project's consent, and a worker's
    /// download is refused in every mode. An unrestricted shell
    /// ([`Self::with_unrestricted_shell`]) allows downloads too.
    pub fn with_package_downloads(mut self, on: bool) -> Self {
        self.package_downloads = on;
        self
    }

    /// Whether [`Self::with_unrestricted_shell`] is on.
    pub fn is_unrestricted(&self) -> bool {
        self.unrestricted
    }

    /// Whether every command no `forbid` rule refuses runs without approval
    /// right now: the shell is unrestricted, or the user's live mode is
    /// Bypass (decision 0094). Both are run-wide, so a description built from
    /// this is the same for every worker assembled under one mode (0043).
    pub fn runs_any_command(&self) -> bool {
        self.unrestricted || self.live.as_ref().is_some_and(|l| l.0.shell_unrestricted())
    }

    /// Whether a command that downloads packages is decided like any other
    /// ([`Self::with_package_downloads`], or an unrestricted shell).
    pub fn allows_package_downloads(&self) -> bool {
        self.package_downloads || self.unrestricted
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

    /// The live permission mode's own program grants, as one sentence
    /// ([`crate::permissions::PermissionControl::mode_program_grants`]).
    /// `None` without a live mode or when the mode grants no program. In
    /// Bypass, or with the shell unrestricted, nothing is asked, so no
    /// question carries it.
    pub fn mode_program_grants(&self) -> Option<String> {
        self.live.as_ref().and_then(|l| l.0.mode_program_grants())
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
                    script_checked: false,
                })
        });
        // A `forbid` rule wins over the allowlist.
        let ruled = match ruled {
            Some(v) if v.decision == Decision::Forbidden => return v,
            other => other,
        };
        // A `forbid` rule also refuses its shape after prefix words
        // (`env rm -rf /`, `command git push --force`) and in every command
        // the call starts (`sh -c`, `find -exec`, `ssh HOST CMD`), found by
        // the same functions the user's rules and the deletion limit use. Only
        // refusals are matched there: an allow rule keeps naming the exact
        // argv it was written for.
        if let Some(v) = self.forbidden_after_prefix(argv) {
            return v;
        }
        match self.build.check(argv) {
            AllowlistMatch::Allowed => PolicyVerdict {
                decision: Decision::Allow,
                read_only: false,
                matched_prefix: None,
                reason: Some("on the build and test allowlist".into()),
                build: true,
                granted: false,
                script_note: None,
                script_checked: false,
            },
            AllowlistMatch::Refused { reason } => PolicyVerdict {
                decision: Decision::Forbidden,
                read_only: false,
                matched_prefix: None,
                reason: Some(reason),
                build: false,
                granted: false,
                script_note: None,
                script_checked: false,
            },
            AllowlistMatch::NotListed => {
                // Decision 0087, amended 2026-10-09: a package download is
                // refused before any grant is read, so no mode, allow set or
                // contract grant turns it into a confined command. An
                // allowlisted build (the user's own configured commands
                // included) is not read for one: it runs with the network
                // off and is the user's choice.
                if let Some(v) = self.package_download(argv) {
                    return v;
                }
                let from_rule = ruled.is_some();
                let verdict = ruled.unwrap_or(PolicyVerdict {
                    decision: self.default,
                    read_only: false,
                    matched_prefix: None,
                    reason: None,
                    build: false,
                    granted: false,
                    script_note: None,
                    script_checked: false,
                });
                // Decision 0093: a grant turns what would be asked about
                // (or allowed unconfined) into a confined allow. A read-only
                // command keeps its own classification and confinement.
                // Every program the command runs needs a grant, from the
                // contract or the mode: a grant of `env`, `xargs` or `sh`
                // covers nothing they run (decision 0094, amended
                // 2026-10-06).
                let needed = self.granted_by_grants(argv);
                let listed = needed
                    .as_ref()
                    .is_some_and(|ps| ps.iter().all(|p| self.granted.contains(p)));
                let live = self.live.as_ref();
                let bypass = live.is_some_and(|l| l.0.shell_unrestricted());
                let by_mode = needed.as_ref().and_then(|ps| {
                    ps.iter()
                        .find(|p| !self.granted.contains(*p))
                        .and_then(|p| live.and_then(|l| l.0.policy_grants_program(p)))
                });
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
                    let grants_any =
                        !self.granted.is_empty() || live.is_some_and(|l| l.0.grants_programs());
                    if !from_rule && !verdict.read_only && grants_any {
                        return PolicyVerdict {
                            script_note: self.lacking_note(argv),
                            ..verdict
                        };
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
                    script_checked: false,
                }
            }
        }
    }

    /// The first `forbid` rule matching `argv` from a place after its prefix
    /// words where its program may stand
    /// ([`crate::permissions::command_starts`]).
    fn forbidden_after_prefix(&self, argv: &[String]) -> Option<PolicyVerdict> {
        // Every command the call runs (decision 0094, amended 2026-10-06):
        // a script's commands and what a program runs from its arguments
        // (`sh -c 'git push --force'`, `find -exec`, `ssh HOST CMD`), each
        // from every place its program may stand. The call's own first word
        // was matched by the caller.
        let places: Vec<(bool, Vec<String>, usize)> =
            crate::permissions::simple_commands_in(argv, &self.reading)
                .into_iter()
                .enumerate()
                .flat_map(|(n, words)| {
                    crate::permissions::command_starts(&words)
                        .into_iter()
                        .filter(move |&at| n > 0 || at > 0)
                        .map(move |at| (n > 0, words.clone(), at))
                        .collect::<Vec<_>>()
                })
                .collect();
        self.rules
            .iter()
            .filter(|rule| rule.decision == Decision::Forbidden)
            .find_map(|rule| {
                places.iter().find_map(|(inner, words, at)| {
                    let matched_prefix = rule.pattern.matches_prefix(&words[*at..])?;
                    // A command the call starts is named the way a script's
                    // command is ([`crate::permissions::SCRIPT_RUNS_LEAD`]),
                    // so the refusal names its program, not the call's.
                    let reason = match (inner, rule.reason.clone()) {
                        (false, why) => why,
                        (true, why) => Some(format!(
                            "the {}{}` (`{}`), which is refused: {}",
                            crate::permissions::SCRIPT_RUNS_LEAD,
                            words[*at],
                            crate::quote_argv(&words[*at..]),
                            why.as_deref().unwrap_or("forbidden by command policy")
                        )),
                    };
                    Some(PolicyVerdict {
                        decision: Decision::Forbidden,
                        read_only: false,
                        matched_prefix: Some(matched_prefix),
                        reason,
                        build: false,
                        granted: false,
                        script_note: None,
                        script_checked: false,
                    })
                })
            })
    }

    /// The refusal of a call that downloads packages, outside a disposable
    /// remote environment: the call itself, every command a script or a
    /// wrapper starts, and what a recipe or script file it runs holds, each
    /// from every place its program may stand (the same reading the
    /// `forbid` rules get, so `env pip install` or `sh -c 'npm ci'` is
    /// found).
    fn package_download(&self, argv: &[String]) -> Option<PolicyVerdict> {
        if self.package_downloads || self.unrestricted {
            return None;
        }
        let fetch = crate::permissions::simple_commands_in(argv, &self.reading)
            .into_iter()
            .find_map(|words| {
                crate::permissions::command_starts(&words)
                    .into_iter()
                    .find_map(|at| super::package_fetch::package_fetch(&words[at..]))
            })?;
        Some(PolicyVerdict {
            decision: Decision::Forbidden,
            read_only: false,
            matched_prefix: None,
            reason: Some(super::package_fetch::refusal(&fetch)),
            build: false,
            granted: false,
            script_note: None,
            script_checked: false,
        })
    }

    /// The script constructs the user turned on (decision 0094, amended
    /// 2026-10-02), read from the live control on every evaluation; none
    /// without one.
    fn script_constructs(&self) -> crate::permissions::ScriptConstructs {
        self.live
            .as_ref()
            .map(|l| l.0.script_constructs())
            .unwrap_or_default()
    }

    /// The programs of `words` ([`crate::permissions::programs_to_grant`])
    /// when a grant covers every one of them, `None` otherwise: a command
    /// runs on grants only when each program it runs, wrappers and the
    /// programs they run included, is granted.
    fn granted_by_grants(&self, words: &[String]) -> Option<Vec<String>> {
        self.granted_by_grants_in(words, &self.reading)
    }

    /// [`Self::granted_by_grants`], reading files in `ctx` (a command of a
    /// script after its `cd`).
    fn granted_by_grants_in(
        &self,
        words: &[String],
        ctx: &crate::permissions::ReadContext,
    ) -> Option<Vec<String>> {
        crate::permissions::programs_to_grant_in(words, ctx)
            .ok()
            .filter(|ps| !ps.is_empty() && ps.iter().all(|p| self.grants_program(p)))
    }

    /// For a command a grant would let run but that runs programs with no
    /// grant (`env curl x` with `env` granted), the note the approval
    /// question carries: the programs that still lack a grant, the innermost
    /// first, so the user, and a shortfall report, name the program that
    /// runs and not only its wrapper (decision 0094, amended 2026-10-06).
    /// `None` when nothing is granted, or the command runs one program.
    fn lacking_note(&self, argv: &[String]) -> Option<String> {
        let ps = crate::permissions::programs_to_grant_in(argv, &self.reading).ok()?;
        if ps.len() < 2 {
            return None;
        }
        let lacking: Vec<&String> = ps
            .iter()
            .rev()
            .filter(|p| !self.grants_program(p))
            .collect();
        let first = lacking.first()?;
        Some(format!(
            "The {}{first}` (`{}`); a command runs without asking only when every program it \
             runs is granted, and {} {} no grant.",
            crate::permissions::COMMAND_RUNS_LEAD,
            crate::quote_argv(argv),
            lacking
                .iter()
                .map(|p| format!("`{p}`"))
                .collect::<Vec<_>>()
                .join(", "),
            if lacking.len() == 1 { "has" } else { "have" }
        ))
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
    /// commands ([`crate::permissions::script_commands`]) is allowed when
    /// every command is covered, as Claude Code allows a compound command
    /// when each subcommand is allowed. A command is covered by a grant of
    /// every program it runs ([`crate::permissions::programs_to_grant`]:
    /// a wrapper and the program it runs each need one, decision 0094,
    /// amended 2026-10-06), by the build and test allowlist's cargo rules (decision
    /// 0084), or, once per script, by a `cd` into the worktree
    /// ([`worktree_cd`]). A script of allowlisted cargo commands, with or
    /// without that `cd`, is a build; one that needs a grant is granted. A
    /// command a `forbid` rule or the build allowlist refuses refuses the
    /// whole call. A command nothing covers, or a script with a construct
    /// that is not judged, leaves the call to be asked, and the note says
    /// which. `None` when this is not a script, or when nothing grants named
    /// programs and the allowlist is empty (the call is asked as before).
    ///
    /// Why the allowlist and `cd` count (decision 0094, amended again 2026-10-02):
    /// a planner writes an acceptance as `cd crate && cargo check`, and a
    /// worker runs it as `sh -c`. That script was asked about whole, and an
    /// unattended run denied the worker the very check its contract named,
    /// though `cargo check` alone runs unasked. A `cd` changes only the
    /// directory the next commands start in; the shell tool checks that
    /// directory on disk and runs decision 0084's checks there before the
    /// script runs ([`PolicyVerdict::script_checked`]).
    ///
    /// Only the strict reader's commands get the allowlist and `cd`
    /// coverage, and only the allowlist's cargo rules, not its exact
    /// entries: an exact entry is a project command line, which may name a
    /// file relative to the directory it runs in.
    ///
    /// With script constructs on, a script the strict reader refuses is read
    /// again accepting them ([`crate::permissions::script_commands_with`]),
    /// and the grant names them.
    ///
    /// The user's deny and ask rules are not read here: their hook runs after
    /// this one on every command of the script, in every mode.
    fn script_verdict(&self, argv: &[String]) -> Option<PolicyVerdict> {
        use crate::permissions::{
            SCRIPT_RUNS_LEAD, Script, relies_on_constructs, script_commands_with,
        };
        let grants_any = !self.granted.is_empty()
            || self.live.as_ref().is_some_and(|l| l.0.grants_programs())
            || !self.build.is_empty();
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
            script_checked: false,
        };
        let constructs = self.script_constructs();
        let commands = match script_commands_with(argv, &constructs) {
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
                    script_checked: false,
                });
            }
        }
        // How each command is covered, in order; the first one nothing
        // covers is named in the question.
        let relaxed = relies_on_constructs(argv, &constructs);
        let mut needs_grant = false;
        let mut built = false;
        let mut cd: Option<&str> = None;
        // The files a command reads are read from where the script's `cd`
        // took it.
        let mut here = self.reading.clone();
        let mut previous: Option<&Vec<String>> = None;
        for words in &commands {
            // An assignment of the script (`export GIT_PAGER=...`) is seen by
            // the commands after it.
            if let Some(p) = previous
                && let Some(assigned) = crate::permissions::after_assignment(p, &here)
            {
                here = assigned;
            }
            previous = Some(words);
            let uncovered = |extra: &str| {
                asked(format!(
                    "The {SCRIPT_RUNS_LEAD}{}` (`{}`), which no grant covers{extra}; a script \
                     runs without asking only when every command in it is granted.",
                    words[0],
                    shown(words)
                ))
            };
            if !relaxed && let Some(change) = worktree_cd(words) {
                match change {
                    Ok(dir) if cd.is_none() => {
                        cd = Some(dir);
                        here = crate::permissions::ReadContext {
                            cwd: here
                                .cwd
                                .as_ref()
                                .map(|c| c.join(dir))
                                .filter(|_| here.root.is_some()),
                            ..here.clone()
                        };
                        continue;
                    }
                    Ok(_) => {
                        return Some(uncovered(
                            " (a script may change directory once, into the worktree)",
                        ));
                    }
                    Err(why) => return Some(uncovered(&format!(" ({why})"))),
                }
            }
            if !relaxed && self.build.allows_by_rules(words) {
                built = true;
                continue;
            }
            if self.granted_by_grants_in(words, &here).is_some() {
                needs_grant = true;
                continue;
            }
            return Some(uncovered(""));
        }
        // A script that only changes directory runs nothing a grant or the
        // allowlist names; it is asked about as before.
        if !needs_grant && !built {
            return Some(asked(
                "Its script runs no command a grant or the build and test allowlist covers.".into(),
            ));
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
                "every command of the script is {} ({}){}{}",
                if built {
                    if needs_grant {
                        "granted or on the build and test allowlist"
                    } else {
                        "on the build and test allowlist"
                    }
                } else {
                    "granted"
                },
                programs
                    .iter()
                    .map(|p| format!("`{p}`"))
                    .collect::<Vec<_>>()
                    .join(", "),
                cd.map(|d| format!(", after a `cd` into `{d}` in the worktree"))
                    .unwrap_or_default(),
                if relaxed {
                    format!(
                        "; it uses script constructs you turned on ({}), so only the programs \
                         kGraf can see were checked",
                        constructs.describe()
                    )
                } else {
                    String::new()
                }
            )),
            build: !needs_grant,
            granted: needs_grant,
            script_note: None,
            script_checked: true,
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

/// Whether a script's command is a directory change a script judged command
/// by command may hold (decision 0094, amended again 2026-10-02): `None` when
/// it is not `cd`; `Some(Ok(dir))` for `cd DIR` with `DIR` one relative path
/// that stays inside the worktree by its words alone (no `..`, no absolute
/// path, no `~`, glob or option, not the `.git` directory or kGraf's state);
/// `Some(Err(why))` for any other `cd`. A bare `cd` goes to the home
/// directory, `cd -` to the previous one, and a glob names whatever matches,
/// so none of them is judged. `pushd` and `popd` are never covered. The
/// shell tool checks the directory on disk (a link out of the worktree is
/// refused) before the script runs.
pub(crate) fn worktree_cd(words: &[String]) -> Option<Result<&str, &'static str>> {
    if words.first().map(String::as_str) != Some("cd") {
        return None;
    }
    let [_, dir] = words else {
        return Some(Err(
            "`cd` takes exactly one directory here; a bare `cd` goes to the home directory",
        ));
    };
    if dir.is_empty()
        || dir.starts_with('-')
        || dir.contains(['*', '?', '[', ']', '~'])
        || dir.chars().any(char::is_control)
    {
        return Some(Err(
            "`cd` names no plain directory: an option, a glob or `~`",
        ));
    }
    let path = std::path::Path::new(dir);
    let inside = path.components().all(|c| {
        matches!(
            c,
            std::path::Component::Normal(_) | std::path::Component::CurDir
        )
    });
    if !inside {
        return Some(Err("`cd` leaves the worktree: an absolute path or `..`"));
    }
    if crate::fs_safety::reads_git_dir_or_kgraf(path) {
        return Some(Err("`cd` enters the `.git` directory or kGraf's own state"));
    }
    Some(Ok(dir))
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
        ] {
            let verdict = policy.evaluate(&argv(&variant));
            assert_eq!(
                verdict.decision,
                Decision::Prompt,
                "{variant:?} must require approval, not run as Allow or as a covered Forbidden"
            );
        }
        // The exact shape inside a script is the shape, and is refused
        // (decision 0094, amended 2026-10-06), naming the script's command.
        let verdict = policy.evaluate(&argv(&["sh", "-c", "rm -rf /"]));
        assert_eq!(verdict.decision, Decision::Forbidden, "{verdict:?}");
        assert!(
            verdict
                .reason
                .as_deref()
                .is_some_and(|r| r.starts_with("the script runs `rm` (`rm -rf /`)")),
            "{verdict:?}"
        );
    }

    /// Decision 0093: a granted program runs without approval, marked
    /// granted; another program is still asked about; a `forbid` rule still
    /// wins over a grant; the match is on the exact `argv[0]`.
    #[test]
    fn a_granted_program_is_allowed_and_nothing_else_changes() {
        let (_dir, ctx) = scripts_tree();
        let policy = ExecPolicy::default_kgraf_policy()
            .with_granted_programs(["python3".to_owned(), "rm".to_owned()])
            .with_read_context(ctx);
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
        let (_dir, ctx) = scripts_tree();
        let policy = ExecPolicy::default_kgraf_policy()
            .with_permissions(control.clone())
            .with_read_context(ctx);
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

    /// Decision 0087, amended 2026-10-09: a command that downloads packages
    /// is refused in every mode, under every grant (Bypass, Auto's allow set,
    /// a contract's grant of the program), as the call itself, behind a
    /// wrapper and inside a script, and the refusal says why and what allows
    /// it. A check-only contract that runs `pip install` met the grant path:
    /// the download ran confined, and failed at the network with no reason
    /// (or, where the network is open, reached PyPI unasked). The same
    /// program's other commands keep their grant, and a run whose commands
    /// execute in a disposable remote environment decides it like any
    /// command.
    #[test]
    fn a_package_download_is_refused_under_every_grant_outside_a_disposable_environment() {
        use crate::permissions::PermissionMode;
        let downloads = [
            argv(&["pip", "install", "vectorops"]),
            argv(&["python3", "-m", "pip", "install", "x"]),
            argv(&["env", "pip", "install", "x"]),
            sh("cd web && npm ci"),
            sh("pip install --index-url http://localhost:8080/simple vectorops==0.1.0"),
            argv(&["cargo", "fetch"]),
        ];
        for mode in PermissionMode::ALL {
            let policy = ExecPolicy::default_kgraf_policy()
                .with_build_allowlist(BuildAllowlist::standard())
                .with_granted_programs(["pip".to_owned(), "npm".to_owned(), "env".to_owned()])
                .with_permissions(control_in(mode, &["pip", "python3", "cargo", "npm"]));
            for call in &downloads {
                let v = policy.evaluate(call);
                assert_eq!(v.decision, Decision::Forbidden, "{mode}: {call:?}: {v:?}");
                assert!(!v.granted, "{mode}: {call:?}");
                let why = v.reason.unwrap_or_default();
                assert!(
                    super::super::is_package_fetch_refusal(&why),
                    "{mode}: {call:?}: {why}"
                );
                assert!(why.contains("decision 0087"), "{why}");
            }
            // The program's other commands keep their grant.
            let show = policy.evaluate(&argv(&["pip", "show", "vectorops"]));
            assert_ne!(show.decision, Decision::Forbidden, "{mode}: {show:?}");
        }
        let bypass = || {
            ExecPolicy::default_kgraf_policy()
                .with_permissions(control_in(PermissionMode::Bypass, &[]))
        };
        let pip = argv(&["pip", "install", "vectorops"]);
        let remote = bypass().with_package_downloads(true).evaluate(&pip);
        assert_eq!(remote.decision, Decision::Allow, "{remote:?}");
        let container = bypass().with_unrestricted_shell(true).evaluate(&pip);
        assert_eq!(container.decision, Decision::Allow, "{container:?}");
        // A recipe a granted command runs is read for a download too; an
        // allowlisted build the user configured is not (it runs with the
        // network off, and is the user's choice).
        let dir = tempfile::tempdir().expect("dir");
        std::fs::write(
            dir.path().join("Makefile"),
            "deps:\n\tpip install -r requirements.txt\ntest:\n\tpip install -e . && pytest\n",
        )
        .expect("write");
        let ctx = crate::permissions::ReadContext::in_worktree(dir.path(), None);
        let policy = bypass()
            .with_build_allowlist(BuildAllowlist::cargo_default().with_configured("make test"))
            .with_read_context(ctx);
        let deps = policy.evaluate(&argv(&["make", "deps"]));
        assert_eq!(deps.decision, Decision::Forbidden, "{deps:?}");
        let test = policy.evaluate(&argv(&["make", "test"]));
        assert!(test.build && test.decision == Decision::Allow, "{test:?}");
        // The policy for a routed run allows it; a `forbid` rule still wins.
        let routed = crate::external::remote_policy();
        assert_eq!(routed.evaluate(&pip).decision, Decision::Allow);
        assert_eq!(
            routed.evaluate(&argv(&["rm", "-rf", "/"])).decision,
            Decision::Forbidden
        );
    }

    /// A worktree holding the scripts these tests run, each starting no
    /// program: a grant of `python3` covers a script only when kGraf can
    /// read it (decision 0094, amended 2026-10-06).
    fn scripts_tree() -> (tempfile::TempDir, crate::permissions::ReadContext) {
        let dir = tempfile::tempdir().expect("dir");
        for name in ["fetch.py", "gen.py", "a.py", "b.py", "x.py", "tools/gen.py"] {
            let p = dir.path().join(name);
            std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
            std::fs::write(p, "print(1)\n").expect("write");
        }
        let ctx = crate::permissions::ReadContext::in_worktree(dir.path(), None);
        (dir, ctx)
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

    /// A question asked in Accept edits names the file programs the mode
    /// grants, so a worker refused `python3` in a script with `cp` learns
    /// that `cp` alone runs unasked; the refused program is still the one
    /// the question names. In Auto it names the allow set; in Manual,
    /// nothing is granted by the mode and no sentence is added.
    #[test]
    fn a_shell_question_names_the_programs_the_mode_grants() {
        use crate::permissions::{PermissionMode, RefusedSubject, refused_subject};
        let script = sh("mkdir -p out && cp a out/a && python3 -c 'print(1)' > out/sha.txt");
        let edits = ExecPolicy::default_kgraf_policy()
            .with_permissions(control_in(PermissionMode::AcceptEdits, &[]));
        let q = question(&edits, &script);
        assert!(
            q.contains(
                "The Accept edits permission mode runs these file programs without approval: \
                 cp, mkdir, mv, touch"
            ),
            "{q}"
        );
        assert_eq!(
            refused_subject(
                "shell",
                &serde_json::json!({ "command": script }),
                "user",
                &q
            ),
            Some(RefusedSubject::Program("python3".into())),
            "the added sentence must not change which program the refusal names: {q}"
        );
        let q = question(&edits, &argv(&["python3", "-c", "print(1)"]));
        assert!(q.contains("cp, mkdir, mv, touch"), "{q}");

        let auto = ExecPolicy::default_kgraf_policy()
            .with_permissions(control_in(PermissionMode::Auto, &["rsync"]));
        let q = question(&auto, &argv(&["python3", "-c", "print(1)"]));
        assert!(
            q.contains(
                "The Auto permission mode's allow set runs these programs without approval: rsync"
            ),
            "{q}"
        );

        let manual = ExecPolicy::default_kgraf_policy()
            .with_permissions(control_in(PermissionMode::Manual, &[]));
        let q = question(&manual, &argv(&["python3", "-c", "print(1)"]));
        assert!(!q.contains("without approval: cp"), "{q}");
        assert!(!q.contains("permission mode runs"), "{q}");
    }

    /// Decision 0094, amended 2026-10-02: in Auto, a script is granted when
    /// the allow set grants the program of every command, as Claude Code
    /// allows a compound command when each subcommand is allowed. A worker
    /// was denied a script of `mkdir` and `cp`, both allowed and both used.
    #[test]
    fn auto_grants_a_script_when_it_grants_every_command() {
        use crate::permissions::PermissionMode;
        let (_dir, ctx) = scripts_tree();
        let policy = ExecPolicy::default_kgraf_policy()
            .with_permissions(control_in(
                PermissionMode::Auto,
                &["python3", "cp", "mkdir"],
            ))
            .with_read_context(ctx);
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
        let (_dir, ctx) = scripts_tree();
        let granted = ExecPolicy::default_kgraf_policy()
            .with_granted_programs(["python3".into()])
            .with_read_context(ctx.clone());
        assert!(
            granted
                .evaluate(&sh("python3 a.py && python3 b.py"))
                .granted
        );
        let q = question(&granted, &sh("python3 a.py && mkdir out"));
        assert!(q.contains("The script runs `mkdir`"), "{q}");
        let both = ExecPolicy::default_kgraf_policy()
            .with_granted_programs(["python3".into()])
            .with_permissions(control_in(PermissionMode::Auto, &["mkdir"]))
            .with_read_context(ctx);
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

    /// Decision 0094, amended 2026-10-02: each script-construct switch, off,
    /// leaves a script using it asked about with the "not judged" note, as
    /// before; on, Auto grants it when every program kGraf can see is in
    /// the allow set, and the grant names the switches. A program granted
    /// to one piece of work counts the same way under Manual. A switch read
    /// live: turning it off again reaches the policy already built.
    #[test]
    fn each_script_construct_switch_lets_auto_grant_a_script_using_it() {
        use crate::permissions::{PermissionMode, ScriptConstruct};
        let allowed = ["mkdir", "cp", "ls", "cat", "echo", "/bin/mkdir"];
        for (c, script) in [
            (ScriptConstruct::Variables, "mkdir out && cp $(ls src) out/"),
            (
                ScriptConstruct::Subshells,
                "(mkdir out && cp a out/) && { ls; }",
            ),
            (ScriptConstruct::Background, "mkdir out & cp a b"),
            (ScriptConstruct::InputRedirects, "cat < in.txt > out.txt"),
            (ScriptConstruct::Escapes, "/bin/mkdir out # make it"),
            (ScriptConstruct::OutsideRedirects, "echo x > ../outside.txt"),
        ] {
            let control = control_in(PermissionMode::Auto, &allowed);
            let policy = ExecPolicy::default_kgraf_policy().with_permissions(control.clone());
            let argv = sh(script);
            let off = policy.evaluate(&argv);
            assert!(
                off.decision == Decision::Prompt && !off.granted,
                "{c}: {off:?}"
            );
            assert!(
                off.script_note
                    .as_deref()
                    .is_some_and(|n| n.contains("not judged command by command")),
                "{c}: {off:?}"
            );
            control.set_script_construct(c, true);
            let on = policy.evaluate(&argv);
            assert!(on.decision == Decision::Allow && on.granted, "{c}: {on:?}");
            assert!(
                on.reason
                    .as_deref()
                    .is_some_and(|r| r.contains(c.key()) && r.contains("kGraf can see")),
                "{c}: {on:?}"
            );
            control.set_script_construct(c, false);
            assert_eq!(policy.evaluate(&argv), off, "{c}: turned off again");
        }
        // A visible program outside the allow set still asks, naming it.
        let control = control_in(PermissionMode::Auto, &["mkdir"]);
        control.set_script_construct(ScriptConstruct::Variables, true);
        let policy = ExecPolicy::default_kgraf_policy().with_permissions(control);
        let q = question(&policy, &sh("mkdir out && echo $(python3 x.py)"));
        assert!(q.contains("The script runs `python3`"), "{q}");
        // A contract's grant under Manual.
        let manual = control_in(PermissionMode::Manual, &[]);
        manual.set_script_construct(ScriptConstruct::Background, true);
        let (_dir, ctx) = scripts_tree();
        let granted = ExecPolicy::default_kgraf_policy()
            .with_granted_programs(["python3".into()])
            .with_permissions(manual)
            .with_read_context(ctx);
        assert!(granted.evaluate(&sh("python3 a.py & python3 b.py")).granted);
        assert!(!granted.evaluate(&sh("python3 a.py & mkdir b")).granted);
    }

    /// A script granted because of a switch is recorded as a grant of each
    /// program kGraf saw in it, the one inside `$(...)` included, never of
    /// the shell.
    #[test]
    fn a_script_granted_by_a_switch_records_each_visible_program() {
        use crate::permissions::{PermissionEvent, PermissionMode, ScriptConstruct};
        let control = control_in(PermissionMode::Auto, &["mkdir", "cp", "ls"]);
        control.set_script_construct(ScriptConstruct::Variables, true);
        let policy = ExecPolicy::default_kgraf_policy().with_permissions(control.clone());
        let argv = sh("mkdir out && cp $(ls src) out/");
        let v = policy.evaluate(&argv);
        assert!(v.granted, "{v:?}");
        policy.record_mode_grant("w1", "c1", &argv, &v);
        let mut subjects: Vec<String> = control
            .settle_call("w1", "c1", None)
            .into_iter()
            .filter_map(|e| match e {
                PermissionEvent::Granted { subject, .. } => Some(subject),
                _ => None,
            })
            .collect();
        subjects.sort();
        assert_eq!(subjects, ["`cp`", "`ls`", "`mkdir`"]);
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

    /// A mode's grant is recorded once per program the mode granted, and
    /// never for a verdict the mode did not decide: a verdict that is not a
    /// grant, an unrestricted shell, or a program granted to the contract
    /// (even when the mode would grant it too).
    #[test]
    fn a_mode_grant_is_recorded_only_for_programs_the_mode_granted() {
        use crate::permissions::{PermissionEvent, PermissionMode};
        let granted_subjects = |control: &crate::permissions::PermissionControl| {
            let mut s: Vec<String> = control
                .settle_call("w1", "c1", None)
                .into_iter()
                .filter_map(|e| match e {
                    PermissionEvent::Granted { subject, .. } => Some(subject),
                    _ => None,
                })
                .collect();
            s.sort();
            s
        };
        let script = sh("mkdir out && mkdir out2 && cp a out/");
        let record = |policy: &ExecPolicy, verdict: &PolicyVerdict| {
            policy.record_mode_grant("w1", "c1", &script, verdict);
        };
        // The mode granted `mkdir` and `cp`: each is recorded once.
        let control = control_in(PermissionMode::Auto, &["mkdir", "cp"]);
        let policy = ExecPolicy::default_kgraf_policy().with_permissions(control.clone());
        let v = policy.evaluate(&script);
        assert!(v.granted, "{v:?}");
        record(&policy, &v);
        assert_eq!(granted_subjects(&control), ["`cp`", "`mkdir`"]);
        // A verdict that is not a grant records nothing.
        let control = control_in(PermissionMode::Auto, &["mkdir", "cp"]);
        let policy = ExecPolicy::default_kgraf_policy().with_permissions(control.clone());
        let mut v = policy.evaluate(&script);
        v.granted = false;
        record(&policy, &v);
        assert_eq!(granted_subjects(&control), Vec::<String>::new());
        // An unrestricted shell is the container's grant, not the mode's.
        let control = control_in(PermissionMode::Auto, &["mkdir", "cp"]);
        let policy = ExecPolicy::default_kgraf_policy()
            .with_permissions(control.clone())
            .with_unrestricted_shell(true);
        let v = policy.evaluate(&script);
        assert!(v.granted, "{v:?}");
        record(&policy, &v);
        assert_eq!(granted_subjects(&control), Vec::<String>::new());
        // `cp` is granted to the contract, so the mode records only `mkdir`.
        let control = control_in(PermissionMode::Auto, &["mkdir", "cp"]);
        let policy = ExecPolicy::default_kgraf_policy()
            .with_permissions(control.clone())
            .with_granted_programs(["cp".to_string()]);
        let v = policy.evaluate(&script);
        assert!(v.granted, "{v:?}");
        record(&policy, &v);
        assert_eq!(granted_subjects(&control), ["`mkdir`"]);
        // A command that is itself a contract-granted program records nothing.
        let control = control_in(PermissionMode::Auto, &["mkdir"]);
        let policy = ExecPolicy::default_kgraf_policy()
            .with_permissions(control.clone())
            .with_granted_programs(["mkdir".to_string()]);
        let one = argv(&["mkdir", "out"]);
        let v = policy.evaluate(&one);
        assert!(v.granted, "{v:?}");
        policy.record_mode_grant("w1", "c1", &one, &v);
        assert_eq!(granted_subjects(&control), Vec::<String>::new());
    }

    /// The reason a granted script gives lists each program once, in order.
    #[test]
    fn a_granted_script_names_each_program_once() {
        let policy = ExecPolicy::default_kgraf_policy().with_permissions(control_in(
            crate::permissions::PermissionMode::Auto,
            &["mkdir", "cp"],
        ));
        let v = policy.evaluate(&sh("mkdir a && mkdir b && cp x y && mkdir c"));
        assert!(v.granted, "{v:?}");
        let why = v.reason.expect("reason");
        assert_eq!(why.matches("mkdir").count(), 1, "{why}");
        assert!(why.contains("`cp`"), "{why}");
        assert!(why.find("mkdir") < why.find("`cp`"), "{why}");
    }

    /// What a review read is remembered for each reviewed command, so a file
    /// changed after the review of any of them is found; the policy's debug
    /// text names the live mode.
    #[test]
    fn a_changed_recipe_is_found_for_every_reviewed_command() {
        let dir = tempfile::tempdir().expect("dir");
        let makefile = dir.path().join("Makefile");
        std::fs::write(&makefile, "t:\n\techo one\nu:\n\techo two\n").expect("write");
        let ctx = crate::permissions::ReadContext::in_worktree(dir.path(), None);
        let policy = ExecPolicy::default_kgraf_policy().with_read_context(ctx);
        let (t, u) = (argv(&["make", "t"]), argv(&["make", "u"]));
        policy.note_reviewed(&t);
        policy.note_reviewed(&u);
        assert_eq!(policy.files_changed_since_review(&t), None);
        std::fs::write(&makefile, "t:\n\techo changed\nu:\n\techo two\n").expect("write");
        for call in [&t, &u] {
            let why = policy.files_changed_since_review(call).expect("changed");
            assert!(why.to_lowercase().contains("`makefile`"), "{why}");
        }
        let live = ExecPolicy::default_kgraf_policy()
            .with_permissions(control_in(crate::permissions::PermissionMode::Auto, &[]));
        assert!(format!("{live:?}").contains("LivePermissions("), "{live:?}");
    }

    /// An allow rule that names the call first decides it, yet a `forbid`
    /// rule still refuses a program that call runs after its prefix words
    /// (`env rm x`); the refusal gives the rule's own reason, and a command
    /// a script starts is named as such.
    #[test]
    fn a_forbid_rule_refuses_after_prefix_words_even_past_an_allow_rule() {
        let policy = ExecPolicy::new(Decision::Prompt)
            .allow_read_only(&["env"], "env is listed")
            .forbid(&["rm"], "no deletes");
        let v = policy.evaluate(&argv(&["env", "rm", "x"]));
        assert_eq!(v.decision, Decision::Forbidden, "{v:?}");
        // The call's own program is named as the rule wrote it.
        assert_eq!(v.reason.as_deref(), Some("no deletes"));
        let v = policy.evaluate(&sh("echo a; rm x"));
        assert_eq!(v.decision, Decision::Forbidden, "{v:?}");
        let why = v.reason.expect("reason");
        assert!(
            why.contains("`rm`") && why.ends_with("which is refused: no deletes"),
            "{why}"
        );
    }

    /// The first rule that names the call decides it: an allow rule written
    /// before a `forbid` rule for the same argv keeps the call allowed, as
    /// the rule order says.
    #[test]
    fn a_rule_naming_the_whole_call_decides_before_a_later_forbid_rule() {
        let policy = ExecPolicy::new(Decision::Prompt)
            .allow_read_only(&["git"], "listed first")
            .forbid(&["git", "push"], "listed second");
        let v = policy.evaluate(&argv(&["git", "push", "origin"]));
        assert!(v.decision == Decision::Allow && v.read_only, "{v:?}");
    }

    /// A call that runs a program with no grant is asked about with a note
    /// naming the programs that lack one, innermost first; a call of one
    /// program, and a call a rule named, carry no note.
    #[test]
    fn a_command_with_an_ungranted_inner_program_is_asked_with_a_note() {
        let one = ExecPolicy::default_kgraf_policy().with_granted_programs(["env".to_string()]);
        let v = one.evaluate(&argv(&["env", "curl", "x"]));
        assert!(v.decision == Decision::Prompt && !v.granted, "{v:?}");
        let note = v.script_note.expect("a note");
        assert!(note.contains("`curl`"), "{note}");
        assert!(note.ends_with("and `curl` has no grant."), "{note}");
        // Two programs lack a grant.
        let other = ExecPolicy::default_kgraf_policy().with_granted_programs(["ls".to_string()]);
        let note = other
            .evaluate(&argv(&["env", "curl", "x"]))
            .script_note
            .expect("a note");
        assert!(note.contains("`curl`, `env` have no grant."), "{note}");
        // A command of one program has no inner program to name.
        assert_eq!(one.evaluate(&argv(&["curl", "x"])).script_note, None);
        // A rule that named the whole call decides it, with no note.
        let ruled = ExecPolicy::new(Decision::Prompt)
            .with_rule(CommandRule {
                pattern: PrefixPattern::exact(&["env", "curl"]),
                decision: Decision::Prompt,
                read_only: false,
                reason: None,
            })
            .with_granted_programs(["env".to_string()]);
        let v = ruled.evaluate(&argv(&["env", "curl", "x"]));
        assert_eq!(v.decision, Decision::Prompt);
        assert_eq!(v.script_note, None);
    }
}

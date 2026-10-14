//! The acceptance check of one attempt. The order of its steps matters (see
//! [`CommandVerifier`]).

use crate::gates::run_confined;
use crate::workspaces::GitWorkspaces;
use kgraf_orchestrator::{
    OpenContract, Verdict, VerifiedCommit, Verifier, placeholder_left_in_place_reason,
    placeholder_writes_left_in_place,
};
use kgraf_runtime::{WorkerResult, forbidden_path, is_build_config_path, same_path};
use kgraf_sandbox::{CommitOutcome, Excluded, GeneratedKind, Leftovers, lockfile_manifests};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Verifies an attempt in these steps, none of which runs host git in the
/// worker's tree or runs worker code outside a sandbox:
///
/// 1. no process the worker started may still run in the worktree (the
///    sandbox hold and session check, `WorkspaceManager::quiesce`): a tree
///    something still writes is not final;
/// 2. the worktree is committed through merge-back (`SafeGit`: hooks and
///    fsmonitor off, attributes from a trusted tree, the gitlink verified,
///    protected paths refused, a moved HEAD refused). The commit is the
///    snapshot of what the worker changed, and the only thing ever merged. A
///    lockfile or Python bytecode the worker's own run generated outside its
///    scope is left out of it and reported ("made by the worker");
/// 3. every path the commit changes against the dispatch base must be one the
///    contract declared, compared as a case- and normalization-insensitive
///    volume resolves names, and none may be repository metadata or cargo's
///    or rustup's configuration ([`build_config_changes`]);
/// 4. every ignored file the worker's run left in the tree is removed
///    (`MergeBack::remove_ignored`); then the acceptance command, narrowed
///    to the tests of the contract's own files when its runner allows
///    ([`crate::scoped_check`]; a gate remediation keeps the whole command),
///    runs **inside the sandbox** of that worktree (writes confined to the
///    worktree, its scratch directory and the check's own folder, network off,
///    kGraf's state unreadable, the same policy as the worker's shell),
///    bounded by the gate timeout; when its shell exits, its process group is
///    killed;
/// 5. the worktree must then be quiet again (a process that left the group is
///    found by the hold check and fails the verification);
/// 6. what the command left is removed, descriptor-relative and never through
///    a symlink (`MergeBack::discard_leftovers`). New files are reported as
///    notices and never merged. A committed file the command changed fails the
///    verification: the merged tree would differ from the tree it checked;
/// 7. a committed attempt that passed a narrowed cargo check also runs the
///    checks of the packages that depend on its packages and can build now,
///    in a throwaway worktree ([`check_dependents`]).
///
/// The commit comes before the check. With the check first, the commit swept
/// up whatever the check wrote, so the scope check refused the worker for a
/// file its check generated ("changes outside the declared write scope:
/// [\"Cargo.lock\"]"). Running the check on a throwaway worktree of the commit
/// was the other option, not taken: it costs a checkout per check, and the
/// check's leftovers (step 6) are judged against the worker's tree. What the
/// worker's run left is kept out of the check instead (below).
///
/// **Nothing the worker's run left is a check input** (decision 0084, amended
/// 2026-10-06). The check's temporary folder, tool homes and cargo target
/// folder are a new folder of its own outside the worker's writable roots
/// (`crate::gates::run_confined`), its target folder's `debug` profile a
/// copy-on-write clone of the checkout's ([`warm_build_folder`]), never the worker's
/// `<scratch>/target`; and ignored files in the tree are removed before it
/// runs. A forged test binary or forged build records in the worker's build
/// folder, or an up-to-date mark of any tool in the tree, no longer make the
/// check pass. The code under test still runs in the check, as in any test
/// run: a test of the worker's own can still pass itself, and only the
/// contract's tests and the quality gate judge that.
pub(crate) struct CommandVerifier {
    pub(crate) command: String,
    pub(crate) repo: PathBuf,
    pub(crate) workspaces: Arc<GitWorkspaces>,
    pub(crate) merge_back: Arc<kgraf_sandbox::MergeBack>,
    pub(crate) read_denies: Vec<PathBuf>,
    /// The check is killed after this long (the configured gate timeout).
    pub(crate) timeout: Duration,
    /// The user's permission control: in the Bypass mode, changes to
    /// `.gitattributes`, `.gitmodules` and `.githooks` are judged by the
    /// declared write scope like any file and merged as plain data (decision
    /// 0094); in every other mode they are refused.
    pub(crate) permissions: Arc<kgraf_tools::PermissionControl>,
    /// `command` is also one of the run's gate commands, so the root gate
    /// runs it on the merged result. Only then may an attempt's whole check
    /// be deferred to it ([`deferred_notice`]); without it a deferred check
    /// would never run.
    pub(crate) root_gate: bool,
    /// The zero-test rule for every contract, not only a small request's
    /// (decision 0100, an opt-in setting): a narrowed cargo check that
    /// selects no test marks the verdict, and the contract stays unverified
    /// until the root gate passes over it.
    pub(crate) zero_test_plans: bool,
}

/// Tries of the hold check, and the pause between them: a process that has
/// been killed can still hold the hold for a few milliseconds.
const QUIESCE_TRIES: u32 = 40;
const QUIESCE_PAUSE: Duration = Duration::from_millis(25);

/// `WorkspaceManager::quiesce`, retried for up to about one second while the
/// worktree reads busy. Any other error, or busy after the last try, is
/// returned.
fn quiesce_retrying(
    manager: &kgraf_sandbox::WorkspaceManager,
    ws: &kgraf_sandbox::Workspace,
) -> Result<kgraf_sandbox::QuietGuard, kgraf_sandbox::WorkspaceError> {
    let mut tries = 0;
    loop {
        match manager.quiesce(ws) {
            Err(kgraf_sandbox::WorkspaceError::Busy { .. }) if tries + 1 < QUIESCE_TRIES => {
                tries += 1;
                std::thread::sleep(QUIESCE_PAUSE);
            }
            other => return other,
        }
    }
}

/// Output a failed check's reason keeps: its end, and, when the output is
/// longer, up to half of it for the error lines of the cut part with their
/// files (`gates::read_log`).
const CHECK_OUTPUT_CAP: usize = 1_000;

/// The build folder a check's own cargo target folder starts from: the
/// checkout's `target/`, as for a worker's worktree (decision 0060), when it
/// is a real folder. The user builds it; no sandboxed command can write it.
/// Never the worker's `<scratch>/target`: the worker's shell writes that
/// folder, and cargo trusts the build records in it.
pub(crate) fn warm_build_folder(checkout: &Path) -> Option<PathBuf> {
    let target = checkout.join("target");
    std::fs::symlink_metadata(&target)
        .is_ok_and(|m| m.is_dir())
        .then_some(target)
}

/// An accepted no-op attempt's own notice (decision 0086): every attempt
/// verified with `Integration::Empty` is reported, not only traced, so a run
/// that keeps "passing" without changing anything stays visible without
/// reading the trace.
/// The notice names the contract: the run's notices are a set, so one fixed
/// text would show once however many attempts were empty.
fn empty_integration_notice(contract: &str) -> String {
    format!(
        "contract {}: this attempt integrated no change: its check passed against the worktree \
         exactly as it was dispatched",
        clean(contract)
    )
}

/// The notice of an attempt accepted as an empty integration because an
/// earlier merge of the run delivered its work (decision 0086). It begins
/// `unverified:` like the run's other unverified reasons (decision 0093): the
/// attempt itself changed nothing, and the evidence is the earlier merge.
fn delivered_earlier_notice(
    contract: &str,
    command: &str,
    writes: &[&str],
    earlier: &crate::earlier_delivery::EarlierDelivery,
) -> String {
    let short: String = earlier.original_base.chars().take(12).collect();
    format!(
        "contract {}: unverified: this attempt integrated no change. Its declared files ({}) \
         were already changed by an earlier merge of this run (contract {}); `{}` passes on the \
         tree as dispatched and failed at the run's original base {short}, so that merge made it \
         pass",
        clean(contract),
        clean(&writes.join(", ")),
        clean(&earlier.contracts.join(", ")),
        clean(command)
    )
}

/// Whether `command` passes on a throwaway worktree of `base`. The worktree
/// is removed on every path; an error means the check could not be run, which
/// says nothing about whether it passes.
fn passes_at(
    manager: &kgraf_sandbox::WorkspaceManager,
    base: &str,
    denies: &[PathBuf],
    command: &str,
    timeout: Duration,
) -> Result<bool, String> {
    run_at(manager, base, denies, command, timeout).map(|(passed, _)| passed)
}

/// [`passes_at`], with the check's output (capped as an attempt's is).
fn run_at(
    manager: &kgraf_sandbox::WorkspaceManager,
    base: &str,
    denies: &[PathBuf],
    command: &str,
    timeout: Duration,
) -> Result<(bool, String), String> {
    run_at_rooted(manager, base, denies, command, timeout).map(|(passed, out, _)| (passed, out))
}

/// [`run_at`], also with the root of the throwaway worktree, so the output's
/// paths can be compared with another run's ([`own_file_failure`]).
fn run_at_rooted(
    manager: &kgraf_sandbox::WorkspaceManager,
    base: &str,
    denies: &[PathBuf],
    command: &str,
    timeout: Duration,
) -> Result<(bool, String, PathBuf), String> {
    let name = crate::workspaces::harness_worktree_name(crate::workspaces::PROBE_PREFIX);
    let ws = manager
        .create(&kgraf_sandbox::WorkspaceSpec {
            name: &name,
            base,
            branch: None,
        })
        .map_err(|e| format!("cannot create a worktree at {base}: {e}"))?;
    let ran = run_confined(
        &ws,
        manager.repo(),
        denies,
        command,
        timeout,
        CHECK_OUTPUT_CAP,
        None,
    );
    let root = ws.root().to_path_buf();
    let removed = manager.remove(ws);
    let ran = ran?;
    removed.map_err(|e| format!("cannot remove the worktree at {base}: {e}"))?;
    Ok((ran.0, ran.1, root))
}

/// The most lines of a failing output the own-file test reads.
const OWN_FILE_LINES: usize = 400;

/// `text` with each spelling of a worktree `root` (as given, and with or
/// without the macOS `/private` prefix) replaced by `<worktree>`, so the
/// outputs of two runs in different worktrees compare line by line. With
/// `windows` (the output is a Windows program's), the Windows spellings too
/// ([`windows_spellings`]); a parameter, so the rule is tested on every host.
///
/// **Why Windows needs more.** A check's output names files with
/// backslashes (`<root>\wcl-core\Cargo.toml`, as cargo prints them), which
/// the matching of [`names_file`] never finds: it reads a whole path between
/// `/` separators, and a backslash is no path character to it, so the tail
/// `Cargo.toml` of `<root>\wcl-core\Cargo.toml` was read as the root
/// manifest. The check runs through Git Bash, whose programs print the
/// worktree in the MSYS form (`/c/...`); and a folder in the temporary
/// folder can be spelled with a short 8.3 name (`RUNNER~1`) or its long one.
/// So on Windows the text's backslashes become `/`, and each root is replaced
/// in its drive form, its MSYS form, and both again for its canonical (long)
/// path.
fn with_roots_named_as(text: &str, roots: &[&Path], windows: bool) -> String {
    let mut spellings: Vec<String> = Vec::new();
    for root in roots {
        if windows {
            let given = root.to_string_lossy().into_owned();
            spellings.extend(windows_spellings(&given));
            if let Ok(long) = kgraf_core::plain_path::canonicalize(root) {
                spellings.extend(windows_spellings(&long.to_string_lossy()));
            }
            continue;
        }
        let r = root.to_string_lossy().trim_end_matches('/').to_owned();
        if r.is_empty() {
            continue;
        }
        match r.strip_prefix("/private").filter(|b| b.starts_with('/')) {
            Some(bare) => spellings.push(bare.to_owned()),
            None => spellings.push(format!("/private{r}")),
        }
        spellings.push(r);
    }
    // Longest first: `/private/x` before `/x`, so no prefix is left behind.
    spellings.sort_by_key(|s| std::cmp::Reverse(s.len()));
    spellings.dedup();
    let mut out = if windows {
        text.replace('\\', "/")
    } else {
        text.to_owned()
    };
    for s in &spellings {
        out = out.replace(s.as_str(), "<worktree>");
    }
    out
}

/// The spellings of the Windows folder `root` in a check's output once its
/// backslashes are `/`: the drive form with either case of letter
/// (`C:/x/y`, a verbatim `\\?\` prefix dropped) and the MSYS form Git
/// Bash's programs print (`/c/x/y`). A root with no drive letter is kept
/// as given.
fn windows_spellings(root: &str) -> Vec<String> {
    let slashed = root.replace('\\', "/");
    let slashed = slashed.strip_prefix("//?/").unwrap_or(&slashed);
    let slashed = slashed.trim_end_matches('/');
    if slashed.is_empty() {
        return Vec::new();
    }
    let mut out = vec![slashed.to_owned()];
    let mut chars = slashed.chars();
    if let (Some(drive), Some(':')) = (chars.next(), chars.next())
        && drive.is_ascii_alphabetic()
    {
        let rest = &slashed[2..];
        let lower = drive.to_ascii_lowercase();
        out.push(format!("{}:{rest}", drive.to_ascii_uppercase()));
        out.push(format!("{lower}:{rest}"));
        out.push(format!("/{lower}{rest}"));
    }
    out
}

fn is_path_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '/')
}

/// Whether `line` names `file` (a path relative to the worktree) as a whole
/// path: `<worktree>/file`, `file` or `./file`, and not the tail of another
/// path (`member/file`) or the head of one (`file.bak`).
fn names_file(line: &str, file: &str) -> bool {
    let file = file.trim_start_matches("./");
    if file.is_empty() {
        return false;
    }
    let absolute = format!("<worktree>/{file}");
    for (needle, is_absolute) in [(absolute.as_str(), true), (file, false)] {
        for (at, _) in line.match_indices(needle) {
            let mut after = line[at + needle.len()..].chars();
            let (next, then) = (after.next(), after.next());
            // A dot ends a sentence unless a name continues after it.
            let continues = next
                .is_some_and(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '/'))
                || (next == Some('.') && then.is_some_and(char::is_alphanumeric));
            if continues {
                continue;
            }
            let before = &line[..at];
            let before = before.strip_suffix("./").unwrap_or(before);
            if is_absolute || !before.chars().next_back().is_some_and(is_path_char) {
                return true;
            }
        }
    }
    false
}

/// Whether another open contract's file `other` could be what a failure in
/// the contract's own file `own` waits on: it lies in the package `own`
/// opens. For a manifest in a directory that is the directory's subtree; for
/// one at the root, the root's own files and `src/` (the members are other
/// packages, so their files are not it).
fn could_complete(own: &str, other: &str) -> bool {
    let own = own.trim_start_matches("./");
    let other = other.trim_start_matches("./");
    if own == other {
        return false;
    }
    match own.rsplit_once('/') {
        Some((dir, _)) => other.starts_with(&format!("{dir}/")),
        None => !other.contains('/') || other.starts_with("src/"),
    }
}

/// A failure of the attempt's check that is the attempt's own, though the
/// check also fails at the dispatch base: a line of the failing output names
/// a file the contract itself writes, and that line does not occur in the
/// output at the dispatch base. A broken manifest the contract wrote is the
/// case: it is not another contract's missing work, and only the worker that
/// wrote it can repair it. Returns the first such line.
///
/// The failure stays ambiguous, and is left to the deferral, when a new line
/// also names a file of an open contract (`others`), or an own file that one
/// of them could complete ([`could_complete`]): the output may then be only
/// their missing work.
pub(crate) fn own_file_failure(
    attempt_output: &str,
    attempt_roots: &[&Path],
    base_output: &str,
    base_root: &Path,
    writes: &[&str],
    others: &[&str],
) -> Option<String> {
    own_file_failure_as(
        attempt_output,
        attempt_roots,
        base_output,
        base_root,
        writes,
        others,
        cfg!(windows),
    )
}

/// [`own_file_failure`], with `windows` saying whether the outputs are a
/// Windows program's ([`with_roots_named_as`]).
pub(crate) fn own_file_failure_as(
    attempt_output: &str,
    attempt_roots: &[&Path],
    base_output: &str,
    base_root: &Path,
    writes: &[&str],
    others: &[&str],
    windows: bool,
) -> Option<String> {
    let attempt = with_roots_named_as(attempt_output, attempt_roots, windows);
    let base = with_roots_named_as(base_output, &[base_root], windows);
    let at_base: std::collections::HashSet<&str> = base.lines().map(str::trim).collect();
    let fresh: Vec<&str> = attempt
        .lines()
        .take(OWN_FILE_LINES)
        .map(str::trim)
        .filter(|line| !line.is_empty() && !at_base.contains(line))
        .collect();
    let named_own = |line: &str| {
        writes
            .iter()
            .copied()
            .filter(|w| names_file(line, w))
            .count()
            > 0
    };
    let waits_on_others = fresh.iter().any(|line| {
        others.iter().any(|o| names_file(line, o))
            || writes
                .iter()
                .any(|w| names_file(line, w) && others.iter().any(|o| could_complete(w, o)))
    });
    if waits_on_others {
        return None;
    }
    fresh
        .into_iter()
        .find(|line| named_own(line))
        .map(str::to_owned)
}

/// What the verdict of an attempt says when its whole check also fails at
/// its dispatch base and nothing is deferred (the plan's last contract, or
/// no root gate): which merges of this run the base holds, and whether the
/// check passed where the run started. Passing there means one of those
/// earlier deliveries broke it; failing there too means it failed before the
/// run's work. Either way the failure is not this attempt's change alone,
/// and the worker, the retry brief and the hold reason are told so. One more
/// run of the command, at the run's original base, and only when an earlier
/// merge exists.
fn base_attribution(
    manager: &kgraf_sandbox::WorkspaceManager,
    branch: &str,
    dispatch_base: &str,
    denies: &[PathBuf],
    command: &str,
    timeout: Duration,
) -> String {
    let earlier =
        match crate::earlier_delivery::merged_before(manager.repo(), branch, dispatch_base) {
            Ok(earlier) => earlier,
            Err(e) => {
                return format!(
                    "\n(it also fails at the dispatch base, so the failure is not this attempt's \
                 change alone; which earlier merge of this run it comes from could not be read: \
                 {e})"
                );
            }
        };
    if earlier.contracts.is_empty() {
        return "\n(it also fails at the dispatch base, where no contract of this run had merged \
                yet: it failed before this run's work, not because of this attempt's change)"
            .into();
    }
    let short: String = earlier.original_base.chars().take(12).collect();
    let contracts = clean(&earlier.contracts.join(", "));
    match passes_at(manager, &earlier.original_base, denies, command, timeout) {
        Ok(true) => format!(
            "\n(it also fails at the dispatch base and passes at the run's original base \
             {short}, so an earlier merge of this run broke it before this attempt started: the \
             dispatch base holds the merges of contracts {contracts})"
        ),
        Ok(false) => format!(
            "\n(it also fails at the dispatch base and at the run's original base {short}: it \
             failed before this run's work, not because of this attempt's change; the merges of \
             contracts {contracts} did not make it pass)"
        ),
        Err(e) => format!(
            "\n(it also fails at the dispatch base, which holds the merges of contracts \
             {contracts}, so the failure is not this attempt's change alone; it could not be run \
             at the run's original base {short} to tell whether one of them broke it: {e})"
        ),
    }
}

/// What an attempt's failed check said when run again on the attempt merged
/// onto the integration tip ([`check_on_tip`]).
enum OnTip {
    /// The tip is still the attempt's dispatch base: nothing merged since.
    NotMoved,
    /// It passes on the merge; `tip` is the tip it merged onto.
    Passed { tip: String },
    /// It fails on the merge too: that output, with a line saying where it
    /// ran.
    Failed { output: String },
    /// It could not run on the merge (a conflict, a git failure): a line
    /// saying why, for the attempt's own output.
    NotRun { note: String },
}

/// Run `command` on `attempt` (the attempt's commit) merged onto the
/// integration tip, when the tip moved past `dispatch_base`. The merge is an
/// unreferenced commit (`MergeBack::trial_merge`): no branch moves and no
/// working tree of the run is touched; the check runs in a throwaway
/// worktree of it, as [`passes_at`] runs one.
fn check_on_tip(
    manager: &kgraf_sandbox::WorkspaceManager,
    merge_back: &kgraf_sandbox::MergeBack,
    dispatch_base: &str,
    attempt: &str,
    denies: &[PathBuf],
    command: &str,
    timeout: Duration,
) -> OnTip {
    let tip = match merge_back.integration_tip() {
        Ok(tip) => tip,
        Err(e) => {
            return OnTip::NotRun {
                note: format!(
                    "\n(the integration tip could not be read to run it on this attempt merged \
                     with the work merged after its dispatch base: {e})"
                ),
            };
        }
    };
    if tip == dispatch_base {
        return OnTip::NotMoved;
    }
    let short: String = tip.chars().take(12).collect();
    match merge_back.trial_merge(&[attempt.to_owned()]) {
        Ok(kgraf_sandbox::merge::TrialMerge::Clean { commit, .. }) => {
            match run_at(manager, &commit, denies, command, timeout) {
                Ok((true, _)) => OnTip::Passed { tip },
                Ok((false, out)) => OnTip::Failed {
                    output: format!(
                        "{}\n(this output is from the attempt merged onto the integration tip \
                         {short}, which holds work merged after the attempt's dispatch base; \
                         the attempt's own tree failed too)",
                        out.trim_end()
                    ),
                },
                Err(e) => OnTip::NotRun {
                    note: format!(
                        "\n(it could not be run on this attempt merged onto the integration tip \
                         {short}, which holds work merged after its dispatch base: {e})"
                    ),
                },
            }
        }
        Ok(kgraf_sandbox::merge::TrialMerge::Conflicted { paths, .. }) => OnTip::NotRun {
            note: format!(
                "\n(it was not run on this attempt merged onto the integration tip {short}, \
                 which holds work merged after its dispatch base: the merge conflicts on {})",
                clean(&paths.join(", "))
            ),
        },
        Err(e) => OnTip::NotRun {
            note: format!(
                "\n(this attempt could not be merged onto the integration tip {short}, which \
                 holds work merged after its dispatch base, to run it there: {e})"
            ),
        },
    }
}

/// The notice of an attempt whose check failed on its own tree and passed on
/// the attempt merged onto the integration tip. Named per contract: the run's
/// notices are a set.
fn on_tip_notice(contract: &str, command: &str, tip: &str) -> String {
    let short: String = tip.chars().take(12).collect();
    format!(
        "contract {}: `{}` failed on the attempt's own tree, which lacks work merged after its \
         dispatch base, and passed on the attempt merged onto the integration tip {short}, which \
         is what merges",
        clean(contract),
        clean(command)
    )
}

/// Output a dependent's failing check keeps. Larger than an attempt's own:
/// the attribution reads the lines that name files, and a test binary's
/// panic lines come after its list of tests.
const DEPENDENT_OUTPUT_CAP: usize = 4_000;

/// What the checks of an attempt's dependent packages said
/// ([`check_dependents`]; decision 0018, amended 2026-10-12).
#[derive(Debug, Default)]
struct DependentsOutcome {
    /// A failure that is this attempt's: the attempt fails with it.
    against: Option<String>,
    /// The commands that passed, recorded in the verdict's `by`.
    passed: Vec<String>,
    /// Eligible dependents the cap left out.
    over_cap: Vec<String>,
    /// Failures not tied to this attempt, and runs that could not be made.
    notices: Vec<String>,
}

/// Run the check of each package that depends on the attempt's packages and
/// can build now ([`crate::scoped_check::dependents`]), on the attempt merged
/// onto the integration tip (the attempt itself when the tip has not moved),
/// in a throwaway worktree. A dependent that fails there runs again at the tip
/// alone, and the failure counts against the attempt when:
///
/// - it passes at the tip: the attempt's change is the only difference; or
/// - a line new to the output names a file this contract writes and no file
///   an open contract writes ([`own_file_failure`]): a panic or a compile
///   error in this contract's own code, reached through the dependent's tests.
///
/// Any other failure (a test that fails without the attempt too, failing in
/// the dependent's own code) may be the fault of merged work this contract
/// cannot change; failing the attempt for it would retry and hold a worker
/// that cannot fix it. It is a notice, and the root gate judges the merged
/// result as before. No error here fails the attempt.
#[allow(clippy::too_many_arguments)]
fn check_dependents(
    manager: &kgraf_sandbox::WorkspaceManager,
    merge_back: &kgraf_sandbox::MergeBack,
    contract: &str,
    attempt: &str,
    dispatch_base: &str,
    attempt_root: &Path,
    denies: &[PathBuf],
    whole: &str,
    writes: &[&str],
    open: &[&str],
    timeout: Duration,
    warm: Option<&Path>,
) -> DependentsOutcome {
    use crate::scoped_check::{DEPENDENTS_CAP, dependents};
    let mut out = DependentsOutcome::default();
    // Read first on the attempt's own tree: most attempts have no eligible
    // dependent, and then no worktree is made.
    if dependents(whole, attempt_root, writes, open, DEPENDENTS_CAP)
        .run
        .is_empty()
    {
        return out;
    }
    let not_run = |why: String| {
        format!(
            "contract {}: the checks of the packages that depend on its packages did not run: \
             {why}",
            clean(contract)
        )
    };
    let tip = match merge_back.integration_tip() {
        Ok(tip) => tip,
        Err(e) => {
            out.notices.push(not_run(format!(
                "the integration tip could not be read: {e}"
            )));
            return out;
        }
    };
    let target = if tip == dispatch_base {
        attempt.to_owned()
    } else {
        match merge_back.trial_merge(&[attempt.to_owned()]) {
            Ok(kgraf_sandbox::merge::TrialMerge::Clean { commit, .. }) => commit,
            Ok(kgraf_sandbox::merge::TrialMerge::Conflicted { paths, .. }) => {
                out.notices.push(not_run(format!(
                    "the attempt conflicts with the integration tip on {}",
                    clean(&paths.join(", "))
                )));
                return out;
            }
            Err(e) => {
                out.notices.push(not_run(format!(
                    "the attempt could not be merged onto the integration tip: {e}"
                )));
                return out;
            }
        }
    };
    // (command, output, worktree root) of each failure.
    let mut failed: Vec<(String, String, PathBuf)> = Vec::new();
    let ran = with_worktree(manager, &target, |ws| {
        // Chosen again on the tree that runs: work merged after the dispatch
        // base can add a dependent or an edge.
        let chosen = dependents(whole, ws.root(), writes, open, DEPENDENTS_CAP);
        out.over_cap = chosen.over_cap;
        for d in chosen.run {
            match run_confined(
                ws,
                manager.repo(),
                denies,
                &d.command,
                timeout,
                DEPENDENT_OUTPUT_CAP,
                warm,
            ) {
                Ok((true, _)) => out.passed.push(d.command),
                Ok((false, output)) => failed.push((d.command, output, ws.root().to_path_buf())),
                Err(e) => out
                    .notices
                    .push(not_run(format!("`{}`: {e}", clean(&d.command)))),
            }
        }
    });
    if let Err(e) = ran {
        out.notices.push(not_run(e));
        return out;
    }
    if failed.is_empty() {
        return out;
    }
    let short: String = tip.chars().take(12).collect();
    let failing: Vec<String> = failed.iter().map(|(c, _, _)| clean(c)).collect();
    let ran = with_worktree(manager, &tip, |ws| {
        for (command, output, root) in failed {
            // One failure tied to the attempt fails it; the rest need not run.
            if out.against.is_some() {
                break;
            }
            let tip_run = run_confined(
                ws,
                manager.repo(),
                denies,
                &command,
                timeout,
                DEPENDENT_OUTPUT_CAP,
                warm,
            )
            .map(|(ok, o)| (ok, o, ws.root().to_path_buf()));
            judge_dependent(
                &mut out, contract, &short, writes, open, command, output, root, tip_run,
            );
        }
    });
    // The worktree could not be made (nothing was judged) or removed.
    if let Err(e) = ran {
        out.notices.push(format!(
            "contract {}: `{}` failed with this attempt; the throwaway worktree at the \
             integration tip {short}, which tells whether the attempt causes it, failed: {e}; \
             the attempt is not failed for what was not judged",
            clean(contract),
            failing.join("`, `")
        ));
    }
    out
}

/// Judge one dependent's check that failed with the attempt, by its run at
/// the integration tip ([`check_dependents`]).
#[allow(clippy::too_many_arguments)]
fn judge_dependent(
    out: &mut DependentsOutcome,
    contract: &str,
    short: &str,
    writes: &[&str],
    open: &[&str],
    command: String,
    output: String,
    root: PathBuf,
    tip_run: Result<(bool, String, PathBuf), String>,
) {
    let quoted = clean_lines(&output);
    let own = match &tip_run {
        Ok((false, tip_out, tip_root)) => {
            own_file_failure(&output, &[root.as_path()], tip_out, tip_root, writes, open)
        }
        _ => None,
    };
    match (tip_run, own) {
        (Ok((true, _, _)), _) => {
            out.against = Some(format!(
                "`{}`, the check of a package that depends on this contract's packages, \
                 fails with this attempt and passes without it (at the integration tip \
                 {short}), so this attempt's change breaks it: {quoted}",
                clean(&command)
            ));
        }
        (Ok((false, _, _)), Some(line)) => {
            out.against = Some(format!(
                "`{}`, the check of a package that depends on this contract's packages, \
                 fails with this attempt, and the line `{}` names a file this contract \
                 writes, so the failure is in this contract's own work: {quoted}",
                clean(&command),
                clean(&line)
            ));
        }
        (Ok(_), _) => out.notices.push(format!(
            "contract {}: `{}`, the check of a package that depends on its packages, fails \
             with this attempt, and the failure could not be tied to this contract's files \
             (it fails at the integration tip {short} too, and names no file this contract \
             writes); the attempt is not failed for it, and the root gate judges the merged \
             result. It said: {}",
            clean(contract),
            clean(&command),
            quoted
                .chars()
                .take(DEFERRED_OUTPUT_CHARS)
                .collect::<String>()
        )),
        (Err(e), _) => out.notices.push(format!(
            "contract {}: `{}`, the check of a package that depends on its packages, fails \
             with this attempt, and it could not be run at the integration tip {short} to \
             tell whether the attempt causes it: {e}; the attempt is not failed for it. It \
             said: {}",
            clean(contract),
            clean(&command),
            quoted
                .chars()
                .take(DEFERRED_OUTPUT_CHARS)
                .collect::<String>()
        )),
    }
}

/// Run `f` in a throwaway worktree of `commit`, removed on every path.
fn with_worktree(
    manager: &kgraf_sandbox::WorkspaceManager,
    commit: &str,
    f: impl FnOnce(&kgraf_sandbox::Workspace),
) -> Result<(), String> {
    let name = crate::workspaces::harness_worktree_name(crate::workspaces::PROBE_PREFIX);
    let ws = manager
        .create(&kgraf_sandbox::WorkspaceSpec {
            name: &name,
            base: commit,
            branch: None,
        })
        .map_err(|e| format!("cannot create a worktree at {commit}: {e}"))?;
    f(&ws);
    manager
        .remove(ws)
        .map_err(|e| format!("cannot remove the worktree at {commit}: {e}"))
}

/// The verdict's `by` with the dependents' checks that passed, and those the
/// cap left out, so the record says what judged the attempt.
fn by_with_dependents(by: String, outcome: &DependentsOutcome) -> String {
    if outcome.passed.is_empty() && outcome.over_cap.is_empty() {
        return by;
    }
    let mut parts = Vec::new();
    if !outcome.passed.is_empty() {
        parts.push(format!("dependents passed: {}", outcome.passed.join(", ")));
    }
    if !outcome.over_cap.is_empty() {
        parts.push(format!(
            "dependents not run, at most {} per attempt: {}",
            crate::scoped_check::DEPENDENTS_CAP,
            outcome.over_cap.join(", ")
        ));
    }
    format!("{by} ({})", parts.join("; "))
}

/// What a worker checked by the whole command is told about deferral.
pub(crate) const DEFERRED_BRIEF: &str = "If that command can only pass once other contracts \
     of the plan have merged (it also fails on the tree you were given), it is checked on the \
     merged result instead; do your own files' part and do not write files of other contracts \
     to make it pass. A change of yours that makes it fail where it passed before is still a \
     failure.";

/// Characters of a deferred check's failing output quoted in its notice.
const DEFERRED_OUTPUT_CHARS: usize = 300;

/// The notice of an attempt whose check was deferred to the root gate
/// (decision 0018, amended 2026-10-02 and 2026-10-10). Named per contract:
/// the run's notices are a set.
fn deferred_notice(contract: &str, command: &str, waits: &Waits, failed: &str) -> String {
    let failed: String = failed.trim().chars().take(DEFERRED_OUTPUT_CHARS).collect();
    format!(
        "contract {}: `{}` is deferred to the root gate: it failed on this attempt and also at \
         its dispatch base, and it needs the work of {} not merged yet, so the failure cannot \
         be told apart from their missing work. The attempt passed its scope and placeholder \
         checks and merges; the whole command runs on the merged result before the run can \
         complete. It said: {}",
        clean(contract),
        clean(command),
        clean(&waits.describe()),
        clean(&failed)
    )
}

/// Files of a waited-on contract named in a notice or an answer, at most.
const NAMED_FILES: usize = 6;

/// The open contracts of the plan whose work an attempt's check needs, so a
/// failure of that check, on the attempt and at its dispatch base alike,
/// cannot be told apart from their missing work.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Waits {
    /// The contracts, in plan order.
    contracts: Vec<String>,
    /// For a narrowed check, the files it needs that the tree lacks, each
    /// with its contract. Empty for a whole check, which needs every open
    /// contract's work.
    files: Vec<(String, String)>,
}

impl Waits {
    /// What `scope`, in the attempt's tree at `root`, waits on among
    /// `unmerged`. A whole check judges the whole instruction, so it waits on
    /// every open contract. A narrowed check waits only on a file the tree
    /// lacks that it needs ([`crate::scoped_check::needs_absent`]): a contract
    /// whose narrowed tests fail on its own stub, while unrelated contracts
    /// are open, is failed as before.
    pub(crate) fn of(
        whole: &str,
        scope: &crate::scoped_check::Scope,
        root: &Path,
        unmerged: &[OpenContract],
    ) -> Self {
        let selected = match scope {
            crate::scoped_check::Scope::Whole { .. } => {
                return Self {
                    contracts: unmerged.iter().map(|o| o.contract.clone()).collect(),
                    files: Vec::new(),
                };
            }
            crate::scoped_check::Scope::Narrowed { selected, .. } => selected,
        };
        let mut waits = Self::default();
        for o in unmerged {
            for w in &o.writes {
                let path = w.path.trim_start_matches("./");
                // Absent, read without following a link: a file the tree
                // holds is not the missing work of another contract.
                if root.join(path).symlink_metadata().is_ok()
                    || !crate::scoped_check::needs_absent(whole, root, selected, path)
                {
                    continue;
                }
                if !waits.contracts.contains(&o.contract) {
                    waits.contracts.push(o.contract.clone());
                }
                waits.files.push((o.contract.clone(), path.to_owned()));
            }
        }
        waits
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.contracts.is_empty()
    }

    /// "contracts c1, c2", or, for a narrowed check, the files it lacks:
    /// "`a/src/lib.rs` of c1, `b/src/main.rs` of c2".
    pub(crate) fn describe(&self) -> String {
        if self.files.is_empty() {
            return format!("contracts {}", self.contracts.join(", "));
        }
        let mut named: Vec<String> = self
            .files
            .iter()
            .take(NAMED_FILES)
            .map(|(c, p)| format!("`{p}` of {c}"))
            .collect();
        if self.files.len() > NAMED_FILES {
            named.push(format!("{} more", self.files.len() - NAMED_FILES));
        }
        format!(
            "contracts {} ({})",
            self.contracts.join(", "),
            named.join(", ")
        )
    }
}

/// The refusal of an attempt that changed none of its declared files.
fn unchanged_writes_reason(command: &str, writes: &[&str]) -> String {
    format!(
        "the attempt changed none of its declared files ({}); `{}` passed on the tree exactly as \
         it was dispatched, which proves nothing about the work. Make the change the contract \
         asks for",
        clean(&writes.join(", ")),
        clean(command)
    )
}

/// The check an attempt at `contract` runs in the tree at `root`: the tests of
/// its declared files, or the whole command. A gate remediation always runs
/// the whole command: it fixes what the merged result failed.
fn check_scope(whole: &str, contract: &OpenContract, root: &Path) -> crate::scoped_check::Scope {
    if !contract.gate_commands.is_empty() {
        return crate::scoped_check::Scope::Whole {
            why: "a gate remediation is checked with the whole command".into(),
        };
    }
    let writes: Vec<&str> = contract.writes.iter().map(|w| w.path.as_str()).collect();
    crate::scoped_check::scope(whole, root, &writes)
}

/// Whether `c` can change how a terminal or a reader shows the text around
/// it: a control character (an escape sequence, a newline, a NUL), a
/// line or paragraph separator (U+2028, U+2029), a zero-width or
/// direction mark, a bidirectional override or isolate, or a byte order mark.
fn is_display_altering(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{061C}'
                | '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{2069}'
                | '\u{FEFF}'
        )
}

/// `text` with every display-altering character ([`is_display_altering`])
/// replaced by `?`. A file name a worker created can hold them, and a notice
/// is printed to a terminal and stored in the run record.
pub(crate) fn clean(text: &str) -> String {
    text.chars()
        .map(|c| if is_display_altering(c) { '?' } else { c })
        .collect()
}

/// Whether `c` can repaint a terminal or reorder the lines of multi-line
/// output: a control character (an escape sequence), a line or paragraph
/// separator, or a bidirectional override or isolate. Narrower than
/// [`is_display_altering`]: the zero-width joiner and non-joiner (U+200D,
/// U+200C) move no text, and Persian and Indic words and emoji sequences in a
/// command's output need them.
fn is_line_altering(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{2028}' | '\u{2029}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
        )
}

/// [`clean`] for multi-line text (a command's output): line breaks and tabs
/// stay, and so do joiners and other marks that move no text; each
/// [`is_line_altering`] character becomes `?`.
pub(crate) fn clean_lines(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c != '\n' && c != '\t' && is_line_altering(c) {
                '?'
            } else {
                c
            }
        })
        .collect()
}

/// The most bytes the placeholder check reads of one declared write target. A
/// larger file is real code, never a placeholder.
const MAX_PLACEHOLDER_READ: u64 = 4 * 1024 * 1024;

/// Reads one declared write target from the still-live worktree, for the
/// placeholder check below. This only ever runs on an attempt merge-back
/// reported `CommitOutcome::Empty`, so the worktree's content at this path is
/// exactly what it held at dispatch, changed or not.
///
/// The path is worker-controlled ground (decision 0083): no component of it
/// may be a link, the leaf is opened with `O_NOFOLLOW | O_NONBLOCK` so a named
/// pipe never blocks the verifier, and only a regular file with one link is
/// read.
fn read_write_target(ws_root: &Path, path: &str) -> Option<String> {
    use std::io::Read;
    let mut walked = ws_root.to_path_buf();
    for part in Path::new(path).components() {
        walked.push(part);
        if std::fs::symlink_metadata(&walked)
            .ok()?
            .file_type()
            .is_symlink()
        {
            return None;
        }
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = opts.open(&walked).ok()?;
    let meta = file.metadata().ok()?;
    #[cfg(unix)]
    let linked = {
        use std::os::unix::fs::MetadataExt;
        meta.nlink() > 1
    };
    #[cfg(not(unix))]
    let linked = false;
    if !meta.is_file() || linked || meta.len() > MAX_PLACEHOLDER_READ {
        return None;
    }
    let mut bytes = Vec::new();
    // Bounded again at the read: the file can grow after the metadata call.
    (&file)
        .take(MAX_PLACEHOLDER_READ + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_PLACEHOLDER_READ {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// The declared write targets as the dispatch base held them, read from the
/// repository's object store (never from the worker's tree), in two git
/// processes however many paths there are: one `ls-tree` naming each path's
/// mode, blob and size, then one `cat-file --batch` for the regular files small
/// enough to read. A path maps to `None` when the base has no regular file
/// there, the file is over the read cap, or it is not UTF-8. A git failure is
/// an error: reading it as "absent" would skip the placeholder check silently.
fn base_write_targets(
    repo: &Path,
    base: &str,
    paths: &[&str],
) -> Result<std::collections::BTreeMap<String, Option<String>>, String> {
    let mut found: std::collections::BTreeMap<String, Option<String>> =
        paths.iter().map(|p| ((*p).to_owned(), None)).collect();
    if paths.is_empty() {
        return Ok(found);
    }
    let git = kgraf_sandbox::git::Git::discover().map_err(|e| e.to_string())?;
    let mut args = vec!["ls-tree", "-l", "-z", base, "--"];
    args.extend_from_slice(paths);
    // `ls-tree` succeeds with no output for a path the base lacks, so a
    // failure here is git failing, not an absent file.
    let listing = git
        .run(repo, args)
        .map_err(|e| format!("cannot list the write targets at {base}: {e}"))?;
    // "<mode> blob <sha> <size>\t<path>\0": only a regular file (100644,
    // 100755) no larger than the read cap is read.
    let mut wanted: Vec<(String, String)> = Vec::new();
    for entry in listing.split(|b| *b == 0).filter(|e| !e.is_empty()) {
        let entry = String::from_utf8_lossy(entry);
        let Some((meta, path)) = entry.split_once('\t') else {
            continue;
        };
        let mut fields = meta.split_whitespace();
        let (Some(mode), Some(kind), Some(sha), Some(size)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let small = size.parse::<u64>().is_ok_and(|n| n <= MAX_PLACEHOLDER_READ);
        if mode.starts_with("100") && kind == "blob" && small && found.contains_key(path) {
            wanted.push((path.to_owned(), sha.to_owned()));
        }
    }
    if wanted.is_empty() {
        return Ok(found);
    }
    let request: String = wanted.iter().map(|(_, sha)| format!("{sha}\n")).collect();
    let reply = git
        .run_with_stdin(repo, ["cat-file", "--batch"], request.as_bytes())
        .map_err(|e| format!("cannot read the write targets at {base}: {e}"))?;
    // Replies come in request order: "<sha> blob <size>\n<content>\n".
    let mut at = 0usize;
    for (path, sha) in wanted {
        let malformed = || {
            format!(
                "cannot read `{}` at {base}: malformed git reply",
                clean(&path)
            )
        };
        let line_end = reply[at..]
            .iter()
            .position(|b| *b == b'\n')
            .map(|n| at + n)
            .ok_or_else(malformed)?;
        let header = String::from_utf8_lossy(&reply[at..line_end]).into_owned();
        let mut fields = header.split_whitespace();
        let (Some(got), Some("blob"), Some(size)) = (fields.next(), fields.next(), fields.next())
        else {
            return Err(format!(
                "cannot read `{}` at {base}: {header}",
                clean(&path)
            ));
        };
        let size: usize = size.parse().map_err(|_| malformed())?;
        let (start, end) = (line_end + 1, line_end + 1 + size);
        if got != sha || end > reply.len() {
            return Err(malformed());
        }
        found.insert(path, String::from_utf8(reply[start..end].to_vec()).ok());
        at = end + 1;
    }
    Ok(found)
}

/// The declared write targets a committed attempt left untouched
/// (`changed` is the commit's diff against the base) that held only a
/// placeholder body at dispatch (`read` returns the base content).
pub(crate) fn untouched_placeholders<'a>(
    contract: &'a OpenContract,
    changed: &[String],
    read: impl Fn(&str) -> Option<String>,
) -> Vec<&'a str> {
    let untouched: Vec<&str> = contract
        .writes
        .iter()
        .map(|w| w.path.as_str())
        .filter(|w| {
            !changed
                .iter()
                .any(|c| same_path(Path::new(c), Path::new(w)))
        })
        .collect();
    placeholder_writes_left_in_place(untouched, read)
}

fn declared(contract: &OpenContract, p: &Path) -> bool {
    contract
        .writes
        .iter()
        .any(|w| same_path(Path::new(&w.path), p))
}

/// `bypass_git_files`: the mode is Bypass, so `.gitattributes`,
/// `.gitmodules` and `.githooks` count as ordinary files
/// (`kgraf_core::paths::is_bypass_git_file`).
pub(crate) fn scope_violations(
    changed: &[String],
    contract: &OpenContract,
    bypass_git_files: bool,
) -> Vec<String> {
    changed
        .iter()
        .filter(|p| {
            let p = Path::new(p.as_str());
            let protected = forbidden_path(p)
                && !(bypass_git_files && kgraf_core::paths::is_bypass_git_file(p));
            protected || !declared(contract, p)
        })
        .cloned()
        .collect()
}

/// The changed paths that are build-tool configuration (cargo, rustup, and the
/// other build tools and version managers: [`is_build_config_path`]),
/// declared or not. Such a change is never merged: once in the checkout, the
/// host's own cargo reads it (the dependency fetch of decision 0087 runs with
/// the checkout's configuration), and so does every later worker's build and
/// the user's own tools. Decision 0084's
/// tripwire already refuses builds in the worktree that changed them; this
/// refuses the attempt itself, so the change never reaches the checkout.
pub(crate) fn build_config_changes(changed: &[String]) -> Vec<String> {
    changed
        .iter()
        .filter(|p| is_build_config_path(Path::new(p.as_str())))
        .cloned()
        .collect()
}

/// The verdict's words for [`build_config_changes`], for the model and the
/// user alike.
pub(crate) fn build_config_reason(paths: &[String]) -> String {
    format!(
        "the attempt changes build-tool configuration ({paths:?}), which kGraf never merges: \
         a `.cargo` folder, a `rust-toolchain` file, or another build tool's or version \
         manager's configuration (`.npmrc`, `.mvn/`, `gradle.properties`, `NuGet.Config`, \
         `.nvmrc`, ...) changes what every later build, and the host's own tools, run. Leave \
         these files as they are and solve the task without them; if the project needs such a \
         change, say so in your result so the user can make it by hand"
    )
}

/// Paths named in a notice, at most this many, then a count.
const NOTICE_PATHS: usize = 5;

fn some_paths(paths: &[&Path]) -> String {
    let mut text = paths
        .iter()
        .take(NOTICE_PATHS)
        .map(|p| clean(&p.display().to_string()))
        .collect::<Vec<_>>()
        .join(", ");
    if paths.len() > NOTICE_PATHS {
        text.push_str(&format!(", and {} more", paths.len() - NOTICE_PATHS));
    }
    text
}

/// The owner's notices for what the worker's own run generated and the commit
/// left out: one per lockfile, one for all bytecode, one for all of a Python
/// package build's output, one for all other build or install output, one for
/// all tool caches.
pub(crate) fn worker_notices(excluded: &[Excluded]) -> Vec<String> {
    let mut notices = Vec::new();
    let mut bytecode = Vec::new();
    let mut package = Vec::new();
    let mut output = Vec::new();
    let mut cache = Vec::new();
    // The links kGraf made to the checkout's installed dependencies are
    // neither the worker's files nor the check's.
    for e in excluded
        .iter()
        .filter(|e| !kgraf_sandbox::is_dependency_link(&e.path))
    {
        match e.kind {
            GeneratedKind::Lockfile => notices.push(format!(
                "the worker's run generated {} (made by the worker), which the project does not \
                 commit; it was left out of the attempt and removed before the check. Commit it \
                 in the project to make builds reproducible",
                clean(&e.path.display().to_string())
            )),
            GeneratedKind::PythonBytecode => bytecode.push(e.path.as_path()),
            GeneratedKind::PackageBuild => package.push(e.path.as_path()),
            GeneratedKind::BuildOutput => output.push(e.path.as_path()),
            GeneratedKind::ToolCache => cache.push(e.path.as_path()),
        }
    }
    if !output.is_empty() {
        notices.push(format!(
            "the worker's run left {} build or install output file(s) (made by the worker): {}; \
             they were left out of the attempt and removed before the check. Add their folders \
             to .gitignore",
            output.len(),
            some_paths(&output)
        ));
    }
    if !cache.is_empty() {
        notices.push(format!(
            "the worker's run left {} tool cache file(s) (made by the worker): {}; they were left \
             out of the attempt and removed before the check. Add them to .gitignore",
            cache.len(),
            some_paths(&cache)
        ));
    }
    if !package.is_empty() {
        notices.push(format!(
            "the worker's run left {} Python package build file(s) (made by the worker): {}; \
             they were left out of the attempt and removed before the check. Add dist/, build/ \
             and *.egg-info/ to .gitignore",
            package.len(),
            some_paths(&package)
        ));
    }
    if !bytecode.is_empty() {
        notices.push(format!(
            "the worker's run left {} Python bytecode file(s) (made by the worker): {}; they were \
             left out of the attempt and removed before the check. Add __pycache__/ to .gitignore",
            bytecode.len(),
            some_paths(&bytecode)
        ));
    }
    notices
}

/// The owner's notices for the files the check created: one per lockfile (the
/// owner acts on each), one for the rest. Paths the commit already left out as
/// the worker's (removed before the check, which may generate it again) are
/// reported once,
/// by [`worker_notices`].
pub(crate) fn check_notices(command: &str, left: &Leftovers, excluded: &[Excluded]) -> Vec<String> {
    let mut notices = Vec::new();
    let created: Vec<&Path> = left
        .created
        .iter()
        .map(PathBuf::as_path)
        .filter(|p| !excluded.iter().any(|e| e.path == *p))
        .filter(|p| !kgraf_sandbox::is_dependency_link(p))
        .collect();
    let (locks, other): (Vec<&Path>, Vec<&Path>) = created
        .into_iter()
        .partition(|p| lockfile_manifests(p).is_some());
    for lock in locks {
        notices.push(format!(
            "the check `{command}` generated {}, which the project does not commit; it was not \
             merged. Commit it in the project to make builds reproducible",
            clean(&lock.display().to_string())
        ));
    }
    if !other.is_empty() {
        notices.push(format!(
            "the check `{command}` created {} file(s) that git would stage: {}; they were not \
             merged. Add them to .gitignore, or commit them in the project",
            other.len(),
            some_paths(&other)
        ));
    }
    notices
}

/// A check that changed committed files checked a tree that is not the one
/// that would be merged.
fn changed_reason(command: &str, changed: &[PathBuf]) -> String {
    let paths: Vec<&Path> = changed.iter().map(PathBuf::as_path).collect();
    format!(
        "the check `{command}` changed or deleted {} committed file(s): {}; the merged tree would \
         differ from the tree it checked. The files were restored; make the check leave committed \
         files alone, or commit what it writes",
        paths.len(),
        some_paths(&paths)
    )
}

#[async_trait::async_trait]
impl Verifier for CommandVerifier {
    async fn verify(
        &self,
        contract: &OpenContract,
        workspace: &Path,
        result: &WorkerResult,
    ) -> Result<Verdict, String> {
        self.verify_among(contract, workspace, result, &[]).await
    }

    async fn verify_among(
        &self,
        contract: &OpenContract,
        workspace: &Path,
        _result: &WorkerResult,
        unmerged: &[OpenContract],
    ) -> Result<Verdict, String> {
        // Only with the whole command also a root gate may a check be
        // deferred: a deferred check must still run before the run can
        // complete. Without one, a check that waits on open contracts fails
        // and says why it cannot judge this contract alone.
        let root_gate = self.root_gate;
        let zero_test_plans = self.zero_test_plans;
        let unmerged = unmerged.to_vec();
        let Some(ws) = self.workspaces.get(workspace) else {
            return Err(format!(
                "{} is not a live worktree of this run",
                workspace.display()
            ));
        };
        let whole = self.command.clone();
        let denies = self.read_denies.clone();
        let merge_back = self.merge_back.clone();
        let contract = contract.clone();
        let workspaces = self.workspaces.clone();
        let attempt_root = workspace.to_path_buf();
        let timeout = self.timeout;
        let warm = warm_build_folder(&self.repo);
        // Read once, at the start: the commit and the scope check judge the
        // attempt under one mode.
        let bypass_git_files =
            self.permissions.mode() == kgraf_tools::permissions::PermissionMode::Bypass;
        tokio::task::spawn_blocking(move || {
            let manager = workspaces.manager.clone();
            let root = attempt_root.clone();
            // 1. Nothing the worker started still runs; held through the commit.
            // A process that dropped its descriptors and left its session is
            // not seen off Linux PID namespaces (decision 0084, known gap);
            // what it writes after the commit below is not in the attempt.
            let quiet = match quiesce_retrying(&manager, &ws) {
                Ok(q) => q,
                Err(e) => {
                    return Ok(Verdict::Fail {
                        reason: format!(
                            "a process the worker started still runs in its worktree, so the \
                             tree is not final: {e}"
                        ),
                    });
                }
            };
            // 2. The snapshot: commit through merge-back, before the check can
            // write anything. An empty attempt keeps its worktree for the check.
            // Numbered per verification from the record's persisted counter: a
            // worktree verified again never collides with an earlier attempt ref.
            let mut attempt =
                match merge_back.attempt_numbered(ws.clone(), contract.verifications.max(1)) {
                    Ok(a) => a,
                    Err(e) => {
                        return Ok(Verdict::Fail {
                            reason: format!("merge-back: {e}"),
                        });
                    }
                };
            attempt.allow_bypass_git_files(bypass_git_files);
            let message = format!("kgraf: contract {}", contract.contract);
            let in_scope = |p: &Path| declared(&contract, p);
            let commit = match merge_back.commit_retaining(&mut attempt, &message, &in_scope) {
                Ok(CommitOutcome::Committed { commit }) => Some(commit),
                Ok(CommitOutcome::Empty) => None,
                Ok(CommitOutcome::Refused(r)) => {
                    return Ok(Verdict::Fail {
                        reason: format!("merge-back refused the attempt: {:?}", r.reasons),
                    });
                }
                Err(e) => {
                    return Ok(Verdict::Fail {
                        reason: format!("merge-back could not commit the attempt: {e}"),
                    });
                }
            };
            drop(quiet);
            // A committed attempt that fails from here on is kept for discard.
            let fail = |attempt, reason: String| {
                if commit.is_some() {
                    workspaces.committed().insert(root.clone(), attempt);
                }
                Ok(Verdict::Fail { reason })
            };
            // 3. Scope, base to the snapshot: only what the worker changed.
            if commit.is_some() {
                let changed = match merge_back.changed_paths(&attempt) {
                    Ok(c) => c,
                    Err(e) => {
                        workspaces.committed().insert(root.clone(), attempt);
                        return Err(format!("cannot read the attempt's changes: {e}"));
                    }
                };
                let outside = scope_violations(&changed, &contract, bypass_git_files);
                if !outside.is_empty() {
                    // The check does not run on a refused attempt.
                    return fail(
                        attempt,
                        format!("changes outside the declared write scope: {outside:?}"),
                    );
                }
                let config = build_config_changes(&changed);
                if !config.is_empty() {
                    return fail(attempt, build_config_reason(&config));
                }
                // A declared target the commit left untouched that held only a
                // placeholder at dispatch is a skipped part of the work, even
                // though other files changed (decision 0086).
                let base = ws.head().to_owned();
                // A git failure while reading a base file is not "absent": it
                // fails the verification rather than skipping the check.
                let declared: Vec<&str> = contract.writes.iter().map(|w| w.path.as_str()).collect();
                let held = match base_write_targets(manager.repo(), &base, &declared) {
                    Ok(held) => held,
                    Err(e) => {
                        workspaces.committed().insert(root.clone(), attempt);
                        return Err(format!("cannot check the placeholder targets: {e}"));
                    }
                };
                let skipped =
                    untouched_placeholders(&contract, &changed, |p| held.get(p).cloned().flatten());
                if !skipped.is_empty() {
                    return fail(attempt, placeholder_left_in_place_reason(&skipped));
                }
            }
            // The check runs against the committed tree only: what the commit
            // left out (the worker's lockfile, its bytecode) is removed first.
            if let Err(e) = merge_back.remove_excluded(&attempt) {
                return fail(
                    attempt,
                    format!("cannot remove the files the worker's run generated: {e}"),
                );
            }
            // Nor anything git ignores that the worker's run left: a build
            // tool's records of what it built would let the check skip the
            // build or the test of the committed sources (decision 0084,
            // amended 2026-10-06). The dependency links are made again.
            if let Err(e) = merge_back.remove_ignored(&attempt) {
                return fail(
                    attempt,
                    format!("cannot remove the ignored files the worker's run left: {e}"),
                );
            }
            // 4. The acceptance command, confined, in the worker's tree; its
            // process group is killed when its shell exits. Narrowed to the
            // tests of the contract's files (decision 0018 amendment): the
            // whole command runs as the root gate on the merged result.
            let scope = check_scope(&whole, &contract, ws.root());
            let command = scope.command(&whole);
            let (mut passed, mut output) = run_confined(
                &ws,
                manager.repo(),
                &denies,
                &command,
                timeout,
                CHECK_OUTPUT_CAP,
                warm.as_deref(),
            )?;
            // A gate remediation must also pass the failing gate's own
            // commands: the run's check alone can pass on the code the gate
            // failed on, which would accept a remediation that fixed nothing.
            let mut failing = command.clone();
            for gate_command in contract.gate_commands.iter().filter(|_| passed) {
                let (ok, out) = run_confined(
                    &ws,
                    manager.repo(),
                    &denies,
                    gate_command,
                    timeout,
                    CHECK_OUTPUT_CAP,
                    warm.as_deref(),
                )?;
                if !ok {
                    (passed, output, failing) = (false, out, gate_command.clone());
                    break;
                }
            }
            // Decision 0100: the contract's own checks run only where the
            // run's check tests nothing, the whole command (`true`) or a
            // narrowed cargo check that selects no test, so a contract the
            // run's check does judge is judged as before. A failure fails the
            // attempt with the check's output; a pass is marked in the
            // verdict, and the orchestrator counts it as a check beside the
            // vacuous one. Run before the dependents' checks below, so a
            // contract whose own check ran is judged by it whether or not a
            // dependent package also tests it.
            let mut own_checked: Vec<String> = Vec::new();
            if passed && commit.is_some() && kgraf_orchestrator::own_checks_real(&contract) {
                let writes: Vec<&str> = contract.writes.iter().map(|w| w.path.as_str()).collect();
                let zero_test = (zero_test_plans
                    || kgraf_orchestrator::triage::is_small_contract(&contract))
                    && crate::scoped_check::cargo_selects_no_tests(&whole, ws.root(), &writes)
                        .is_some();
                if kgraf_orchestrator::vacuous_check(&command) || zero_test {
                    for check in &contract.contract_checks {
                        let (ok, out) = run_confined(
                            &ws,
                            manager.repo(),
                            &denies,
                            check,
                            timeout,
                            CHECK_OUTPUT_CAP,
                            warm.as_deref(),
                        )?;
                        if !ok {
                            (passed, output, failing) = (false, out, check.clone());
                            own_checked.clear();
                            break;
                        }
                        own_checked.push(check.clone());
                    }
                }
            }
            // Decision 0018, amended 2026-10-02: the whole command judges the
            // whole instruction. On an attempt whose tree lacks other open
            // contracts of the plan, it can fail on their missing work, and
            // no attempt of a contract they depend on could ever pass. The
            // command runs again at the dispatch base: if it passes there,
            // this attempt's change broke it, a plain failure. If it fails
            // there too, the failure cannot be told apart from the missing
            // work, so the check is deferred to the root gate, which runs
            // the same command on the merged result before the run can
            // complete. Only a committed (non-empty) attempt, whose scope and
            // placeholder checks above passed, is deferred.
            //
            // The run at the dispatch base is made for every such failure,
            // not only when contracts are open: for the plan's last contract
            // nothing is deferred, but whether the base already fails is what
            // tells an earlier delivery's break from this attempt's own. The
            // verdict stays a failure; its reason says which
            // (`base_attribution`).
            //
            // Amended 2026-10-10: a narrowed check is treated the same way
            // when it needs files open contracts write that the tree lacks
            // (`Waits`). A contract that writes only a new workspace's
            // manifests has its check narrowed to the new packages, which
            // cannot build before their sources merge; it was failed for
            // their absence on every attempt. A narrowed check that needs
            // no such file runs at the base only as before: never.
            let mut deferred: Option<String> = None;
            let waits = Waits::of(&whole, &scope, ws.root(), &unmerged);
            let at_base = !passed
                && failing == command
                && commit.is_some()
                && contract.gate_commands.is_empty()
                && (matches!(scope, crate::scoped_check::Scope::Whole { .. }) || !waits.is_empty());
            if at_base {
                let writes: Vec<&str> = contract.writes.iter().map(|w| w.path.as_str()).collect();
                let others: Vec<&str> = unmerged
                    .iter()
                    .flat_map(|o| o.writes.iter().map(|w| w.path.as_str()))
                    .collect();
                let ran = run_at_rooted(&manager, ws.head(), &denies, &command, timeout);
                // Amended 2026-10-11: a failure that names a file this
                // contract wrote, and that the dispatch base did not show,
                // is this contract's own defect. It is never deferred: the
                // worker gets the error while it can still fix it.
                let own = match &ran {
                    Ok((false, base_out, base_root)) if !waits.is_empty() => own_file_failure(
                        &output,
                        &[ws.root(), attempt_root.as_path()],
                        base_out,
                        base_root,
                        &writes,
                        &others,
                    ),
                    _ => None,
                };
                match ran.map(|(passed, _, _)| passed) {
                    Ok(false) if own.is_some() => output.push_str(&format!(
                        "\n(it also fails at the dispatch base, but not with this error: the \
                         line `{}` names a file this contract writes, so the failure is in \
                         this contract's own work and it is not deferred)",
                        clean(own.as_deref().unwrap_or_default())
                    )),
                    Ok(false) if root_gate && !waits.is_empty() => {
                        deferred = Some(clean_lines(&output));
                        passed = true;
                    }
                    Ok(false) if !waits.is_empty() => output.push_str(&format!(
                        "\n(it also fails at the dispatch base, and it needs the work of {} not \
                         merged yet: it cannot judge this contract alone, and the run has no \
                         root gate to defer it to, so it fails until that work merges first)",
                        waits.describe()
                    )),
                    Ok(false) => output.push_str(&base_attribution(
                        &manager,
                        merge_back.integration_branch(),
                        ws.head(),
                        &denies,
                        &command,
                        timeout,
                    )),
                    Ok(true) => output.push_str(
                        "\n(it passes at the dispatch base, so this attempt's change makes it \
                         fail)",
                    ),
                    Err(e) if !waits.is_empty() => output.push_str(&format!(
                        "\n(it could not be run at the dispatch base to tell whether the missing \
                         work of {}, not merged yet, causes the failure: {e})",
                        waits.describe()
                    )),
                    Err(e) => output.push_str(&format!(
                        "\n(it could not be run at the dispatch base to tell whether an \
                         earlier merge of this run causes the failure: {e})"
                    )),
                }
            }
            // Work merged after the dispatch base: contracts that merged while
            // this attempt ran are on the integration tip, not in its tree,
            // so a check that needs their work fails on the attempt alone,
            // though merge-back lands the attempt on the tip. The check runs
            // once more on the attempt merged onto the tip, in a throwaway
            // worktree: passing there passes the attempt (that tree is what
            // merges, and the root gate still runs on the merged result);
            // failing there, its output replaces the attempt's, since what is
            // still wrong on the tree that would land is what a retry, which
            // starts from the tip, must fix.
            let mut on_tip_passed: Option<String> = None;
            if !passed
                && failing == command
                && deferred.is_none()
                && contract.gate_commands.is_empty()
                && let Some(sha) = &commit
            {
                match check_on_tip(
                    &manager,
                    &merge_back,
                    ws.head(),
                    sha,
                    &denies,
                    &command,
                    timeout,
                ) {
                    OnTip::NotMoved => {}
                    OnTip::Passed { tip } => {
                        passed = true;
                        on_tip_passed = Some(tip);
                    }
                    OnTip::Failed { output: merged } => output = merged,
                    OnTip::NotRun { note } => output.push_str(&note),
                }
            }
            // 5. Quiet again, held through the cleanup. A process that escaped
            // the group fails the verification and nothing is cleaned: the
            // worktree is left as it is, never merged, never continued.
            let quiet = match quiesce_retrying(&manager, &ws) {
                Ok(q) => q,
                Err(e) => {
                    return Ok(Verdict::Fail {
                        reason: format!(
                            "the check `{command}` left a process running in the worktree after \
                             its process group was killed: {e}"
                        ),
                    });
                }
            };
            // 6. What the check wrote: removed whether it passed or failed (a
            // worktree kept for a continued worker must not carry it).
            let cleared = merge_back.discard_leftovers(&attempt);
            drop(quiet);
            if !passed {
                let mut reason = format!("`{}` failed: {}", clean(&failing), clean_lines(&output));
                if let crate::scoped_check::Scope::Narrowed { selected, .. } = &scope {
                    reason.push_str(&format!(
                        " (the tests of the contract's files: {})",
                        clean(&selected.join(", "))
                    ));
                }
                if let Err(e) = &cleared {
                    reason.push_str(&format!("; also, what it left could not be removed: {e}"));
                }
                // The worker's files stay in the worktree for inspection (its
                // attempt ref is swept at the end of the run); a failed
                // attempt is never continued in it.
                return Ok(Verdict::Fail { reason });
            }
            let left = match cleared {
                Ok(left) => left,
                Err(e) => {
                    return fail(
                        attempt,
                        format!("cannot remove what `{command}` left in the worktree: {e}"),
                    );
                }
            };
            if !left.changed.is_empty() {
                return fail(attempt, changed_reason(&command, &left.changed));
            }
            // Decision 0018, amended 2026-10-12: the earlier behavior
            // signal. A narrowed check runs only the contract's own package,
            // which often has no tests, so a semantic defect surfaced only at
            // the root gate. The packages that depend on this one and can
            // build now run too; only a failure tied to this attempt fails it.
            let dependents = match (&commit, &scope) {
                (Some(sha), crate::scoped_check::Scope::Narrowed { .. })
                    if deferred.is_none() && contract.gate_commands.is_empty() =>
                {
                    let writes: Vec<&str> =
                        contract.writes.iter().map(|w| w.path.as_str()).collect();
                    let open: Vec<&str> = unmerged
                        .iter()
                        .filter(|o| o.contract != contract.contract)
                        .flat_map(|o| o.writes.iter().map(|w| w.path.as_str()))
                        .collect();
                    check_dependents(
                        &manager,
                        &merge_back,
                        &contract.contract,
                        sha,
                        ws.head(),
                        ws.root(),
                        &denies,
                        &whole,
                        &writes,
                        &open,
                        timeout,
                        warm.as_deref(),
                    )
                }
                _ => DependentsOutcome::default(),
            };
            if let Some(reason) = dependents.against.clone() {
                return fail(
                    attempt,
                    format!("`{}` passed, but {reason}", clean(&command)),
                );
            }
            let mut notices = worker_notices(attempt.excluded());
            notices.extend(dependents.notices.iter().cloned());
            if let Some(tip) = &on_tip_passed {
                notices.push(on_tip_notice(&contract.contract, &command, tip));
            }
            notices.extend(check_notices(&command, &left, attempt.excluded()));
            let Some(commit) = commit else {
                // The general-path stub refusal (decision 0086, generalizing
                // 0081's interface-only `stub_left_in_place`): an empty
                // attempt's declared write targets are byte-identical to
                // dispatch by construction (nothing changed at all), so this
                // asks what that unchanged content *is*, not whether it
                // changed. A target still holding nothing but a placeholder
                // body is refused rather than passed.
                //
                // Either way the worktree is left where it is: `release` (a
                // refused one, a losing one) or `integrate` (the selected one)
                // removes it under decision 0088's `Delete` and keeps it under
                // `Keep`, and neither removes one an open contract still
                // continues in (a handoff child's inherited worktree).
                let write_paths: Vec<&str> =
                    contract.writes.iter().map(|w| w.path.as_str()).collect();
                let left_as_stub = placeholder_writes_left_in_place(write_paths.clone(), |p| {
                    read_write_target(ws.root(), p)
                });
                if !left_as_stub.is_empty() {
                    let reason = placeholder_left_in_place_reason(&left_as_stub);
                    return Ok(Verdict::Fail { reason });
                }
                // A contract that declared files to write and changed none of
                // them did not do its work, whatever its check says: a check
                // that already passes before the work (`cargo build` over a
                // stub that compiles) proves nothing. Refused, as an empty
                // remediation is (decision 0018); retried, then held. A handoff
                // child adopting its parent's worktree is the exception: the
                // work it continues may already be all there is.
                //
                // Another exception: an earlier merge of this run delivered
                // the work, so the check passes here and failed where the run
                // started. That is the work's evidence; it is accepted as an
                // empty integration and said, never silently.
                let mut delivered_earlier = None;
                if !write_paths.is_empty() && contract.workspace.is_none() {
                    let refusal = |why: Option<String>| {
                        let mut reason = unchanged_writes_reason(&command, &write_paths);
                        if let Some(why) = why {
                            reason.push_str(&format!(". {why}"));
                        }
                        Ok(Verdict::Fail { reason })
                    };
                    match crate::earlier_delivery::find(
                        manager.repo(),
                        merge_back.integration_branch(),
                        ws.head(),
                        &write_paths,
                    ) {
                        Err(e) => {
                            return refusal(Some(format!(
                                "Whether an earlier merge delivered the work could not be \
                                 read: {e}"
                            )));
                        }
                        Ok(None) => return refusal(None),
                        Ok(Some(earlier)) => {
                            match passes_at(
                                &manager,
                                &earlier.original_base,
                                &denies,
                                &command,
                                timeout,
                            ) {
                                Ok(false) => delivered_earlier = Some(earlier),
                                // The check also passes where the run started:
                                // it proves nothing about the work.
                                Ok(true) => return refusal(None),
                                Err(e) => {
                                    return refusal(Some(format!(
                                        "The check could not be run at the run's original base \
                                         to tell whether an earlier merge delivered the work: {e}"
                                    )));
                                }
                            }
                        }
                    }
                }
                workspaces
                    .empty
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(root);
                match &delivered_earlier {
                    Some(earlier) => notices.push(delivered_earlier_notice(
                        &contract.contract,
                        &command,
                        &write_paths,
                        earlier,
                    )),
                    None => notices.push(empty_integration_notice(&contract.contract)),
                }
                return Ok(Verdict::Pass {
                    by: command,
                    commit: None,
                    notices,
                });
            };
            // Kept for merge (the selected one) or discard (the rest).
            workspaces.committed().insert(root, attempt);
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            let by = match &deferred {
                Some(failed) => {
                    notices.push(deferred_notice(
                        &contract.contract,
                        &command,
                        &waits,
                        failed,
                    ));
                    format!("{command} (deferred to the root gate)")
                }
                None => {
                    // Decision 0100, the zero-test rule (cargo only): a
                    // narrowed check that selects no recognized test target,
                    // with no dependent check beside it, compiled the work and
                    // tested nothing. The verdict says so, and the
                    // orchestrator leaves the contract unverified under that
                    // reason code until the root gate passes over it. Applied
                    // to a small request's contract only for now: a planned
                    // contract's verdict is as it was.
                    let writes: Vec<&str> =
                        contract.writes.iter().map(|w| w.path.as_str()).collect();
                    let vacuous = (dependents.passed.is_empty()
                        && contract.gate_commands.is_empty()
                        && own_checked.is_empty()
                        && (zero_test_plans
                            || kgraf_orchestrator::triage::is_small_contract(&contract)))
                    .then(|| {
                        crate::scoped_check::cargo_selects_no_tests(&whole, ws.root(), &writes)
                    })
                    .flatten();
                    let by = by_with_dependents(command, &dependents);
                    let by = if own_checked.is_empty() {
                        by
                    } else {
                        format!(
                            "{by} [{}: {}]",
                            kgraf_orchestrator::CONTRACT_CHECK_PASSED,
                            own_checked.join(" && ")
                        )
                    };
                    match vacuous {
                        Some(packages) => format!(
                            "{by} [{}: {} has no recognized test target]",
                            kgraf_orchestrator::SCOPED_CHECK_VACUOUS,
                            packages.join(", ")
                        ),
                        None => by,
                    }
                }
            };
            Ok(Verdict::Pass {
                by: by.clone(),
                commit: Some(VerifiedCommit {
                    sha: commit,
                    verified_by: by,
                    verified_ms: now,
                }),
                notices,
            })
        })
        .await
        .map_err(|e| e.to_string())?
    }

    /// The claim is checked against the dispatching repository (never a
    /// worker's tree): the commit must exist. The command is not re-run on
    /// resume.
    async fn recheck(&self, commit: &VerifiedCommit) -> Result<bool, String> {
        let repo = self.repo.clone();
        let spec = format!("{}^{{commit}}", commit.sha);
        tokio::task::spawn_blocking(move || {
            let git = kgraf_sandbox::git::Git::discover().map_err(|e| e.to_string())?;
            Ok(git.run(&repo, ["cat-file", "-e", spec.as_str()]).is_ok())
        })
        .await
        .map_err(|e| e.to_string())?
    }

    /// The narrowed check, named in the brief so the worker runs what it is
    /// judged by and knows sibling stubs failing the whole suite are not its
    /// work. Read from the worktree as dispatched; the verifier reads it again
    /// after the work, so a test file the worker adds is included then.
    fn acceptance_brief(&self, contract: &OpenContract, workspace: &Path) -> Option<String> {
        // A gate remediation is judged on the merged result: sibling failures
        // there are its work, so the "not yours to fix" text does not apply.
        if !contract.gate_commands.is_empty() {
            return None;
        }
        let scope = check_scope(&self.command, contract, workspace);
        let text = crate::scoped_check::brief(&scope, &self.command)?;
        // Decision 0018, amended 2026-10-02: the worker is told when the whole
        // check may be deferred, so it does not chase files of other
        // contracts that the check needs.
        if self.root_gate && matches!(scope, crate::scoped_check::Scope::Whole { .. }) {
            return Some(format!("{text} {DEFERRED_BRIEF}"));
        }
        Some(text)
    }

    /// The check [`Verifier::verify_among`] runs on the tree as it is now, by
    /// the same rule, and what it does with a failure that comes from the
    /// open contracts' missing work. The narrowing is read again when the
    /// attempt is verified, so the answer says so.
    fn judge(
        &self,
        contract: &OpenContract,
        workspace: &Path,
        unmerged: &[OpenContract],
    ) -> Option<String> {
        let whole = &self.command;
        if !contract.gate_commands.is_empty() {
            return Some(format!(
                "Your attempt is judged by `{}` and the failing gate's own commands, run in \
                 your worktree when you finish.",
                clean(whole)
            ));
        }
        let scope = check_scope(whole, contract, workspace);
        let command = clean(&scope.command(whole));
        let mut text = match &scope {
            crate::scoped_check::Scope::Whole { .. } => format!(
                "Your attempt is judged by `{command}`, run in your worktree when you finish."
            ),
            crate::scoped_check::Scope::Narrowed { selected, .. } => format!(
                "Your attempt is judged by `{command}` (the tests of your files: {}), run in \
                 your worktree when you finish; the tests are chosen again then, from your \
                 files as you leave them.",
                clean(&selected.join(", "))
            ),
        };
        let waits = Waits::of(whole, &scope, workspace, unmerged);
        if waits.is_empty() {
            return Some(text);
        }
        let needs = clean(&waits.describe());
        if self.root_gate {
            text.push_str(&format!(
                " It needs the work of {needs}, not merged yet: if it fails on your attempt and \
                 also on the tree you were given, it is deferred to the root gate, which runs \
                 `{}` on the merged result. A failure your change causes where the check passed \
                 before is still a failure.",
                clean(whole)
            ));
        } else {
            text.push_str(&format!(
                " It needs the work of {needs}, not merged yet, and the run has no root gate to \
                 defer it to: if it fails for that missing work, this attempt fails, and the \
                 plan must order that work first."
            ));
        }
        Some(text)
    }

    /// The narrowed check a write set gets, read from the repository at plan
    /// intake (`scoped_check::scope`, the rule attempts are checked by).
    fn plan_check_scope(&self, writes: &[&str], root: Option<&Path>) -> Option<Vec<String>> {
        match crate::scoped_check::scope(&self.command, root?, writes) {
            crate::scoped_check::Scope::Narrowed { selected, .. } => Some(selected),
            crate::scoped_check::Scope::Whole { .. } => None,
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;

    /// Runs `read_write_target` on a thread: a read that blocks on a named
    /// pipe must fail this test, not hang the suite.
    fn read_bounded(root: &Path, rel: &str) -> Option<String> {
        let (tx, rx) = std::sync::mpsc::channel();
        let (root, rel) = (root.to_path_buf(), rel.to_owned());
        std::thread::spawn(move || {
            let _ = tx.send(read_write_target(&root, &rel));
        });
        rx.recv_timeout(Duration::from_secs(5))
            .expect("the read returned instead of blocking")
    }

    #[test]
    fn a_regular_file_is_read() {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
        std::fs::write(dir.path().join("src/a.rs"), "fn a() {}").expect("write");
        assert_eq!(
            read_bounded(dir.path(), "src/a.rs").as_deref(),
            Some("fn a() {}")
        );
    }

    /// A file over the cap is not read (it is real code, never a placeholder).
    #[test]
    fn a_file_over_the_read_cap_is_not_read() {
        let dir = tempfile::tempdir().expect("dir");
        let big = std::fs::File::create(dir.path().join("big.rs")).expect("create");
        big.set_len(MAX_PLACEHOLDER_READ + 1).expect("len");
        assert_eq!(read_bounded(dir.path(), "big.rs"), None);
        let ok = std::fs::File::create(dir.path().join("ok.rs")).expect("create");
        ok.set_len(MAX_PLACEHOLDER_READ).expect("len");
        assert!(read_bounded(dir.path(), "ok.rs").is_some());
    }

    #[test]
    fn a_named_pipe_is_never_opened_for_a_blocking_read() {
        let dir = tempfile::tempdir().expect("dir");
        let fifo = dir.path().join("a.rs");
        let c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).expect("cstr");
        // SAFETY: `c` is a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        assert_eq!(read_bounded(dir.path(), "a.rs"), None);
    }

    #[test]
    fn a_link_at_the_leaf_or_on_the_way_is_never_followed() {
        let dir = tempfile::tempdir().expect("dir");
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("secret.rs"), "fn f() { todo!() }").expect("write");
        std::os::unix::fs::symlink(outside.path().join("secret.rs"), dir.path().join("leaf.rs"))
            .expect("link");
        std::os::unix::fs::symlink(outside.path(), dir.path().join("dir")).expect("link");
        assert_eq!(read_bounded(dir.path(), "leaf.rs"), None);
        assert_eq!(read_bounded(dir.path(), "dir/secret.rs"), None);
    }

    #[test]
    fn a_hard_linked_file_is_never_read() {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::write(dir.path().join("a.rs"), "x").expect("write");
        std::fs::hard_link(dir.path().join("a.rs"), dir.path().join("b.rs")).expect("link");
        assert_eq!(read_bounded(dir.path(), "b.rs"), None);
    }

    #[test]
    fn control_bytes_in_a_name_never_reach_a_notice() {
        let evil = "a\u{1b}[2Jb\nc\0.txt";
        assert_eq!(clean(evil), "a?[2Jb?c?.txt");
        let excluded = vec![Excluded {
            path: PathBuf::from("Cargo\u{1b}.lock"),
            kind: GeneratedKind::Lockfile,
        }];
        let notices = worker_notices(&excluded);
        assert!(
            notices.iter().all(|n| !n.chars().any(char::is_control)),
            "{notices:?}"
        );
        let byte = PathBuf::from(evil);
        let text = some_paths(&[byte.as_path()]);
        assert!(!text.chars().any(char::is_control), "{text:?}");
        let left = Leftovers {
            created: vec![
                PathBuf::from("x\u{7}y.bin"),
                PathBuf::from("Cargo\u{1b}.lock"),
            ],
            changed: Vec::new(),
        };
        let all = check_notices("true", &left, &[]);
        assert!(
            all.iter().all(|n| !n.chars().any(char::is_control)),
            "{all:?}"
        );
    }

    #[test]
    fn the_empty_notice_names_its_contract_so_a_set_keeps_each() {
        let a = empty_integration_notice("c-1");
        let b = empty_integration_notice("c-2");
        assert_ne!(a, b);
        assert!(
            a.contains("c-1") && a.contains("integrated no change"),
            "{a}"
        );
        let set: std::collections::BTreeSet<String> = [a, b].into_iter().collect();
        assert_eq!(set.len(), 2);
    }

    fn contract_writing(paths: &[&str]) -> OpenContract {
        let mut c = OpenContract::planned("c1", "t".into(), "add it", "it exists");
        c.writes = paths
            .iter()
            .map(|p| kgraf_orchestrator::WriteTarget {
                path: (*p).into(),
                node: None,
            })
            .collect();
        c
    }

    #[test]
    fn clean_replaces_bidi_overrides_and_line_separators() {
        let text = "a\u{202E}b\u{2028}c\u{2066}d\u{200B}e\u{1b}[31mf\ng";
        assert_eq!(clean(text), "a?b?c?d?e?[31mf?g");
        assert_eq!(clean_lines(text), "a?b?c?d\u{200B}e?[31mf\ng");
        assert_eq!(clean("plain \u{e9} text"), "plain \u{e9} text");
    }

    /// Joiners carry meaning in output: a Persian word with a non-joiner and
    /// an emoji family sequence come back unchanged, while every bidi
    /// override and isolate beside them is still replaced.
    #[test]
    fn clean_lines_keeps_joiners() {
        let persian = "\u{645}\u{6CC}\u{200C}\u{62E}\u{648}\u{627}\u{647}\u{645}";
        let family = "\u{1F469}\u{200D}\u{1F469}\u{200D}\u{1F467}";
        let text = format!(
            "{persian}\n{family}\t\u{202A}\u{202B}\u{202C}\u{202D}\u{2067}\u{2069}\u{2029}"
        );
        assert_eq!(clean_lines(&text), format!("{persian}\n{family}\t???????"));
    }

    fn git_repo_with(file: &str, content: &str) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().expect("dir");
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .current_dir(dir.path())
                .args(["-c", "core.hooksPath=/dev/null"])
                .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
                .args(args)
                .output()
                .expect("git");
            assert!(out.status.success(), "{args:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        };
        run(&["init", "-q"]);
        std::fs::write(dir.path().join(file), content).expect("write");
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "x"]);
        let sha = run(&["rev-parse", "HEAD"]);
        (dir, sha)
    }

    #[test]
    fn base_targets_read_absent_or_fail_loud() {
        let (dir, sha) = git_repo_with("a.rs", "fn f() { todo!() }");
        let read = |paths: &[&str]| base_write_targets(dir.path(), &sha, paths);
        let got = read(&["a.rs", "missing.rs"]).expect("read");
        assert_eq!(got["a.rs"], Some("fn f() { todo!() }".to_owned()));
        // A path the base lacks is absent, not an error.
        assert_eq!(got["missing.rs"], None);
        assert!(read(&[]).expect("none").is_empty());
        // A revision git cannot resolve, and a repository that is not one,
        // are errors: never read as "absent".
        assert!(base_write_targets(dir.path(), &"0".repeat(40), &["a.rs"]).is_err());
        let not_repo = tempfile::tempdir().expect("dir");
        assert!(base_write_targets(not_repo.path(), &sha, &["a.rs"]).is_err());
    }

    /// Many targets, kinds and sizes come back from one listing and one batch
    /// read: files with newlines and spaces in the name and content, a
    /// directory, a symlink, an empty file, a non-UTF-8 file and one over the cap
    /// are each read or skipped by their own kind.
    #[cfg(unix)]
    #[test]
    fn base_targets_are_read_in_one_batch_by_kind() {
        let (dir, _) = git_repo_with("seed.txt", "x");
        let big = "y".repeat(MAX_PLACEHOLDER_READ as usize + 1);
        std::fs::create_dir_all(dir.path().join("d")).expect("dir");
        std::fs::write(dir.path().join("one.rs"), "fn a() { todo!() }\n").expect("one");
        std::fs::write(dir.path().join("two words.rs"), "line1\nline2\n\n").expect("two");
        std::fs::write(dir.path().join("empty.rs"), "").expect("empty");
        std::fs::write(dir.path().join("bin.rs"), [0xff, 0xfe, 0x00]).expect("bin");
        std::fs::write(dir.path().join("big.rs"), &big).expect("big");
        std::fs::write(dir.path().join("d/inner.rs"), "inner").expect("inner");
        std::os::unix::fs::symlink("one.rs", dir.path().join("link.rs")).expect("link");
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .current_dir(dir.path())
                .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
                .args(args)
                .output()
                .expect("git");
            assert!(out.status.success(), "{args:?}");
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        };
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "y"]);
        let sha = run(&["rev-parse", "HEAD"]);
        let paths = [
            "one.rs",
            "two words.rs",
            "empty.rs",
            "bin.rs",
            "big.rs",
            "d",
            "d/inner.rs",
            "link.rs",
            "missing.rs",
        ];
        let got = base_write_targets(dir.path(), &sha, &paths).expect("read");
        assert_eq!(got["one.rs"].as_deref(), Some("fn a() { todo!() }\n"));
        assert_eq!(got["two words.rs"].as_deref(), Some("line1\nline2\n\n"));
        assert_eq!(got["empty.rs"].as_deref(), Some(""));
        assert_eq!(got["d/inner.rs"].as_deref(), Some("inner"));
        for skipped in ["bin.rs", "big.rs", "d", "link.rs", "missing.rs"] {
            assert_eq!(got[skipped], None, "{skipped}");
        }
    }

    /// The check's cargo folder starts from the checkout's own `target/`
    /// when it is a real folder, never from a link or a file.
    #[test]
    fn the_warm_build_folder_is_the_checkouts_own_real_target_folder() {
        let dir = tempfile::tempdir().expect("dir");
        let c = dir.path();
        assert_eq!(warm_build_folder(c), None, "absent");
        std::fs::write(c.join("target"), "x").expect("write");
        assert_eq!(warm_build_folder(c), None, "a file");
        std::fs::remove_file(c.join("target")).expect("rm");
        std::fs::create_dir(c.join("elsewhere")).expect("mkdir");
        std::os::unix::fs::symlink(c.join("elsewhere"), c.join("target")).expect("ln");
        assert_eq!(warm_build_folder(c), None, "a link to a folder");
        std::fs::remove_file(c.join("target")).expect("rm");
        std::fs::create_dir(c.join("target")).expect("mkdir");
        assert_eq!(warm_build_folder(c), Some(c.join("target")));
    }

    /// A gate remediation is always checked whole; any other attempt is
    /// narrowed to the tests of its declared files when it can be.
    #[test]
    fn a_remediation_is_checked_whole_and_another_attempt_is_narrowed() {
        let dir = tempfile::tempdir().expect("dir");
        for (path, text) in [("pkg/a.py", ""), ("tests/test_a.py", "")] {
            let p = dir.path().join(path);
            std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
            std::fs::write(p, text).expect("write");
        }
        let plain = contract_writing(&["pkg/a.py"]);
        let narrowed = check_scope("pytest", &plain, dir.path());
        assert!(
            matches!(narrowed, crate::scoped_check::Scope::Narrowed { .. }),
            "{narrowed:?}"
        );
        let mut remediation = contract_writing(&["pkg/a.py"]);
        remediation.gate_commands = vec!["cargo test".to_owned()];
        let whole = check_scope("pytest", &remediation, dir.path());
        assert!(
            matches!(whole, crate::scoped_check::Scope::Whole { .. }),
            "{whole:?}"
        );
    }

    /// Start `command` in `ws` on another thread and wait until it has begun
    /// (its first line writes the marker), so the worktree reads busy.
    fn hold_busy<'a>(
        scope: &'a std::thread::Scope<'a, '_>,
        manager: &'a kgraf_sandbox::WorkspaceManager,
        ws: &'a kgraf_sandbox::Workspace,
        command: &'a str,
    ) -> std::thread::ScopedJoinHandle<'a, ()> {
        let handle = scope.spawn(move || {
            let _ = run_confined(
                ws,
                manager.repo(),
                &[],
                command,
                Duration::from_secs(30),
                1_000,
                None,
            );
        });
        let marker = ws.root().join("started.txt");
        let waited = std::time::Instant::now();
        while !marker.exists() && waited.elapsed() < Duration::from_secs(30) {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(marker.exists(), "the command started");
        handle
    }

    /// The hold check waits for a command that is stopping and gives up on
    /// one that keeps running: the wait is bounded, and it is not skipped.
    #[test]
    fn the_hold_check_waits_a_moment_and_then_gives_up() {
        let (dir, sha) = git_repo_with("a.txt", "x");
        let base = tempfile::tempdir().expect("base");
        let manager =
            kgraf_sandbox::WorkspaceManager::open(dir.path(), base.path()).expect("manager");
        let ws = manager
            .create(&kgraf_sandbox::WorkspaceSpec {
                name: "quiesce-probe",
                base: &sha,
                branch: None,
            })
            .expect("worktree");
        std::thread::scope(|s| {
            // A command that ends within the wait: the check retries and holds.
            let h = hold_busy(s, &manager, &ws, "echo go > started.txt; sleep 0.4");
            let guard = quiesce_retrying(&manager, &ws);
            assert!(guard.is_ok(), "waited for the command to end");
            drop(guard);
            h.join().expect("join");
            std::fs::remove_file(ws.root().join("started.txt")).expect("rm");
            // A command that outlasts the wait: busy is returned, after waiting.
            let h = hold_busy(s, &manager, &ws, "echo go > started.txt; sleep 6");
            let began = std::time::Instant::now();
            let busy = quiesce_retrying(&manager, &ws);
            let waited = began.elapsed();
            assert!(
                matches!(busy, Err(kgraf_sandbox::WorkspaceError::Busy { .. })),
                "busy after the last try"
            );
            assert!(waited >= Duration::from_millis(700), "retried: {waited:?}");
            drop(busy);
            h.join().expect("join");
        });
        manager.remove(ws).expect("remove");
    }

    #[test]
    fn a_committed_attempt_that_skipped_a_dispatch_placeholder_is_named() {
        let c = contract_writing(&["src/a.rs", "src/b.rs", "src/c.rs"]);
        let base = |p: &str| match p {
            "src/a.rs" | "src/b.rs" => Some("fn f() { todo!() }".to_owned()),
            "src/c.rs" => Some("fn f() -> u8 { 1 }".to_owned()),
            _ => None,
        };
        // a.rs was changed; b.rs (placeholder) and c.rs (real) were not.
        let left = untouched_placeholders(&c, &["src/a.rs".to_owned()], base);
        assert_eq!(left, vec!["src/b.rs"]);
        // Every placeholder target changed: nothing to refuse.
        let changed = ["src/a.rs".to_owned(), "src/b.rs".to_owned()];
        let none = untouched_placeholders(&c, &changed, base);
        assert!(none.is_empty(), "{none:?}");
    }
}

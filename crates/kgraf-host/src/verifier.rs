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
/// 4. the acceptance command, narrowed to the tests of the contract's own
///    files when its runner allows ([`crate::scoped_check`]; a gate
///    remediation keeps the whole command), runs **inside the sandbox** of that worktree
///    (writes confined to the worktree and its scratch directory, network off,
///    kGraf's state unreadable, the same policy as the worker's shell),
///    bounded by the gate timeout; when its shell exits, its process group is
///    killed;
/// 5. the worktree must then be quiet again (a process that left the group is
///    found by the hold check and fails the verification);
/// 6. what the command left is removed, descriptor-relative and never through
///    a symlink (`MergeBack::discard_leftovers`). New files are reported as
///    notices and never merged. A committed file the command changed fails the
///    verification: the merged tree would differ from the tree it checked.
///
/// The commit comes before the check. With the check first, the commit swept
/// up whatever the check wrote, so the scope check refused the worker for a
/// file its check generated ("changes outside the declared write scope:
/// [\"Cargo.lock\"]"). Running the check on a throwaway worktree of the commit
/// was the other option, rejected: it loses the worker's warm build directory
/// and the ignored environment directories (`node_modules`, `.venv`) a check
/// may need, so a check that passes in the worker's tree could fail there for
/// reasons that are not the work; the quality gate already gives the
/// clean-checkout judgment on the merged tree.
///
/// **Known limit: worker-controlled caches are check inputs.** The check sees
/// the worker's scratch directory (`CARGO_TARGET_DIR` at `<scratch>/target`,
/// Python bytecode at `<scratch>/pycache`) and ignored files in the tree, all of
/// which the worker's run could have written. A worker can therefore make its
/// own check pass with build output that does not match the committed
/// sources. This verdict selects among attempts; only the quality gate, which
/// runs the project's commands on a clean checkout of the merged commit,
/// judges the merged tree.
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

/// Output a failed check's reason keeps, from the end.
const CHECK_OUTPUT_CAP: usize = 1_000;

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
    );
    let removed = manager.remove(ws);
    let (passed, _) = ran?;
    removed.map_err(|e| format!("cannot remove the worktree at {base}: {e}"))?;
    Ok(passed)
}

/// What a worker checked by the whole command is told about deferral.
pub(crate) const DEFERRED_BRIEF: &str = "If that command can only pass once other contracts \
     of the plan have merged (it also fails on the tree you were given), it is checked on the \
     merged result instead; do your own files' part and do not write files of other contracts \
     to make it pass. A change of yours that makes it fail where it passed before is still a \
     failure.";

/// Characters of a deferred check's failing output quoted in its notice.
const DEFERRED_OUTPUT_CHARS: usize = 300;

/// The notice of an attempt whose whole check was deferred to the root gate
/// (decision 0018, amended 2026-10-02). Named per contract: the run's notices
/// are a set.
fn deferred_notice(contract: &str, command: &str, unmerged: &[String], failed: &str) -> String {
    let failed: String = failed.trim().chars().take(DEFERRED_OUTPUT_CHARS).collect();
    format!(
        "contract {}: `{}` is deferred to the root gate: it failed on this attempt and also at \
         its dispatch base while contracts {} of the plan were not merged, so the failure \
         cannot be told apart from their missing work. The attempt passed its scope and \
         placeholder checks and merges; the whole command runs on the merged result before the \
         run can complete. It said: {}",
        clean(contract),
        clean(command),
        clean(&unmerged.join(", ")),
        clean(&failed)
    )
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
    for e in excluded {
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
        unmerged: &[String],
    ) -> Result<Verdict, String> {
        // Only with the whole command also a root gate: a deferred check
        // must still run before the run can complete.
        let unmerged: Vec<String> = if self.root_gate {
            unmerged.to_vec()
        } else {
            Vec::new()
        };
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
                )?;
                if !ok {
                    (passed, output, failing) = (false, out, gate_command.clone());
                    break;
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
            let mut deferred: Option<String> = None;
            let defers = !passed
                && failing == command
                && commit.is_some()
                && contract.gate_commands.is_empty()
                && matches!(scope, crate::scoped_check::Scope::Whole { .. })
                && !unmerged.is_empty();
            if defers {
                match passes_at(&manager, ws.head(), &denies, &command, timeout) {
                    Ok(false) => {
                        deferred = Some(clean_lines(&output));
                        passed = true;
                    }
                    Ok(true) => output.push_str(
                        "\n(it passes at the dispatch base, so this attempt's change makes it \
                         fail)",
                    ),
                    Err(e) => output.push_str(&format!(
                        "\n(it could not be run at the dispatch base to tell whether the \
                         contracts not merged yet ({}) cause the failure: {e})",
                        unmerged.join(", ")
                    )),
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
            let mut notices = worker_notices(attempt.excluded());
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
                        &unmerged,
                        failed,
                    ));
                    format!("{command} (deferred to the root gate)")
                }
                None => command,
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

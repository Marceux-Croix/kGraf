//! Runs contract-check commands in the sandbox (0030, 0031).
//!
//! Each run gets its **own** directory under the scratch root, and that directory
//! is the sandbox's only writable root:
//!
//! ```text
//!   <scratch>/check-<n>/tree     the dispatch base plus the artifact's files; cwd
//!   <scratch>/check-<n>/tmp      TMPDIR
//!   <scratch>/check-<n>/target   CARGO_TARGET_DIR, one per run
//!   <scratch>/check-<n>.out|.err output files, outside every writable root
//!   <scratch>/check-<n>.hold     the hold file (and its `+sessions` log),
//!                                outside every writable root
//! ```
//!
//! so one check cannot poison another's build directory or read-modify its tree.
//! Trade-off: a target directory per run pays every dependency build again, every
//! stage; the alternative (one shared target) let a check plant artifacts another
//! check linked against.
//!
//! **The base is the dispatch base commit** ([`CheckBase::Commit`]): `git archive`
//! of that commit, taken once per runner and extracted per run, so a check sees
//! what a worker's worktree starts from, never the working directory's uncommitted
//! or ignored files. A project without git uses [`CheckBase::Directory`], copied
//! whole except its top-level `.git` and `.kgraf`.
//!
//! **Bounded.** One timeout covers the whole run. On Unix the child leads its own
//! process group (the sandbox calls `setsid`), and the group is killed when the
//! child exits **and** on timeout, so a backgrounded descendant cannot outlive the
//! check. The gates' lifecycle is shared (`kgraf_sandbox::lifecycle`): the
//! group is killed while the leader is still unreaped (`WNOWAIT`), until the
//! run's hold file has no holder, and only then is the leader reaped, so a
//! group id is never signaled after it could be reused. The hold file is
//! outside the check's writable root, so a check cannot delete and recreate it
//! to hide a holder. Output goes to files the parent opened, never pipes, so a descendant
//! holding stdout cannot stall the reader; only the last [`DEFAULT_TAIL_BYTES`] are
//! read back. Before exec, `RLIMIT_FSIZE` bounds every file (the output files
//! included) and `RLIMIT_CPU` bounds CPU per process.
//!
//! Not bounded, stated rather than hidden: the process **count**. `RLIMIT_NPROC`
//! counts every process of the user on macOS and Linux, not the check's, so any
//! value low enough to stop a fork bomb also breaks the user's other programs.
//! The group kill and the timeout end a fork storm that stays in the group; a
//! descendant that calls `setsid` itself leaves the group and is not killed,
//! but it still holds the hold file, so the check ends as one that could not
//! run rather than as a quiet tree.
//!
//! **What an escaped descendant can still do**, stated rather than hidden
//!: it is not killed. It runs on, inside the same sandbox, until it exits
//! by itself or `RLIMIT_CPU` stops it, with write access to the run's
//! directory only. Cleanup removes that directory when the run ends, and the
//! descendant can race it: files it writes after the removal are left in the
//! scratch directory, which the host removes when the run ends and sweeps at
//! the next startup if the process died.
//!
//! **kGraf's state is always unreadable**: the constructor takes the state
//! paths and also denies `<repository>/.kgraf`; there is no runner without
//! them.

use crate::checks::{CheckError, FileOverlay};
use kgraf_sandbox::{CheckNetwork, SandboxPolicy, sandboxed_command};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Bytes of output kept per stream.
pub const DEFAULT_TAIL_BYTES: usize = 4_096;

/// Bytes of each stream scanned for compiler errors before it is cut to
/// [`DEFAULT_TAIL_BYTES`]. `RLIMIT_FSIZE` is the only bound on the output
/// files, so the scan reads at most this much from each file's end.
#[cfg(unix)]
const SCAN_BYTES: usize = 8 * 1024 * 1024;

/// Most contract-check runs at once in this process: half the cores, at least
/// one, at most four. A starting point, not a measured number.
pub fn max_concurrent_checks() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get() / 2)
        .unwrap_or(1)
        .clamp(1, 4)
}

fn check_slots() -> &'static tokio::sync::Semaphore {
    static SLOTS: std::sync::OnceLock<tokio::sync::Semaphore> = std::sync::OnceLock::new();
    SLOTS.get_or_init(|| tokio::sync::Semaphore::new(max_concurrent_checks()))
}

/// What a check tree is made from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckBase {
    /// A commit of a git repository.
    Commit { repo: PathBuf, rev: String },
    /// A directory without git.
    Directory(PathBuf),
}

/// Where a challenge's artifacts are read ([`SandboxRunner::artifacts`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Artifacts {
    Commit(crate::challenge::CommitArtifacts),
    Directory(PathBuf),
}

/// Per-process limits applied before exec (Unix).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResourceLimits {
    /// Largest file any process of the check may write.
    pub file_size_bytes: u64,
    /// CPU seconds per process.
    pub cpu_seconds: u64,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            file_size_bytes: 2 * 1024 * 1024 * 1024,
            cpu_seconds: 900,
        }
    }
}

/// One command's result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunResult {
    pub success: bool,
    pub timed_out: bool,
    /// Exit status as text.
    pub status: String,
    /// The last bytes of stderr, then of stdout.
    pub tail: String,
    /// How long the run waited for a check slot before it started (the
    /// process-wide bound on concurrent checks); its own bound starts after.
    pub queued: Duration,
}

impl RunResult {
    pub(crate) fn describe(&self, argv: &[String]) -> String {
        if self.timed_out {
            format!("`{}` timed out", argv.join(" "))
        } else {
            format!("`{}` {}: {}", argv.join(" "), self.status, self.tail)
        }
    }
}

#[derive(Debug)]
enum Resolved {
    Commit {
        repo: PathBuf,
        /// The commit the runner was made from: the run's base, before any
        /// of the plan landed. The project's own files are read from it
        /// ([`SandboxRunner::base_differs`]).
        origin: String,
        /// The commit and its archive (made once per commit). The commit moves
        /// when continuous merge-back advances the integration branch
        /// ([`SandboxRunner::advance_base`]).
        current: Mutex<CommitBase>,
    },
    Directory(PathBuf),
}

#[derive(Debug)]
struct CommitBase {
    sha: String,
    archive: Option<Arc<Tarball>>,
}

/// An archive of one base commit. Removed when the last check extracting it
/// is done, so advancing the base never pulls a file from under a running
/// check, and superseded archives do not pile up in the scratch directory.
#[derive(Debug)]
struct Tarball(PathBuf);

impl Drop for Tarball {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Runs check commands in the sandbox over fresh trees of the base.
#[derive(Debug)]
pub struct SandboxRunner {
    base: Resolved,
    scratch: PathBuf,
    read_denies: Vec<PathBuf>,
    accept_partial: bool,
    /// The network checks run with: denied, except where the backend cannot
    /// deny it (Windows), where it is open and every result says so
    /// ([`CheckNetwork`], decision 0011 amended 2026-10-06).
    network: CheckNetwork,
    timeout: Duration,
    /// The bound on a compile (stub-compile and canary) run, separate from the
    /// test runs' bound: a cold compile on a loaded machine is slow for reasons
    /// that say nothing about the contract.
    compile_timeout: Duration,
    limits: ResourceLimits,
    seq: AtomicU64,
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> CheckError + '_ {
    move |source| CheckError::Io {
        path: path.to_path_buf(),
        source,
    }
}

impl SandboxRunner {
    /// A runner over `base`, making trees under `scratch` (absolute, outside the
    /// base, created), with reads of every path in `state_denies` refused inside
    /// the sandbox. Each deny must exist: the sandbox resolves it.
    pub fn new(
        base: CheckBase,
        scratch: PathBuf,
        state_denies: Vec<PathBuf>,
    ) -> Result<Self, CheckError> {
        let root = match &base {
            CheckBase::Commit { repo, .. } | CheckBase::Directory(repo) => repo.clone(),
        };
        if !root.is_absolute() || !scratch.is_absolute() {
            return Err(CheckError::Config(format!(
                "base {} and scratch {} must be absolute",
                root.display(),
                scratch.display()
            )));
        }
        std::fs::create_dir_all(&scratch).map_err(io(&scratch))?;
        let root = std::fs::canonicalize(&root).map_err(io(&root))?;
        let scratch = std::fs::canonicalize(&scratch).map_err(io(&scratch))?;
        if scratch.starts_with(&root) {
            return Err(CheckError::Config(format!(
                "scratch {} is inside the base {}",
                scratch.display(),
                root.display()
            )));
        }
        let mut read_denies = Vec::new();
        for d in state_denies.into_iter().chain(
            [
                kgraf_core::legacy::STATE_DIR,
                // Former name (decision 0062): denied too while it exists.
                kgraf_core::legacy::LEGACY_STATE_DIR,
            ]
            .into_iter()
            .map(|d| root.join(d))
            .filter(|p| p.exists()),
        ) {
            read_denies.push(std::fs::canonicalize(&d).map_err(|source| CheckError::Io {
                path: d.clone(),
                source,
            })?);
        }
        let base = match base {
            CheckBase::Directory(_) => Resolved::Directory(root),
            CheckBase::Commit { rev, .. } => {
                let git = kgraf_sandbox::git::Git::discover()
                    .map_err(|e| CheckError::Config(e.to_string()))?;
                let sha = git
                    .run_str(
                        &root,
                        ["rev-parse", "--verify", &format!("{rev}^{{commit}}")],
                    )
                    .map_err(|e| {
                        CheckError::Config(format!("`{rev}` is not a commit of the base: {e}"))
                    })?;
                Resolved::Commit {
                    repo: root,
                    origin: sha.clone(),
                    current: Mutex::new(CommitBase { sha, archive: None }),
                }
            }
        };
        Ok(Self {
            base,
            scratch,
            read_denies,
            accept_partial: false,
            network: CheckNetwork::current(),
            timeout: Duration::from_secs(600),
            compile_timeout: Duration::from_secs(600),
            limits: ResourceLimits::default(),
            seq: AtomicU64::new(0),
        })
    }

    /// Run on a backend that reports isolation gaps (Linux, Windows). Refused
    /// without this, as the sandbox refuses.
    pub fn accept_partial_isolation(mut self, accept: bool) -> Self {
        self.accept_partial = accept;
        self
    }

    /// Decide the network checks run with. The default is the platform's
    /// ([`CheckNetwork::current`]); a test passes
    /// [`CheckNetwork::open_with_notice`] to stand in for a backend that
    /// cannot deny it.
    pub fn with_check_network(mut self, network: CheckNetwork) -> Self {
        self.network = network;
        self
    }

    /// The bound on test runs.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The bound on compile runs ([`SandboxRunner::compile`]).
    pub fn with_compile_timeout(mut self, timeout: Duration) -> Self {
        self.compile_timeout = timeout;
        self
    }

    pub fn with_limits(mut self, limits: ResourceLimits) -> Self {
        self.limits = limits;
        self
    }

    /// The bound on one test run.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// The bound on one compile run.
    pub fn compile_timeout(&self) -> Duration {
        self.compile_timeout
    }

    /// Where a challenge reads recorded artifacts: the base commit checks are
    /// made from now, or the base directory.
    pub fn artifacts(&self) -> Artifacts {
        match &self.base {
            Resolved::Commit { repo, current, .. } => {
                Artifacts::Commit(crate::challenge::CommitArtifacts {
                    repo: repo.clone(),
                    sha: current
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .sha
                        .clone(),
                })
            }
            Resolved::Directory(dir) => Artifacts::Directory(dir.clone()),
        }
    }

    /// The commit checks are made from, for a git base.
    pub fn base_commit(&self) -> Option<String> {
        match &self.base {
            Resolved::Commit { current, .. } => Some(
                current
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .sha
                    .clone(),
            ),
            Resolved::Directory(_) => None,
        }
    }

    /// Move a git base to `rev`: the orchestrator calls this when continuous
    /// merge-back advances the run's integration branch, so a check sees what
    /// a fresh worktree now starts from. Checks already running keep the tree
    /// they made. Returns whether the base moved; a directory base never does.
    pub fn advance_base(&self, rev: &str) -> Result<bool, CheckError> {
        let Resolved::Commit { repo, current, .. } = &self.base else {
            return Ok(false);
        };
        let git =
            kgraf_sandbox::git::Git::discover().map_err(|e| CheckError::Config(e.to_string()))?;
        let sha = git
            .run_str(
                repo,
                ["rev-parse", "--verify", &format!("{rev}^{{commit}}")],
            )
            .map_err(|e| CheckError::Config(format!("`{rev}` is not a commit of the base: {e}")))?;
        let mut cur = current.lock().unwrap_or_else(|p| p.into_inner());
        if cur.sha == sha {
            return Ok(false);
        }
        *cur = CommitBase { sha, archive: None };
        Ok(true)
    }

    /// Whether reads of `path` are denied inside the sandbox.
    pub fn denies(&self, path: &Path) -> bool {
        let Ok(p) = std::fs::canonicalize(path) else {
            return false;
        };
        self.read_denies.iter().any(|d| p.starts_with(d))
    }

    /// Whether the base holds something at `path` other than a file with
    /// exactly `content`: a file with other bytes, a link, a directory, or a
    /// file over the read bound. An absent path is `false`. Read from the
    /// commit the runner was made from, never an advanced base: a file a party
    /// of the plan wrote since is the plan's, not the project's. Read through
    /// the hardened blob reader (no filter, no textconv), or from the base
    /// directory.
    pub async fn base_differs(&self, path: &str, content: &str) -> Result<bool, CheckError> {
        let rel = path.trim().trim_start_matches("./").to_owned();
        let want = content.as_bytes().to_vec();
        match &self.base {
            Resolved::Commit { repo, origin, .. } => {
                let repo = repo.clone();
                let sha = origin.clone();
                blocking(move || {
                    use kgraf_sandbox::merge::blob::{BlobError, read_blob};
                    let max = crate::challenge::MAX_ARTIFACT_BYTES;
                    match read_blob(&repo, &sha, &rel, max) {
                        Ok(bytes) => Ok(bytes != want),
                        Err(BlobError::Absent { .. }) => Ok(false),
                        Err(
                            BlobError::Symlink { .. }
                            | BlobError::NotAFile { .. }
                            | BlobError::TooLarge { .. },
                        ) => Ok(true),
                        Err(e) => Err(CheckError::Config(format!(
                            "cannot read `{rel}` at the base commit: {e}"
                        ))),
                    }
                })
                .await
            }
            Resolved::Directory(root) => {
                let at = root.join(&rel);
                blocking(move || match std::fs::symlink_metadata(&at) {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
                    Err(source) => Err(CheckError::Io { path: at, source }),
                    Ok(m) if !m.is_file() => Ok(true),
                    Ok(_) => std::fs::read(&at)
                        .map(|bytes| bytes != want)
                        .map_err(|source| CheckError::Io { path: at, source }),
                })
                .await
            }
        }
    }

    /// Whether the base holds a file at `path` (any content, any size).
    pub async fn base_has(&self, path: &str) -> bool {
        // `base_differs` against empty content is true for any non-empty
        // file, link or directory; an empty file is read as absent, which
        // only makes a probe answer no.
        self.base_differs(path, "").await.unwrap_or(false)
    }

    /// The names of the entries of directory `dir` (repository-relative,
    /// `""` the root) in the base, as [`SandboxRunner::read_base`] reads.
    /// Empty when there is no such directory or it cannot be read.
    pub async fn list_base(&self, dir: &str) -> Vec<String> {
        let rel = dir.trim().trim_start_matches("./").trim_end_matches('/');
        match &self.base {
            Resolved::Commit { repo, origin, .. } => {
                let repo = repo.clone();
                let spec = if rel.is_empty() {
                    origin.clone()
                } else {
                    format!("{origin}:{rel}")
                };
                blocking(move || {
                    let git = kgraf_sandbox::git::Git::discover()
                        .map_err(|e| CheckError::Config(e.to_string()))?;
                    git.run_str(&repo, ["ls-tree", "--name-only", &spec])
                        .map_err(|e| CheckError::Config(e.to_string()))
                })
                .await
                .map(|out| out.lines().map(str::to_owned).collect())
                .unwrap_or_default()
            }
            Resolved::Directory(root) => std::fs::read_dir(root.join(rel))
                .map(|it| {
                    it.filter_map(Result::ok)
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    /// Whether `program` (a bare name) is on the `PATH` a check runs with.
    pub fn has_program(&self, program: &str) -> bool {
        program_installed(program, Path::new("")).is_ok()
    }

    /// The text of the file at `path` in the base: `None` when the base holds
    /// no file there (absent, a link, a directory, over the read bound, or not
    /// UTF-8). Read from the commit the runner was made from, as
    /// [`SandboxRunner::base_differs`] reads, through the same hardened blob
    /// reader, or from the base directory. An interface may name a file the
    /// repository already has by path alone; this is where its content comes
    /// from.
    pub async fn read_base(&self, path: &str) -> Result<Option<String>, CheckError> {
        let rel = path.trim().trim_start_matches("./").to_owned();
        let max = crate::challenge::MAX_ARTIFACT_BYTES;
        match &self.base {
            Resolved::Commit { repo, origin, .. } => {
                let repo = repo.clone();
                let sha = origin.clone();
                blocking(move || {
                    use kgraf_sandbox::merge::blob::{BlobError, read_blob};
                    match read_blob(&repo, &sha, &rel, max) {
                        Ok(bytes) => Ok(String::from_utf8(bytes).ok()),
                        Err(
                            BlobError::Absent { .. }
                            | BlobError::Symlink { .. }
                            | BlobError::NotAFile { .. }
                            | BlobError::TooLarge { .. },
                        ) => Ok(None),
                        Err(e) => Err(CheckError::Config(format!(
                            "cannot read `{rel}` at the base commit: {e}"
                        ))),
                    }
                })
                .await
            }
            Resolved::Directory(root) => {
                let at = root.join(&rel);
                blocking(move || match std::fs::symlink_metadata(&at) {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Err(source) => Err(CheckError::Io { path: at, source }),
                    Ok(m) if !m.is_file() || m.len() > max => Ok(None),
                    Ok(_) => std::fs::read(&at)
                        .map(|bytes| String::from_utf8(bytes).ok())
                        .map_err(|source| CheckError::Io { path: at, source }),
                })
                .await
            }
        }
    }

    /// Make a fresh tree of the base, overlay `layers` in order, and run `argv`
    /// in the sandbox. Everything the run wrote is removed afterwards, also when
    /// the future is dropped.
    pub async fn run(
        &self,
        layers: &[&[FileOverlay]],
        argv: &[String],
    ) -> Result<RunResult, CheckError> {
        self.run_bounded(layers, argv, self.timeout).await
    }

    /// [`SandboxRunner::run`] for a compile, under the compile bound.
    pub async fn compile(
        &self,
        layers: &[&[FileOverlay]],
        argv: &[String],
    ) -> Result<RunResult, CheckError> {
        self.run_bounded(layers, argv, self.compile_timeout).await
    }

    async fn run_bounded(
        &self,
        layers: &[&[FileOverlay]],
        argv: &[String],
        timeout: Duration,
    ) -> Result<RunResult, CheckError> {
        // One process-wide bound on check runs at once, across every runner and
        // run: each run builds its own tree and, for Rust, its
        // own target, so many interfaces checked at once would starve the
        // workers and the machine. Waiting here is queueing, not a timeout: the
        // run's own bound starts after the permit.
        let asked = std::time::Instant::now();
        let _permit = check_slots()
            .acquire()
            .await
            .map_err(|e| CheckError::Config(format!("the check slots are closed: {e}")))?;
        let queued = asked.elapsed();
        let n = self.seq.fetch_add(1, Ordering::SeqCst);
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let name = format!("check-{}-{nonce}-{n}", std::process::id());
        let run_dir = self.scratch.join(&name);
        let hold = hold_of(&run_dir);
        let _cleanup = Cleanup(vec![
            run_dir.clone(),
            self.scratch.join(format!("{name}.out")),
            self.scratch.join(format!("{name}.err")),
            hold.clone(),
            sessions_of(&hold),
        ]);
        for d in ["tree", "tmp", "target"] {
            let p = run_dir.join(d);
            std::fs::create_dir_all(&p).map_err(io(&p))?;
        }
        std::fs::File::create(&hold).map_err(io(&hold))?;
        let tree = run_dir.join("tree");
        let layers: Vec<FileOverlay> = layers.iter().flat_map(|l| l.iter().cloned()).collect();
        self.materialize(&tree, layers).await?;
        if let Some(program) = argv.first() {
            program_installed(program, &tree)?;
        }
        let mut result = self.exec(&name, &run_dir, argv, timeout).await?;
        result.queued = queued;
        // Said in every result, a pass included: a check that passed with the
        // network open proves less than one that passed with it denied.
        if let Some(notice) = self.network.notice() {
            if !result.tail.is_empty() {
                result.tail.push('\n');
            }
            result.tail.push_str("[sandbox] ");
            result.tail.push_str(notice);
        }
        Ok(result)
    }

    async fn materialize(&self, tree: &Path, layers: Vec<FileOverlay>) -> Result<(), CheckError> {
        match &self.base {
            Resolved::Directory(root) => {
                let (root, tree) = (root.clone(), tree.to_path_buf());
                blocking(move || copy_tree(&root, &tree, true)).await?;
            }
            Resolved::Commit { repo, current, .. } => {
                let tarball = {
                    let mut cur = current.lock().unwrap_or_else(|p| p.into_inner());
                    match cur.archive.as_ref() {
                        Some(t) => t.clone(),
                        None => {
                            let sha = cur.sha.clone();
                            let t = self.scratch.join(format!("base-{sha}.tar"));
                            let git = kgraf_sandbox::git::Git::discover()
                                .map_err(|e| CheckError::Config(e.to_string()))?;
                            let out = t.to_string_lossy().into_owned();
                            git.run(repo, ["archive", "--format=tar", "-o", &out, &sha])
                                .map_err(|e| {
                                    CheckError::Config(format!("git archive {sha}: {e}"))
                                })?;
                            let t = Arc::new(Tarball(t));
                            cur.archive = Some(t.clone());
                            t
                        }
                    }
                };
                let tree = tree.to_path_buf();
                blocking(move || {
                    let tar = tarball.0.clone();
                    let out = std::process::Command::new("tar")
                        .arg("-xf")
                        .arg(&tar)
                        .arg("-C")
                        .arg(&tree)
                        .output()
                        .map_err(io(&tar))?;
                    if !out.status.success() {
                        return Err(CheckError::Config(format!(
                            "cannot extract {}: {}",
                            tar.display(),
                            String::from_utf8_lossy(&out.stderr).trim()
                        )));
                    }
                    Ok(())
                })
                .await?;
            }
        }
        let tree = tree.to_path_buf();
        // Descriptor-relative, no-follow writes: the base may hold a symbolic
        // link a worker committed (`tests -> /home/you`), and a host-side write
        // through it would land outside the tree. An overlay
        // whose path crosses a link, or ends at one, is refused (decision
        // 0083), so the check could not run and the contract is held with
        // that reason.
        blocking(move || {
            for f in &layers {
                kgraf_sandbox::write_confined(&tree, Path::new(&f.path), f.content.as_bytes())
                    .map_err(io(&tree.join(&f.path)))?;
            }
            Ok(())
        })
        .await
    }

    fn policy(&self, run_dir: &Path) -> SandboxPolicy {
        // Never `with_ancestor_reads`: the tree is a copy under a scratch root
        // (`/tmp/kgraf-checks` first) whose parents are not the owner's,
        // and another user could plant a `.cargo/config.toml` there whose
        // `runner` would run as the owner and forge a pass. A `.cargo/config.toml` the repository tracks is in the copy
        // already; a cargo file above the tree makes cargo fail loud instead.
        let mut policy = SandboxPolicy::new(vec![run_dir.to_path_buf()], self.network.network())
            .with_tmp_dir(run_dir.join("tmp"))
            // The Windows backend is always partial; a check runs there under
            // the same decision that opens its network, and says so
            // (`CheckNetwork::accepts_partial_isolation`, decision 0011).
            .accept_partial_isolation(
                self.accept_partial || self.network.accepts_partial_isolation(),
            );
        // Every process of the check holds this file shared until it exits,
        // so its absence of holders proves the tree is gone. It lies
        // outside the check's only writable root, beside `.out` and `.err`, as
        // a worktree's hold lies outside the worktree: a check that could
        // delete and recreate it would free an escaped descendant's lock.
        #[cfg(unix)]
        {
            policy = policy.holding(hold_of(run_dir));
        }
        for d in &self.read_denies {
            policy = policy.deny_read(d.clone());
        }
        // The checkout's installed dependency trees (`node_modules/`, a
        // `.venv/`, `vendor/`), linked into the tree and readable, never
        // writable (decision 0084, amended 2026-10-06): a check of a
        // TypeScript or Python contract needs the project's compiler and
        // libraries. A tree copied from a directory already holds them.
        if let Resolved::Commit { repo, .. } = &self.base {
            let links = kgraf_sandbox::link_dependency_trees(&run_dir.join("tree"), repo);
            policy = policy.with_dependency_links(&links);
        }
        policy
    }

    /// [`Self::policy`] for one command: `sh -c 'cd svc/api && pytest'` runs
    /// with the virtual environment of `svc/api`, the folder it works in.
    fn policy_for(&self, run_dir: &Path, argv: &[String]) -> SandboxPolicy {
        let policy = self.policy(run_dir);
        match argv {
            [shell, flag, script]
                if matches!(shell.as_str(), "sh" | "bash" | "/bin/sh" | "/bin/bash")
                    && flag == "-c" =>
            {
                match kgraf_sandbox::script_start_dir(script) {
                    Some(dir) => policy.with_command_dir(run_dir.join("tree").join(dir)),
                    None => policy,
                }
            }
            _ => policy,
        }
    }

    #[cfg(unix)]
    async fn exec(
        &self,
        name: &str,
        run_dir: &Path,
        argv: &[String],
        timeout: Duration,
    ) -> Result<RunResult, CheckError> {
        use std::os::unix::process::CommandExt;
        use std::process::Stdio;
        let Some((program, args)) = argv.split_first() else {
            return Err(CheckError::Config("an empty command".into()));
        };
        let program = program.clone();
        let mut cmd = sandboxed_command(
            &self.policy_for(run_dir, argv),
            &program,
            &run_dir.join("tree"),
        )
        .map_err(|e| CheckError::Sandbox(e.to_string()))?;
        cmd.args(args)
            .env("CARGO_TARGET_DIR", run_dir.join("target"))
            .env("CARGO_NET_OFFLINE", "true")
            .env("PYTHONDONTWRITEBYTECODE", "1");
        let out_path = self.scratch.join(format!("{name}.out"));
        let err_path = self.scratch.join(format!("{name}.err"));
        let out = std::fs::File::create(&out_path).map_err(io(&out_path))?;
        let err = std::fs::File::create(&err_path).map_err(io(&err_path))?;
        let (mut std_cmd, _probe) = cmd.into_parts();
        std_cmd
            .stdin(Stdio::null())
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err));
        let limits = self.limits;
        // SAFETY: `setrlimit` is async-signal-safe and the closure allocates
        // nothing; it runs in the child between fork and exec.
        unsafe {
            std_cmd.pre_exec(move || {
                let lim = libc::rlimit {
                    rlim_cur: limits.file_size_bytes as libc::rlim_t,
                    rlim_max: limits.file_size_bytes as libc::rlim_t,
                };
                if libc::setrlimit(libc::RLIMIT_FSIZE, &lim) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        // RLIMIT_CPU: shared with the gate runner (`kgraf_sandbox::lifecycle`),
        // so the bound and its rationale are set in one place (see this
        // module's doc comment).
        // SAFETY: forwarded from `limit_cpu_seconds`'s own contract.
        unsafe {
            kgraf_sandbox::lifecycle::limit_cpu_seconds(&mut std_cmd, limits.cpu_seconds);
        }
        let mut child = tokio::process::Command::from(std_cmd)
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| match e.kind() {
                // Linux execs the program itself after confining the child:
                // absent, or not executable under the policy, is a missing
                // program, not a sandbox failure and not a verdict.
                std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied => {
                    CheckError::ProgramMissing {
                        program: program.clone(),
                        detail: format!("the sandboxed exec failed: {e}"),
                    }
                }
                _ => CheckError::Sandbox(format!("cannot spawn: {e}")),
            })?;
        // The gates' lifecycle : the group is killed
        // while its leader is still unreaped, so its id cannot name another
        // program's group, until the hold file shows every member gone; only
        // then is the leader reaped. The guard kills the group if this future
        // is dropped (a cancelled check) before the leader is reaped.
        let pid = child
            .id()
            .ok_or_else(|| CheckError::Sandbox("the check's process has no id".into()))?;
        let mut group = Some(GroupKill(pid));
        let hold = hold_of(run_dir);
        let deadline = tokio::time::Instant::now() + timeout;
        let (success, timed_out, status, code) = loop {
            let exited = kgraf_sandbox::lifecycle::exited_unreaped(pid)
                .map_err(|e| CheckError::Sandbox(format!("waiting for the check: {e}")))?;
            if exited {
                let h = hold.clone();
                blocking(move || {
                    kgraf_sandbox::lifecycle::kill_group_until_hold_free(pid, &h);
                    Ok(())
                })
                .await?;
                let s = child
                    .wait()
                    .await
                    .map_err(|e| CheckError::Sandbox(e.to_string()))?;
                break (s.success(), false, s.to_string(), s.code());
            }
            if tokio::time::Instant::now() >= deadline {
                // Still running, so still unreaped: the group id is its own.
                let h = hold.clone();
                blocking(move || {
                    kgraf_sandbox::lifecycle::kill_group_until_hold_free(pid, &h);
                    Ok(())
                })
                .await?;
                let _ = child.kill().await;
                break (false, true, "timed out".to_owned(), None);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        // Reaped: the id may be reused from here on, so no signal is sent to it.
        if let Some(g) = group.take() {
            std::mem::forget(g);
        }
        // Quiesce: a process that left the group (its own `setsid`) still holds
        // the hold file; the tree is not quiet, and the check did not end.
        if !kgraf_sandbox::lifecycle::hold_free(&hold) {
            return Err(CheckError::Sandbox(
                "a process the check started left its process group and is still running".into(),
            ));
        }
        let tail = blocking(move || {
            Ok(join_tail(
                &last_bytes(&err_path, SCAN_BYTES),
                &last_bytes(&out_path, SCAN_BYTES),
            ))
        })
        .await?;
        if !success
            && !timed_out
            && let Some((named, detail)) = missing_program(code, &tail)
        {
            // The shell names what it could not open: when that is one of the
            // command's own arguments and the program itself ran, a file is
            // missing, not a program.
            if let Some(file) = named.as_deref().filter(|n| {
                *n != program
                    && detail.contains("No such file")
                    && args.iter().any(|a| {
                        a.strip_prefix("./").unwrap_or(a) == n.strip_prefix("./").unwrap_or(n)
                    })
            }) {
                return Err(CheckError::InputMissing {
                    program: program.clone(),
                    file: file.to_owned(),
                    detail,
                });
            }
            return Err(CheckError::ProgramMissing {
                program: named.unwrap_or_else(|| program.clone()),
                detail,
            });
        }
        Ok(RunResult {
            queued: Duration::ZERO,
            success,
            timed_out,
            status,
            tail,
        })
    }

    /// Windows: `SandboxedChild` has no async wait and its stdio cannot be sent
    /// to a file, so the child's output is discarded and it is polled; the job
    /// object kills the whole tree on timeout and on drop.
    #[cfg(windows)]
    async fn exec(
        &self,
        _name: &str,
        run_dir: &Path,
        argv: &[String],
        timeout: Duration,
    ) -> Result<RunResult, CheckError> {
        use kgraf_sandbox::Redirect;
        let Some((program, args)) = argv.split_first() else {
            return Err(CheckError::Config("an empty command".into()));
        };
        let mut cmd = sandboxed_command(
            &self.policy_for(run_dir, argv),
            program,
            &run_dir.join("tree"),
        )
        .map_err(|e| CheckError::Sandbox(e.to_string()))?;
        // `CARGO_NET_OFFLINE` stays with the network open: the check judges
        // the code against the dependencies the user already fetched, as on
        // every other platform, and the sandbox cannot write the user's cargo
        // home here either (a Low integrity process cannot write the user's
        // files), so a fetch could only fail after its download.
        cmd.args(args)
            .env("CARGO_TARGET_DIR", run_dir.join("target"))
            .env("CARGO_NET_OFFLINE", "true")
            .env("PYTHONDONTWRITEBYTECODE", "1");
        cmd.stdin(Redirect::Null)
            .stdout(Redirect::Null)
            .stderr(Redirect::Null);
        blocking(move || {
            let mut child = cmd
                .spawn()
                .map_err(|e| CheckError::Sandbox(e.to_string()))?;
            let deadline = std::time::Instant::now() + timeout;
            loop {
                match child.try_wait() {
                    Ok(Some(s)) => {
                        let _ = child.kill();
                        return Ok(RunResult {
                            queued: Duration::ZERO,
                            success: s.success(),
                            timed_out: false,
                            status: s.to_string(),
                            tail: "(output is not captured on Windows)".into(),
                        });
                    }
                    Ok(None) if std::time::Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    Ok(None) => {
                        let _ = child.kill();
                        return Ok(RunResult {
                            queued: Duration::ZERO,
                            success: false,
                            timed_out: true,
                            status: "timed out".into(),
                            tail: String::new(),
                        });
                    }
                    Err(e) => return Err(CheckError::Sandbox(e.to_string())),
                }
            }
        })
        .await
    }

    #[cfg(not(any(unix, windows)))]
    async fn exec(
        &self,
        _name: &str,
        _run_dir: &Path,
        _argv: &[String],
        _timeout: Duration,
    ) -> Result<RunResult, CheckError> {
        Err(CheckError::Sandbox("no sandbox on this platform".into()))
    }
}

/// A run's hold file: `<scratch>/<name>.hold`, beside the run's directory and
/// outside it (the check can write only inside its directory).
fn hold_of(run_dir: &Path) -> PathBuf {
    let mut name = run_dir.as_os_str().to_owned();
    name.push(".hold");
    PathBuf::from(name)
}

/// The session log the sandbox keeps beside a hold file
/// (`kgraf_sandbox`'s `session_log`: the hold's name plus `+sessions`).
fn sessions_of(hold: &Path) -> PathBuf {
    let mut name = hold.as_os_str().to_owned();
    name.push("+sessions");
    PathBuf::from(name)
}

/// Whether a failed run's exit code and output say a program was missing
/// inside the sandbox, rather than that the command ran and failed: the check
/// could not run. Returns the program when the output names
/// it, and the evidence.
///
/// - exit 127: a shell (`sh -c "python3 ..."`) or `env` did not find a program;
///   `sh: NAME: command not found` names it.
/// - exit 71 with `execvp() of 'NAME' failed`: macOS `sandbox-exec` could not
///   exec the program under the policy.
/// - `xcrun: error: invalid active developer path`: a macOS shim
///   (`/usr/bin/python3`, `/usr/bin/git`, ...) whose Command Line Tools are not
///   installed. The shim exists, so a lookup finds it; only running it shows
///   that nothing is behind it.
///
/// Probing is the run itself, under the same policy as the check, so what the
/// sandbox can and cannot reach is what is classified.
// Only the unix run path calls this; Windows refuses to run checks at all.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn missing_program(code: Option<i32>, tail: &str) -> Option<(Option<String>, String)> {
    let line_with = |needle: &str| {
        tail.lines()
            .find(|l| l.contains(needle))
            .map(|l| l.trim().to_owned())
    };
    if let Some(line) = line_with("xcrun: error: invalid active developer path") {
        return Some((None, line));
    }
    if code == Some(71)
        && let Some(line) = line_with("execvp() of '")
    {
        let named = line
            .split("execvp() of '")
            .nth(1)
            .and_then(|r| r.split('\'').next())
            .map(str::to_owned);
        return Some((named, line));
    }
    if code == Some(127) {
        let line = line_with("not found").or_else(|| line_with("No such file"));
        let named = line.as_deref().and_then(|l| {
            // `sh: NAME: command not found`, `sh: 1: NAME: not found`,
            // `env: 'NAME': No such file or directory`.
            let parts: Vec<&str> = l.split(':').map(str::trim).collect();
            parts
                .iter()
                .rev()
                .nth(1)
                .map(|n| n.trim_matches(|c| c == '\'' || c == '"').to_owned())
                .filter(|n| !n.is_empty())
        });
        return Some((
            named,
            line.unwrap_or_else(|| "exit status 127: a program was not found".to_owned()),
        ));
    }
    None
}

/// Whether `program` resolves to an executable file the way the sandboxed
/// child resolves it: a bare name on `PATH` (the sandbox passes `PATH` through),
/// a path with a separator against the check tree, which is the child's working
/// directory. [`CheckError::ProgramMissing`] when it does not.
///
/// Written because the live path showed the gap: a sandboxed exec of a missing
/// program fails like any failing command, so `python3` absent from a machine
/// read as "the consumer expectations do not compile against the stub", a
/// verdict on the contract. Now it is a check that could not run, and the
/// contract is held with that reason. Searched: `which` (MIT) does this
/// lookup, but the workspace does not depend on it and the rule is ten lines.
fn program_installed(program: &str, tree: &Path) -> Result<(), CheckError> {
    let missing = |detail: String| CheckError::ProgramMissing {
        program: program.to_owned(),
        detail,
    };
    let path = Path::new(program);
    if path.components().count() > 1 || path.is_absolute() {
        let full = if path.is_absolute() {
            path.to_path_buf()
        } else {
            tree.join(path)
        };
        return if is_executable(&full) {
            Ok(())
        } else {
            Err(missing(format!("no executable file at {}", full.display())))
        };
    }
    let Some(dirs) = std::env::var_os("PATH") else {
        return Err(missing("PATH is not set".into()));
    };
    #[cfg(windows)]
    let names: Vec<String> = {
        let exts = std::env::var("PATHEXT").unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".into());
        std::iter::once(program.to_owned())
            .chain(exts.split(';').map(|e| format!("{program}{e}")))
            .collect()
    };
    #[cfg(not(windows))]
    let names = [program.to_owned()];
    for dir in std::env::split_paths(&dirs) {
        if names.iter().any(|n| is_executable(&dir.join(n))) {
            return Ok(());
        }
    }
    Err(missing("not found on PATH".into()))
}

fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.is_file() && meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        meta.is_file()
    }
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, CheckError> + Send + 'static,
) -> Result<T, CheckError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| CheckError::Config(e.to_string()))?
}

/// Kills a process group on drop: when the run's future is dropped (a
/// cancelled check) while the leader is not yet reaped. Forgotten once the
/// leader is reaped, when its id may name another program.
#[cfg(unix)]
struct GroupKill(u32);

#[cfg(unix)]
impl Drop for GroupKill {
    fn drop(&mut self) {
        if let Ok(pid) = libc::pid_t::try_from(self.0) {
            // SAFETY: a plain syscall; a group that no longer exists is ESRCH.
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
    }
}

/// Removes a run's files when the run ends or its future is dropped.
struct Cleanup(Vec<PathBuf>);

impl Drop for Cleanup {
    fn drop(&mut self) {
        for p in &self.0 {
            if p.is_dir() {
                let _ = std::fs::remove_dir_all(p);
            } else {
                let _ = std::fs::remove_file(p);
            }
        }
    }
}

// Only read by the `#[cfg(unix)]` branch above (line ~398..465) that tails a failed
// gate's captured stderr/stdout for a remediation prompt; the `#[cfg(windows)]` branch
// builds its own message differently, so neither helper is used on that target
// (a Windows cross-build found this under `-D warnings`).
#[cfg(unix)]
fn last_bytes(path: &Path, keep: usize) -> Vec<u8> {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(keep as u64);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::with_capacity(keep.min(len as usize));
    let _ = f.take(keep as u64).read_to_end(&mut buf);
    buf
}

#[cfg(unix)]
fn join_tail(err: &[u8], out: &[u8]) -> String {
    // Host toolchain noise is dropped first, so it does not take room from the
    // errors in the byte budget, and never reaches a model, the trace or a
    // notice.
    format!(
        "{}\n{}",
        condense(
            &filter_host_noise(&String::from_utf8_lossy(err)),
            DEFAULT_TAIL_BYTES
        ),
        condense(
            &filter_host_noise(&String::from_utf8_lossy(out)),
            DEFAULT_TAIL_BYTES
        )
    )
    .trim()
    .to_owned()
}

/// Whether a line is output of the host's own developer tools and not of the
/// project: macOS `xcrun` and `xcodebuild` lines a compiler relays while it
/// looks for the SDK. Never an error line (`error: ...`, `file:1: error: ...`):
/// a missing toolchain is a finding, whatever tool says it.
#[cfg_attr(not(unix), allow(dead_code))]
fn is_host_noise(line: &str) -> bool {
    !starts_error(line)
        && (line.contains("DVTDeveloperPaths")
            || line.contains("DVTFilePathFSEvents")
            || line.contains("xcodebuild[")
            || line.contains("codebuild[")
            || line.contains("xcrun")
            || line.contains("MacOSX.sdk"))
}

/// A line that continues a warning block: an indented line, a `|` gutter, a
/// numbered source line or a `= note` line. A blank line ends the block.
#[cfg_attr(not(unix), allow(dead_code))]
fn continues_block(line: &str) -> bool {
    !line.trim().is_empty() && continues_error(line)
}

/// `text` without the host's toolchain noise and without repeated identical
/// warning blocks, each kind said once at the end with its count
/// (`[N identical lines removed]`). Error lines are never removed. A compiler
/// that relays one `xcrun` warning for every crate of a workspace made one
/// refusal 28 KB, nearly all of it these.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn filter_host_noise(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut kept: Vec<&str> = Vec::with_capacity(lines.len());
    let mut noise = 0usize;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        // A warning block about host tools: its header, then every line of it.
        if line.starts_with("warning:") && is_host_noise(line) {
            noise += 1;
            i += 1;
            while i < lines.len() && continues_block(lines[i]) {
                noise += 1;
                i += 1;
            }
            continue;
        }
        if is_host_noise(line) {
            noise += 1;
        } else {
            kept.push(line);
        }
        i += 1;
    }
    // Identical warning blocks: the first stays.
    let mut out: Vec<&str> = Vec::with_capacity(kept.len());
    let mut seen: BTreeSet<Vec<&str>> = BTreeSet::new();
    let mut repeated = 0usize;
    let mut i = 0;
    while i < kept.len() {
        if kept[i].starts_with("warning:") {
            let mut j = i + 1;
            while j < kept.len() && continues_block(kept[j]) {
                j += 1;
            }
            let block = kept[i..j].to_vec();
            if seen.contains(&block) {
                repeated += block.len();
            } else {
                out.extend(block.iter().copied());
                seen.insert(block);
            }
            i = j;
        } else {
            out.push(kept[i]);
            i += 1;
        }
    }
    if noise == 0 && repeated == 0 {
        return text.to_owned();
    }
    let mut result = out.join("\n");
    if !result.is_empty() {
        result.push('\n');
    }
    if noise > 0 {
        result.push_str(&format!("[{noise} host toolchain lines removed]\n"));
    }
    if repeated > 0 {
        result.push_str(&format!("[{repeated} identical lines removed]\n"));
    }
    result
}

/// Whether a line starts a compiler diagnostic that is an error: rustc's
/// `error[E0560]: ...` and `error: ...`, and the `file:line: error: ...` form
/// of C compilers and many others.
#[cfg_attr(not(unix), allow(dead_code))]
fn starts_error(line: &str) -> bool {
    line.starts_with("error[")
        || line.starts_with("error:")
        || line.contains(": error:")
        || line.contains(": error[")
}

/// Whether a line continues the diagnostic above it: rustc's ` --> path`,
/// `  |` gutter, `3 | code` and `= note` lines, all indented or numbered.
#[cfg_attr(not(unix), allow(dead_code))]
fn continues_error(line: &str) -> bool {
    line.starts_with([' ', '\t', '|', '='])
        || line.split_once('|').is_some_and(|(n, _)| {
            let n = n.trim();
            !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())
        })
}

/// `text` cut to at most `keep` bytes, compiler errors first: every error
/// block (the error line and the context lines after it) up to three quarters
/// of `keep`, then the end of the output in the room left. A plain tail let
/// trailing warnings (a toolchain's own noise on stderr) push the one error
/// that explains a failed check out of the record.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn condense(text: &str, keep: usize) -> String {
    const MARKER: &str = "[... output cut; the compiler errors above were kept ...]\n";
    if text.len() <= keep {
        return text.to_owned();
    }
    let budget = (keep / 4 * 3).saturating_sub(MARKER.len());
    let mut errors = String::new();
    let mut in_block = false;
    for line in text.lines() {
        in_block = if starts_error(line) {
            true
        } else {
            in_block && !line.trim().is_empty() && continues_error(line)
        };
        // A block that does not fit is left out whole line by line; later
        // shorter lines may still fit, which keeps more locations.
        if in_block && errors.len() + line.len() < budget {
            errors.push_str(line);
            errors.push('\n');
        }
    }
    let used = if errors.is_empty() {
        0
    } else {
        errors.len() + MARKER.len()
    };
    let mut start = text.len() - keep.saturating_sub(used).min(text.len());
    while !text.is_char_boundary(start) {
        start += 1;
    }
    let tail = &text[start..];
    if errors.is_empty() {
        tail.to_owned()
    } else {
        format!("{errors}{MARKER}{tail}")
    }
}

/// Copy `from` to `to`, skipping only the top-level `.git` and `.kgraf`.
/// Symbolic links are recreated as links on Unix; elsewhere a link is refused
/// rather than dropped or followed.
fn copy_tree(from: &Path, to: &Path, top: bool) -> Result<(), CheckError> {
    std::fs::create_dir_all(to).map_err(io(to))?;
    for entry in std::fs::read_dir(from).map_err(io(from))? {
        let entry = entry.map_err(io(from))?;
        let name = entry.file_name();
        let src = entry.path();
        let dst = to.join(&name);
        let kind = entry.file_type().map_err(io(&src))?;
        if top
            && (name == ".git"
                || name == kgraf_core::legacy::STATE_DIR
                || name == kgraf_core::legacy::LEGACY_STATE_DIR)
        {
            continue;
        }
        if kind.is_symlink() {
            #[cfg(unix)]
            {
                let target = std::fs::read_link(&src).map_err(io(&src))?;
                std::os::unix::fs::symlink(target, &dst).map_err(io(&dst))?;
            }
            #[cfg(not(unix))]
            return Err(CheckError::Config(format!(
                "{} is a symbolic link; a check tree cannot reproduce it here",
                src.display()
            )));
        } else if kind.is_dir() {
            copy_tree(&src, &dst, false)?;
        } else {
            std::fs::copy(&src, &dst).map_err(io(&src))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_TAIL_BYTES, RunResult, condense, filter_host_noise, missing_program};

    /// A compile error in the middle of the output, then more than the cap of
    /// trailing toolchain warnings: the error and its location are kept.
    #[test]
    fn compiler_errors_survive_trailing_warnings() {
        let mut out = String::from("   Compiling iface v0.1.0\n");
        out.push_str("error[E0560]: struct `Receipt` has no field named `taxable_cents`\n");
        out.push_str(" --> pricing_engine/src/lib.rs:3:53\n");
        out.push_str("  |\n");
        out.push_str("3 |     Receipt { total_cents, taxable_cents }\n");
        out.push_str("  |                            ^^^^^^^^^^^^^ no such field\n\n");
        for i in 0..80 {
            out.push_str(&format!(
                "2026-01-01 xcrun[{i}] DVTFilePathFSEvents: Failed to start fs event stream.\n"
            ));
        }
        assert!(out.len() > 5 * 1024, "{}", out.len());
        let kept = condense(&out, DEFAULT_TAIL_BYTES);
        assert!(kept.len() <= DEFAULT_TAIL_BYTES, "{}", kept.len());
        assert!(kept.contains("error[E0560]"), "{kept}");
        assert!(kept.contains("pricing_engine/src/lib.rs:3:53"), "{kept}");
        assert!(kept.contains("^^^^^^^^^^^^^ no such field"), "{kept}");
        assert!(
            !kept.contains("Compiling iface"),
            "a line that is not an error is not moved up: {kept}"
        );
        assert!(
            kept.ends_with("Failed to start fs event stream.\n"),
            "the end of the output is still kept"
        );
        let r = RunResult {
            success: false,
            timed_out: false,
            status: "exit status: 101".into(),
            tail: kept,
            queued: std::time::Duration::ZERO,
        };
        assert!(
            r.describe(&["cargo".into(), "build".into()])
                .contains("E0560")
        );
        // Output under the cap is kept whole; output with no error is a tail.
        assert_eq!(condense("short", 10), "short");
        let noise = "w\n".repeat(3_000);
        assert_eq!(condense(&noise, 100), noise[noise.len() - 100..]);
    }

    /// A real 5.7 KB refusal of the distributed-efficacy benchmark (a vacuity
    /// failure on macOS): `xcrun` and `xcodebuild` lines the compiler relayed
    /// for every crate, the same warning for each crate, and the three failing
    /// tests. The noise goes, with its count; the verdict, the failing tests
    /// and their panics stay.
    #[test]
    fn host_toolchain_noise_is_removed_from_a_recorded_refusal_and_the_failure_stays() {
        let recorded =
            include_str!("../tests/fixtures/check-output/vacuity-refusal-with-xcrun-noise.txt");
        let filtered = filter_host_noise(recorded);
        for gone in ["xcrun", "xcodebuild", "DVTDeveloperPaths", "MacOSX.sdk"] {
            assert!(!filtered.contains(gone), "{gone} is host noise: {filtered}");
        }
        for kept in [
            "the test fails against the reference, so nothing may satisfy it",
            "test vip_flat_discount_default_tax_free_shipping ... FAILED",
            "not yet implemented: contract 6: checkout::checkout_order is not implemented yet",
            "test result: FAILED. 0 passed; 3 failed",
            "error: test failed, to rerun pass `-p checkout --test integration`",
        ] {
            assert!(filtered.contains(kept), "{kept} stays: {filtered}");
        }
        assert!(
            filtered.contains("host toolchain lines removed"),
            "the removal is said: {filtered}"
        );
        assert!(
            filtered.len() * 10 < recorded.len() * 7,
            "{} of {} bytes remain",
            filtered.len(),
            recorded.len()
        );
    }

    /// An error line is never noise, even when a host tool says it: a missing
    /// Command Line Tools install is the finding. Identical warning blocks
    /// are said once, with the count of the lines removed.
    #[test]
    fn error_lines_stay_and_identical_warning_blocks_are_said_once() {
        let text = "xcrun: error: invalid active developer path (/Library/Developer/CommandLineTools)\n\
                    warning: unused variable: `x`\n --> a/src/lib.rs:1:1\n  |\n1 | let x = 1;\n\n\
                    warning: unused variable: `x`\n --> a/src/lib.rs:1:1\n  |\n1 | let x = 1;\n\n\
                    warning: unused variable: `y`\n --> b/src/lib.rs:1:1\n";
        let filtered = filter_host_noise(text);
        assert!(filtered.contains("xcrun: error: invalid active developer path"));
        assert_eq!(
            filtered.matches("unused variable: `x`").count(),
            1,
            "{filtered}"
        );
        assert!(filtered.contains("unused variable: `y`"), "{filtered}");
        assert!(
            filtered.contains("[4 identical lines removed]"),
            "{filtered}"
        );
        assert!(!filtered.contains("host toolchain"), "{filtered}");
        // Nothing to remove: the text is returned as it came.
        assert_eq!(
            filter_host_noise("error: boom\nwarning: once\n"),
            "error: boom\nwarning: once\n"
        );
    }

    /// The filter runs before the byte budget: with the noise first and the
    /// error after it, the error is in the tail the model reads.
    #[cfg(unix)]
    #[test]
    fn the_tail_a_model_reads_carries_no_host_noise() {
        let mut err = String::new();
        for i in 0..200 {
            err.push_str(&format!(
                "2026-01-01 xcodebuild[{i}] DVTDeveloperPaths: Failed to get length\n"
            ));
        }
        err.push_str("error[E0425]: cannot find value `x` in this scope\n --> a/src/lib.rs:2:5\n");
        let tail = super::join_tail(err.as_bytes(), b"");
        assert!(tail.contains("error[E0425]"), "{tail}");
        assert!(!tail.contains("DVTDeveloperPaths"), "{tail}");
        assert!(tail.len() < 400, "{} bytes", tail.len());
    }

    #[test]
    fn a_missing_program_is_told_apart_from_a_failing_command() {
        let (named, _) = missing_program(Some(127), "sh: python9: command not found").expect("sh");
        assert_eq!(named.as_deref(), Some("python9"));
        let (named, _) = missing_program(Some(127), "sh: 1: python9: not found").expect("dash");
        assert_eq!(named.as_deref(), Some("python9"));
        let (named, _) =
            missing_program(Some(127), "env: 'python9': No such file or directory").expect("env");
        assert_eq!(named.as_deref(), Some("python9"));
        let (named, _) = missing_program(
            Some(71),
            "sandbox-exec: execvp() of 'cargo' failed: No such file or directory",
        )
        .expect("seatbelt");
        assert_eq!(named.as_deref(), Some("cargo"));
        assert!(
            missing_program(
                Some(1),
                "xcrun: error: invalid active developer path (/Library/Developer/CommandLineTools), \
                 missing xcrun at: /Library/Developer/CommandLineTools/usr/bin/xcrun"
            )
            .is_some(),
            "the macOS shim with no Command Line Tools"
        );
        // A command that ran and failed is a result, never a missing program.
        assert!(missing_program(Some(1), "AssertionError: 0 != 8080").is_none());
        assert!(missing_program(Some(101), "error[E0425]: cannot find value").is_none());
        assert!(missing_program(Some(71), "some other failure").is_none());
    }
}

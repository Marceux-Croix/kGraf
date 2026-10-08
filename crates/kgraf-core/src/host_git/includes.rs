//! Every file git may read configuration from for a repository, found
//! without following a condition: the config files git lists (system,
//! global, the repository's own, a per-worktree one) and, read from each of
//! them recursively, every `include.path` and `includeIf.<condition>.path`
//! target, whether it exists yet or not.
//!
//! Decision 0094 freezes these in a Bypass shell command's sandbox: a
//! command that could write one of them while kGraf's git runs could add a
//! driver or hook between kGraf's configuration listing and the command it
//! prepares. A condition is not evaluated (`gitdir:` matches differently in
//! each worktree, `onbranch:` changes with a checkout), so every target is
//! included. A target that does not exist is still returned: git skips a
//! missing include silently, so creating it later would add configuration.
//!
//! [`IncludeScan`] caches the answer per repository and reads the files
//! again only when one of them changed (length, modification and change
//! times, inode), or was written within [`RACY`] of the last scan, the margin
//! a coarse file system clock needs.

use super::{HostGitError, base, fixed_overrides, set_overrides};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime};

/// Git's own include depth limit (`MAX_INCLUDE_DEPTH`).
const MAX_DEPTH: usize = 10;

/// A stamp younger than this is not trusted to show the next change.
pub const RACY: Duration = Duration::from_secs(3);

/// What identifies one version of a file, or its absence.
type Stamp = Option<(u64, SystemTime, SystemTime, u64)>;

/// Every path a scan looked at, with its stamp then.
type Seen = Vec<(PathBuf, Stamp)>;

fn stamp(path: &Path) -> Stamp {
    let meta = std::fs::metadata(path).ok()?;
    #[cfg(unix)]
    let (changed, inode) = {
        use std::os::unix::fs::MetadataExt;
        let ctime = SystemTime::UNIX_EPOCH
            + Duration::new(
                u64::try_from(meta.ctime()).unwrap_or(0),
                u32::try_from(meta.ctime_nsec()).unwrap_or(0),
            );
        (ctime, meta.ino())
    };
    #[cfg(not(unix))]
    let (changed, inode) = (meta.modified().unwrap_or(SystemTime::UNIX_EPOCH), 0);
    Some((
        meta.len(),
        meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        changed,
        inode,
    ))
}

fn settled(s: &Stamp, now: SystemTime) -> bool {
    match s {
        None => true,
        Some((_, modified, changed, _)) => [modified, changed]
            .iter()
            .all(|t| now.duration_since(**t).is_ok_and(|age| age >= RACY)),
    }
}

/// A cached scan of one repository's configuration files.
#[derive(Debug, Default)]
pub struct IncludeScan {
    last: Option<Last>,
}

#[derive(Debug)]
struct Last {
    common_dir: PathBuf,
    /// Every file the scan read or looked for, with its stamp then.
    seen: Seen,
    settled: bool,
    files: Vec<PathBuf>,
}

impl IncludeScan {
    pub fn new() -> Self {
        Self::default()
    }

    /// The configuration files of the repository whose shared git directory
    /// is `common_dir`, include targets among them. Cached until one of the
    /// files it read changes.
    pub fn files(&mut self, git: &Path, common_dir: &Path) -> Result<Vec<PathBuf>, HostGitError> {
        if let Some(last) = &self.last
            && last.common_dir == common_dir
            && last.settled
            && last.seen.iter().all(|(p, s)| stamp(p) == *s)
        {
            return Ok(last.files.clone());
        }
        let now = SystemTime::now();
        let (files, seen) = scan(git, common_dir)?;
        let settled = seen.iter().all(|(_, s)| settled(s, now));
        self.last = Some(Last {
            common_dir: common_dir.to_path_buf(),
            seen,
            settled,
            files: files.clone(),
        });
        Ok(files)
    }
}

/// One uncached scan: the files, and every path looked at with its stamp.
///
/// One git process lists the repository's configuration with includes and
/// origins: every file it read (system, global, the repository's own, a
/// per-worktree one, each include whose condition held) and every include
/// directive in them. A target git did not read (missing, or behind a
/// condition that did not hold here) is still listed; one that exists is
/// read on its own, as is another worktree's `config.worktree`. So the
/// common case costs one process.
fn scan(git: &Path, common_dir: &Path) -> Result<(Vec<PathBuf>, Seen), HostGitError> {
    let mut read: BTreeSet<PathBuf> = BTreeSet::new();
    let mut files: BTreeSet<PathBuf> = BTreeSet::new();
    let mut queue: Vec<(PathBuf, usize)> = Vec::new();
    let pre = [
        std::ffi::OsString::from("--git-dir"),
        common_dir.as_os_str().to_owned(),
    ];
    let repo = super::Invocation {
        exe: git,
        cwd: Some(common_dir),
        pre: &pre,
    };
    let first = listing(&repo, None)?;
    absorb(&first, common_dir, 1, &mut read, &mut files, &mut queue);
    let mut extra = vec![
        common_dir.join("config"),
        common_dir.join("config.worktree"),
    ];
    if let Ok(entries) = std::fs::read_dir(common_dir.join("worktrees")) {
        extra.extend(entries.flatten().map(|e| e.path().join("config.worktree")));
    }
    queue.extend(extra.into_iter().map(|p| (p, 0)));
    while let Some((file, depth)) = queue.pop() {
        files.insert(file.clone());
        if read.contains(&file) || depth > MAX_DEPTH || !file.is_file() {
            continue;
        }
        let dir = file.parent().unwrap_or(Path::new("/"));
        let inv = super::Invocation {
            exe: git,
            cwd: Some(dir),
            pre: &[],
        };
        let out = listing(&inv, Some(&file))?;
        read.insert(file.clone());
        absorb(&out, dir, depth + 1, &mut read, &mut files, &mut queue);
    }
    let seen: Seen = files.iter().map(|p| (p.clone(), stamp(p))).collect();
    Ok((files.into_iter().collect(), seen))
}

/// `git config [--file F] --includes --show-origin -z --list`: NUL-separated
/// `origin`, `key LF value` pairs.
fn listing(inv: &super::Invocation<'_>, file: Option<&Path>) -> Result<Vec<u8>, HostGitError> {
    let mut args: Vec<String> = vec!["config".into()];
    if let Some(f) = file {
        args.push("--file".into());
        args.push(f.display().to_string());
    }
    args.extend(["--includes", "--show-origin", "-z", "--list"].map(String::from));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    Ok(run(inv, &args)?.unwrap_or_default())
}

/// Every origin file of a listing goes to `read` and `files`; every include
/// directive's target, resolved from its origin file's directory, to `files`
/// and the queue (`depth` deep).
fn absorb(
    out: &[u8],
    base: &Path,
    depth: usize,
    read: &mut BTreeSet<PathBuf>,
    files: &mut BTreeSet<PathBuf>,
    queue: &mut Vec<(PathBuf, usize)>,
) {
    let mut records = out.split(|b| *b == 0);
    while let (Some(origin), Some(entry)) = (records.next(), records.next()) {
        let origin = String::from_utf8_lossy(origin);
        let Some(path) = origin.strip_prefix("file:") else {
            continue;
        };
        let origin_file = absolute(base, Path::new(path));
        read.insert(origin_file.clone());
        files.insert(origin_file.clone());
        let (key, value) = match entry.iter().position(|b| *b == b'\n') {
            Some(nl) => (&entry[..nl], &entry[nl + 1..]),
            None => (entry, &[][..]),
        };
        let key = String::from_utf8_lossy(key).to_ascii_lowercase();
        let is_include =
            key == "include.path" || (key.starts_with("includeif.") && key.ends_with(".path"));
        let value = String::from_utf8_lossy(value);
        if !is_include || value.is_empty() {
            continue;
        }
        let dir = origin_file.parent().unwrap_or(Path::new("/"));
        let target = if let Some(rest) = value.strip_prefix("~/") {
            match std::env::var_os("HOME") {
                Some(home) => absolute(Path::new(&home), Path::new(rest)),
                None => continue,
            }
        } else {
            absolute(dir, Path::new(value.as_ref()))
        };
        queue.push((target, depth));
    }
}

fn absolute(base: &Path, path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    // Lexically, so a target that does not exist yet still compares equal
    // to the path a sandbox rule names: `.` dropped, `..` pops a name.
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// A hardened `git` run with the fixed overrides: its output on success,
/// `None` on exit status 1 (nothing found), an error otherwise.
fn run(inv: &super::Invocation<'_>, args: &[&str]) -> Result<Option<Vec<u8>>, HostGitError> {
    super::check_version(inv.exe)?;
    let mut cmd = base(inv);
    set_overrides(&mut cmd, &fixed_overrides());
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let out = cmd.output().map_err(|source| HostGitError::Spawn {
        exe: inv.exe.display().to_string(),
        source,
    })?;
    match out.status.code() {
        Some(0) => Ok(Some(out.stdout)),
        Some(1) if out.stderr.is_empty() => Ok(None),
        code => Err(HostGitError::Config {
            code,
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        }),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env_remove("GIT_DIR")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Includes are followed whatever their condition, relative ones from
    /// the including file's directory, nested ones too, and a missing target
    /// is still listed; an unchanged repository is answered from the cache,
    /// a changed include list is read again.
    #[test]
    fn every_include_target_is_found_and_cached_until_a_file_changes() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = crate::plain_path::canonicalize(tmp.path()).expect("canonical");
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).expect("repo");
        git(&repo, &["init", "-q", "-b", "main"]);
        let common = repo.join(".git");
        git(&repo, &["config", "include.path", "local.cfg"]);
        git(
            &repo,
            &[
                "config",
                "includeIf.onbranch:nowhere.path",
                "../tree/never-matched.cfg",
            ],
        );
        std::fs::write(
            common.join("local.cfg"),
            "[include]\n\tpath = nested/missing.cfg\n",
        )
        .expect("local");
        let git_exe = Path::new("git");
        let mut scan = IncludeScan::new();
        let files = scan.files(git_exe, &common).expect("scan");
        for want in [
            common.join("config"),
            common.join("local.cfg"),
            common.join("nested/missing.cfg"),
            repo.join("tree/never-matched.cfg"),
        ] {
            assert!(
                files.contains(&want),
                "missing {} in {files:#?}",
                want.display()
            );
        }
        // Just written, so not settled: the next call scans again rather
        // than trust a stamp a coarse clock may not move.
        assert!(scan.last.as_ref().is_some_and(|l| !l.settled));
        let now = SystemTime::now();
        assert!(!settled(&stamp(&common.join("config")), now));
        assert!(settled(&stamp(&common.join("config")), now + RACY * 2));
        assert!(settled(&None, now));
        std::fs::write(common.join("local.cfg"), "[include]\n\tpath = other.cfg\n").expect("edit");
        let files = scan.files(git_exe, &common).expect("rescan");
        assert!(files.contains(&common.join("other.cfg")), "{files:#?}");
        assert!(!files.contains(&common.join("nested/missing.cfg")));
    }
}

//! Persisted allow rules: "always allow this command pattern for this
//! project."
//!
//! Ported from goose `permission/permission_store.rs`
//! (`third_party/goose/UPSTREAM`): one JSON file, loaded whole, saved by
//! writing a temp file and renaming it over the target so a crash mid-write
//! never corrupts the store. Restructured for a **pattern** match (tool plus a
//! [`Scope`]) rather than goose's exact-argument-hash match, because the task
//! this store exists for is "always allow this command pattern," not "always
//! allow this exact call again."
//!
//! **Applied as a hook, not a bespoke check.** [`PersistedAllowHook`]
//! implements [`kgraf_tools::approval::ApprovalHook`], so a persisted rule
//! composes with the rest of a worker's [`kgraf_tools::approval::ApprovalChain`]
//! (0055) instead of being a second gate a caller has to remember to consult.
//!
//! **The subject comes from the tool, never from a guess here**. A rule's [`Scope::Prefix`] is checked against `Tool::permission_subject`
//! -- the same call [`crate::skills::ToolPolicy`] uses -- which reports `None`
//! for anything it has not parsed as one simple command, and the check itself
//! uses [`crate::skills::word_boundary_prefix`], the identical word-boundary
//! rule `allowed-tools`' `Name(prefix:*)` form uses. A hook that instead read
//! `input["cmd"]` and matched it with `starts_with` would let a `"git "` rule
//! cover `"git status; rm -rf ~"` (a compound command a real shell tool must
//! refuse to name a subject for at all) and would let `"git"` match `"gitk"`.
//!
//! **`kgraf_tools::ShellTool` now implements `permission_subject`**,
//! returning the canonical JSON of a
//! plain argv, `None` for anything compound or wrapped. That closed the gap
//! this comment used to describe (a `Prefix` rule matching nothing at all for
//! a real tool), but opened a narrower one: `ShellTool`'s subject is a full
//! command line, not a hierarchical name, so *no* `Scope::Prefix` over it is
//! safe (`sed -Ei`, `dash -c`, `git --git-dir`, ... all defeat a name-prefix
//! list; see [`Scope::Prefix`]'s doc comment). [`allow_prefix`](PersistedAllowStore::allow_prefix)
//! refuses to construct one for [`PREFIX_FORBIDDEN_TOOLS`]; [`Scope::Exact`]
//! (via [`allow_exact`](PersistedAllowStore::allow_exact)) is what such a tool
//! supports. The `Shell` test double in this module's tests still stands in
//! for a *simpler*, hypothetical `Prefix`-eligible tool, to exercise the
//! generic word-boundary codepath independent of `ShellTool`'s specifics.
//!
//! **Requires a local filesystem**: [`FileLock`]'s `flock` and
//! [`check_store_file_permissions`]'s ownership/mode check are both
//! meaningless over NFS, where `flock` can silently fail to exclude a second
//! host and ownership bits do not necessarily mean what they mean locally
//! (`root_squash`, differing uid maps). [`PersistedAllowStore::open`] refuses
//! a store path on a filesystem it can identify as a network one; on a
//! platform or filesystem it cannot identify, it proceeds, so this is a
//! best-effort defense, not a guarantee -- the requirement itself is that the
//! store's directory is on local storage, stated here because not every
//! platform lets the code check it.

use crate::SessionError;
use crate::skills::word_boundary_prefix;
use kgraf_tools::ToolRegistry;
use kgraf_tools::approval::{ApprovalHook, HookDecision, PendingCall};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// The current on-disk schema version. [`PersistedAllowStore::open`] refuses a
/// file recorded under any other version rather than guess
/// its shape: a future, incompatible schema read as if it were this one could
/// silently mismatch fields.
const CURRENT_VERSION: u32 = 1;

/// What a rule matches, besides the tool and project.
///
/// **No implicit "match everything."** There is no `None`
/// variant and no `Default`: a caller reaching for "no restriction" must
/// explicitly choose [`Scope::AnyCall`] through [`PersistedAllowStore::allow_any_call`],
/// which additionally requires a [`ConfirmAllowAnyCall`] marker. `serde` never
/// invents a scope either: both [`AllowRule`] and the store file itself
/// (`StoreFile`) are `#[serde(deny_unknown_fields)]`, `scope` is a required
/// field with no default, and it is adjacently tagged (`{"kind": ..., "value":
/// ...}`) so a store written before this fix, whose rules carried a
/// `subject_prefix: Option<String>` instead, fails to parse loudly rather than
/// a missing `scope` silently becoming [`Scope::AnyCall`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Scope {
    /// Matches every call to the tool, regardless of subject. Only
    /// constructible via [`PersistedAllowStore::allow_any_call`].
    AnyCall,
    /// Matches a call whose subject equals `prefix`, or starts with `prefix`
    /// followed by whitespace ([`word_boundary_prefix`]). Never empty:
    /// [`PersistedAllowStore::allow_prefix`] refuses to construct one from an
    /// empty string, because an empty prefix matches every subject and that
    /// is what [`Scope::AnyCall`] is for, explicitly.
    ///
    /// **Never for a tool whose subject is a full command line, not a
    /// hierarchical name**:
    /// [`PersistedAllowStore::allow_prefix`] refuses to construct one for
    /// [`PREFIX_FORBIDDEN_TOOLS`]. `sed -Ei`, `dash -c`, `perl -e`,
    /// `busybox`, `timeout`, `nice`, `git --git-dir`, `git -C`, `git grep -O`
    /// and `find -fprint` all defeat a name-prefix allow-list without ever
    /// tripping a naive check, because a prefix's whole premise -- "the
    /// matched leading tokens are what runs" -- is false once a program's own
    /// trailing flags redirect, wrap or reinterpret its invocation. Use
    /// [`Scope::Exact`] instead: it can only ever match a byte-identical
    /// repeat of one specific, previously-seen command.
    Prefix(String),
    /// Matches a call whose subject equals `exact`, exactly, character for
    /// character. Where [`Tool::permission_subject`](kgraf_tools::Tool::permission_subject)
    /// returns the canonical JSON of a full argv (as [`kgraf_tools::ShellTool`]
    /// does), `exact` is that canonical JSON, so this can never match a
    /// different invocation that merely starts the same way. Never empty,
    /// for the same reason [`Scope::Prefix`] is not.
    Exact(String),
}

/// A deliberate, data-free marker required to create a [`Scope::AnyCall`]
/// rule. "Allow every call to this tool" can then never be
/// reached through a default, a missing field, or an accidentally empty
/// prefix -- constructing this value at all is the explicit confirmation the
/// caller (ultimately, code acting on the user's own "yes, always" choice)
/// must make.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConfirmAllowAnyCall;

/// One persisted "always allow" rule.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AllowRule {
    /// The tool's wire name, matched exactly.
    pub tool: String,
    pub scope: Scope,
    /// The project this rule applies to (a path or a stable project key).
    pub project: String,
    /// Epoch milliseconds when the rule was recorded.
    pub created_at_ms: i64,
    /// Why the rule was added, or who added it. Required so the store stays
    /// auditable, not just a bag of silent exceptions.
    pub note: String,
}

impl AllowRule {
    fn matches(&self, project: &str, tool: &str, subject: Option<&str>) -> bool {
        if self.project != project || self.tool != tool {
            return false;
        }
        match &self.scope {
            Scope::AnyCall => true,
            Scope::Prefix(prefix) => subject.is_some_and(|s| word_boundary_prefix(s, prefix)),
            Scope::Exact(exact) => subject == Some(exact.as_str()),
        }
    }
}

/// Tool wire names for which [`Scope::Prefix`] is never safe: their subject is
/// a full command line, not a name space with meaningful hierarchy, so no
/// prefix of it reliably names "what will run" (see [`Scope::Prefix`]'s doc comment for the concrete bypasses).
/// [`kgraf_tools::ShellTool::name`] is `"shell"`.
const PREFIX_FORBIDDEN_TOOLS: &[&str] = &[SHELL_TOOL];

/// [`kgraf_tools::ShellTool::name`].
const SHELL_TOOL: &str = "shell";

/// The persisted rules for one project, backed by one JSON file.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoreFile {
    version: u32,
    rules: Vec<AllowRule>,
}

/// What [`PersistedAllowStore::open`] found, so a caller can log or display it
/// rather than the rule count being invisible.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpenReport {
    pub rule_count: usize,
    /// Rules that can no longer match any call
    /// ([`PersistedAllowStore::inert_rules`]). A caller shows them, so a
    /// command the user once allowed always is not asked about again
    /// without a word.
    pub inert_rule_count: usize,
}

/// A loaded, mutable, on-disk allow-rule store.
#[derive(Debug)]
pub struct PersistedAllowStore {
    path: PathBuf,
    file: StoreFile,
}

impl PersistedAllowStore {
    /// Load the store at `path`, or start empty if it does not exist yet.
    ///
    /// `path` must not be inside `workspace_root`: a worker
    /// with write access to its own worktree must not also be able to write
    /// the file that decides what it may run unasked. Every project's
    /// worktree is a writable sandbox root (disjoint writes), so any of them
    /// containing the store would let a compromised or careless worker grant
    /// itself permissions.
    pub fn open(
        path: impl Into<PathBuf>,
        workspace_root: &Path,
    ) -> Result<(Self, OpenReport), SessionError> {
        let path = path.into();
        if is_inside(&path, workspace_root)? {
            return Err(SessionError::ApprovalStore {
                path: path.clone(),
                reason: format!(
                    "the allow-rule store may not live inside the writable workspace root {}; \
                     a worker could read or rewrite its own approvals",
                    workspace_root.display()
                ),
            });
        }
        refuse_network_filesystem(&path)?;
        let file = Self::read_store_file(&path)?;
        let store = Self { path, file };
        let report = OpenReport {
            rule_count: store.file.rules.len(),
            inert_rule_count: store.inert_rules().len(),
        };
        Ok((store, report))
    }

    /// Re-read the rules from disk, so a rule another process revoked (or
    /// added) is seen by this handle. On an error the handle keeps its rules
    /// and the error is returned; a caller that gates on the rules should treat
    /// it as "no rule covers this".
    pub fn reload(&mut self) -> Result<(), SessionError> {
        self.file = Self::read_store_file(&self.path)?;
        Ok(())
    }

    /// The `shell` rules that name a command no rule may name any more, so
    /// they can never match: [`kgraf_tools::ShellTool`] reports a subject
    /// only for an argv [`kgraf_tools::shell::exact_rule_allowable`] accepts,
    /// an allowlist of programs and argument shapes. A rule recorded under an
    /// earlier, wider check (an interpreter given program text, such as
    /// `python3 -c`, or a wrapper) stays in the file, never deleted without
    /// the user's say, and is reported here instead; that command is asked
    /// about each time. A subject that is not a JSON argv at all can never
    /// match either, and is reported too.
    pub fn inert_rules(&self) -> Vec<&AllowRule> {
        self.file
            .rules
            .iter()
            .filter(|rule| rule.tool == SHELL_TOOL)
            .filter(|rule| match &rule.scope {
                Scope::AnyCall => false,
                Scope::Prefix(subject) | Scope::Exact(subject) => {
                    serde_json::from_str::<Vec<String>>(subject).map_or(true, |argv| {
                        !kgraf_tools::shell::exact_rule_allowable(&argv)
                    })
                }
            })
            .collect()
    }

    /// Every rule currently in the store, for display or audit.
    pub fn rules(&self) -> &[AllowRule] {
        &self.file.rules
    }

    /// Whether `tool`/`subject` in `project` is covered by a persisted rule,
    /// and which one, so a caller can show the user why a call ran unasked.
    pub fn matching_rule(
        &self,
        project: &str,
        tool: &str,
        subject: Option<&str>,
    ) -> Option<&AllowRule> {
        self.file
            .rules
            .iter()
            .find(|rule| rule.matches(project, tool, subject))
    }

    /// Persist a rule that matches calls whose subject starts with `prefix`
    /// (on a word boundary; see [`word_boundary_prefix`]). Refuses an empty
    /// `prefix`: use [`PersistedAllowStore::allow_any_call`]
    /// to allow every call explicitly instead.
    pub fn allow_prefix(
        &mut self,
        project: impl Into<String>,
        tool: impl Into<String>,
        prefix: impl Into<String>,
        note: impl Into<String>,
    ) -> Result<&AllowRule, SessionError> {
        let tool = tool.into();
        if PREFIX_FORBIDDEN_TOOLS.contains(&tool.as_str()) {
            return Err(SessionError::ApprovalStore {
                path: self.path.clone(),
                reason: format!(
                    "{tool} cannot take a Scope::Prefix rule: its subject is a full command \
                     line, not a hierarchical name, so no prefix of it reliably names what \
                     will run; call allow_exact to allow one specific, canonical command instead"
                ),
            });
        }
        let prefix = prefix.into();
        if prefix.is_empty() {
            return Err(SessionError::ApprovalStore {
                path: self.path.clone(),
                reason: "an empty subject prefix would match every call to this tool; \
                         call allow_any_call to permit that explicitly"
                    .into(),
            });
        }
        self.insert(AllowRule {
            tool,
            scope: Scope::Prefix(prefix),
            project: project.into(),
            created_at_ms: now_ms(),
            note: note.into(),
        })
    }

    /// Persist a rule that matches only a call whose subject equals `exact`,
    /// character for character. The
    /// only scope [`allow_prefix`](Self::allow_prefix) permits for
    /// [`PREFIX_FORBIDDEN_TOOLS`] such as `shell`: `exact` should be the
    /// canonical JSON [`kgraf_tools::Tool::permission_subject`] returned
    /// for the specific call being allowed, taken from the pending call the
    /// caller is approving, never hand-typed.
    pub fn allow_exact(
        &mut self,
        project: impl Into<String>,
        tool: impl Into<String>,
        exact: impl Into<String>,
        note: impl Into<String>,
    ) -> Result<&AllowRule, SessionError> {
        let exact = exact.into();
        if exact.is_empty() {
            return Err(SessionError::ApprovalStore {
                path: self.path.clone(),
                reason: "an empty exact subject would match a call whose tool reports no \
                         subject at all for a different, unrelated reason; refusing"
                    .into(),
            });
        }
        self.insert(AllowRule {
            tool: tool.into(),
            scope: Scope::Exact(exact),
            project: project.into(),
            created_at_ms: now_ms(),
            note: note.into(),
        })
    }

    /// Persist a rule that matches every call to `tool`, regardless of
    /// subject. Requires [`ConfirmAllowAnyCall`]: the marker
    /// carries no information, so passing it is itself the caller's
    /// deliberate confirmation, never a default that falls out of an absent
    /// prefix.
    pub fn allow_any_call(
        &mut self,
        project: impl Into<String>,
        tool: impl Into<String>,
        note: impl Into<String>,
        _confirm: ConfirmAllowAnyCall,
    ) -> Result<&AllowRule, SessionError> {
        self.insert(AllowRule {
            tool: tool.into(),
            scope: Scope::AnyCall,
            project: project.into(),
            created_at_ms: now_ms(),
            note: note.into(),
        })
    }

    /// Remove every any-call rule for `tool` in `project`, persisted at once
    /// under the same lock as [`Self::insert`]. Returns how many were removed.
    pub fn revoke_any_call(&mut self, project: &str, tool: &str) -> Result<usize, SessionError> {
        let _lock = FileLock::acquire(&self.path)?;
        let mut fresh = Self::read_store_file(&self.path)?;
        let before = fresh.rules.len();
        fresh
            .rules
            .retain(|r| !(r.project == project && r.tool == tool && r.scope == Scope::AnyCall));
        let removed = before - fresh.rules.len();
        if removed > 0 {
            Self::write_store_file(&self.path, &fresh)?;
        }
        self.file = fresh;
        Ok(removed)
    }

    /// Add a rule and persist it at once, under an exclusive lock across the
    /// whole read-modify-write: another process (or another
    /// call on another `PersistedAllowStore` handle to the same path) cannot
    /// read the same on-disk state and race a write that clobbers this rule,
    /// because the file is re-read fresh only after the lock is held.
    ///
    /// The in-memory store only changes if the write to disk succeeds, so a
    /// failed save never leaves the running process believing a rule exists
    /// that a restart would not find.
    fn insert(&mut self, rule: AllowRule) -> Result<&AllowRule, SessionError> {
        let _lock = FileLock::acquire(&self.path)?;
        let mut fresh = Self::read_store_file(&self.path)?;
        fresh.rules.push(rule);
        Self::write_store_file(&self.path, &fresh)?;
        self.file = fresh;
        self.file.rules.last().ok_or_else(|| SessionError::ApprovalStore {
            path: self.path.clone(),
            reason: "internal: the rule just inserted is missing from the file that was just written"
                .into(),
        })
    }

    fn read_store_file(path: &Path) -> Result<StoreFile, SessionError> {
        match std::fs::metadata(path) {
            Ok(meta) => check_store_file_permissions(path, &meta)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(SessionError::ApprovalStore {
                    path: path.to_path_buf(),
                    reason: e.to_string(),
                });
            }
        }
        match std::fs::read(path) {
            Ok(bytes) => {
                let file: StoreFile =
                    serde_json::from_slice(&bytes).map_err(|e| SessionError::ApprovalStore {
                        path: path.to_path_buf(),
                        reason: format!("could not parse the store: {e}"),
                    })?;
                if file.version != CURRENT_VERSION {
                    return Err(SessionError::ApprovalStore {
                        path: path.to_path_buf(),
                        reason: format!(
                            "unknown store version {} (this code understands version {CURRENT_VERSION}); \
                             refusing rather than guess its shape",
                            file.version
                        ),
                    });
                }
                Ok(file)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(StoreFile {
                version: CURRENT_VERSION,
                rules: Vec::new(),
            }),
            Err(e) => Err(SessionError::ApprovalStore {
                path: path.to_path_buf(),
                reason: e.to_string(),
            }),
        }
    }

    /// Write `file` to `path`, atomically: a uniquely-named temp file in the
    /// same directory, fsynced, then renamed over the target (a fixed temp name would let two concurrent writers' temp files
    /// collide). The temp file -- and so the file that ends up at `path` -- is
    /// created mode 0600: this store's content is itself a
    /// capability list, not a file a mode of 0644 belongs on.
    fn write_store_file(path: &Path, file: &StoreFile) -> Result<(), SessionError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|e| SessionError::ApprovalStore {
                path: path.to_path_buf(),
                reason: e.to_string(),
            })?;
        }
        let content =
            serde_json::to_string_pretty(file).map_err(|e| SessionError::ApprovalStore {
                path: path.to_path_buf(),
                reason: e.to_string(),
            })?;
        let unique_name = format!(
            "{}.tmp.{}.{}",
            path.file_name().and_then(|n| n.to_str()).unwrap_or("store"),
            std::process::id(),
            now_ms(),
        );
        let temp_path = path.with_file_name(unique_name);
        write_new_file_mode_0600(&temp_path, content.as_bytes()).map_err(|e| {
            SessionError::ApprovalStore {
                path: path.to_path_buf(),
                reason: e.to_string(),
            }
        })?;
        std::fs::rename(&temp_path, path).map_err(|e| SessionError::ApprovalStore {
            path: path.to_path_buf(),
            reason: e.to_string(),
        })?;
        // Best effort: fsync the directory entry for the rename itself. Not
        // every filesystem or sandbox supports opening a directory this way;
        // the file's own content is already fsynced above, so a failure here
        // narrows only to "the rename's metadata might not survive a crash
        // that lands in the instant after this call," not to data loss.
        #[cfg(unix)]
        if let Some(parent) = path.parent() {
            let dir = if parent.as_os_str().is_empty() {
                Path::new(".")
            } else {
                parent
            };
            let _ = std::fs::File::open(dir).and_then(|f| f.sync_all());
        }
        Ok(())
    }
}

/// Write `bytes` to a brand-new file at `path` (fails if it already exists,
/// so two callers racing on the same unique name never silently clobber one
/// another), mode 0600 on Unix, fsynced before returning.
fn write_new_file_mode_0600(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    #[cfg(unix)]
    let mut file = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?
    };
    #[cfg(not(unix))]
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// An exclusive lock over a store's read-modify-write, held
/// via a sidecar `<store>.lock` file so the lock's identity survives the
/// store file itself being replaced by a rename mid-hold by some other actor.
/// A no-op on non-Unix targets: `flock` has no portable equivalent here, and
/// kGraf does not yet run multi-process on those targets.
struct FileLock {
    #[cfg(unix)]
    file: std::fs::File,
}

impl FileLock {
    fn acquire(store_path: &Path) -> Result<Self, SessionError> {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            use std::os::unix::fs::OpenOptionsExt;
            let lock_path = store_path.with_extension("lock");
            if let Some(parent) = lock_path.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent).map_err(|e| SessionError::ApprovalStore {
                    path: lock_path.clone(),
                    reason: e.to_string(),
                })?;
            }
            let file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .mode(0o600)
                .open(&lock_path)
                .map_err(|e| SessionError::ApprovalStore {
                    path: lock_path.clone(),
                    reason: e.to_string(),
                })?;
            // SAFETY: `file`'s fd is valid and owned exclusively by this
            // `FileLock` for the syscall's duration. `flock` with `LOCK_EX`
            // blocks this thread until the lock is free rather than failing
            // spuriously, so a non-zero return is a genuine error.
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
            if rc != 0 {
                return Err(SessionError::ApprovalStore {
                    path: lock_path,
                    reason: std::io::Error::last_os_error().to_string(),
                });
            }
            Ok(Self { file })
        }
        #[cfg(not(unix))]
        {
            let _ = store_path;
            Ok(Self {})
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            // SAFETY: releasing a lock this struct itself holds.
            unsafe {
                libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
            }
        }
    }
}

/// Whether `candidate` is inside `root`: a purely
/// lexical comparison is not enough. `root` and the *nearest existing
/// ancestor* of `candidate` (the store file itself usually does not exist
/// yet) are each `canonicalize`d -- resolving symlinks and, on the
/// filesystems that fold names, the on-disk case -- before comparing, so a
/// symlink planted inside the workspace pointing elsewhere, a `/var` vs
/// `/private/var` alias, or an upper/lower-case spelling of the same APFS
/// path cannot slip past a textual check.
///
/// The comparison itself is run both case-sensitively and case-folded, and
/// refuses if *either* matches, rather than trying to detect the real
/// filesystem's case sensitivity: a false refusal just means "pick a
/// different path for the store," while a missed case-insensitive alias is
/// the vulnerability this exists to close.
fn is_inside(candidate: &Path, root: &Path) -> Result<bool, SessionError> {
    let root_canon = std::fs::canonicalize(root).map_err(|e| SessionError::ApprovalStore {
        path: root.to_path_buf(),
        reason: format!("could not canonicalize the workspace root: {e}"),
    })?;
    let absolute_candidate = lexically_normalize(candidate);
    let (existing, tail) = nearest_existing_ancestor_and_tail(&absolute_candidate);
    let existing_canon =
        std::fs::canonicalize(&existing).map_err(|e| SessionError::ApprovalStore {
            path: existing.clone(),
            reason: format!("could not canonicalize {}: {e}", existing.display()),
        })?;
    let candidate_canon = existing_canon.join(tail);

    let fold = |p: &Path| p.to_string_lossy().to_lowercase();
    Ok(candidate_canon.starts_with(&root_canon)
        || fold(&candidate_canon).starts_with(&fold(&root_canon)))
}

/// Absolutize `path` against the current directory (if relative) and collapse
/// `.`/`..` components lexically, without touching the filesystem.
fn lexically_normalize(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut out = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Walk up from `absolute` (already lexically normalized: no `.`/`..` left)
/// to the nearest ancestor that exists on disk, returning it along with the
/// non-existent tail below it (so the caller can rejoin the tail onto the
/// ancestor's canonicalized form).
fn nearest_existing_ancestor_and_tail(absolute: &Path) -> (PathBuf, PathBuf) {
    let mut existing = absolute.to_path_buf();
    let mut tail_components: Vec<std::ffi::OsString> = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name().map(std::ffi::OsStr::to_os_string) else {
            break;
        };
        tail_components.push(name);
        if !existing.pop() {
            break;
        }
    }
    tail_components.reverse();
    (existing, tail_components.into_iter().collect())
}

/// Refuse a store file that is group- or world-writable, or not owned by the
/// current user: either means a rule in it -- including a
/// hand-planted [`Scope::AnyCall`] -- was not necessarily written by this
/// user's own `allow_*` calls.
#[cfg(unix)]
fn check_store_file_permissions(path: &Path, meta: &std::fs::Metadata) -> Result<(), SessionError> {
    use std::os::unix::fs::MetadataExt;
    let mode = meta.mode() & 0o777;
    if mode & 0o022 != 0 {
        return Err(SessionError::ApprovalStore {
            path: path.to_path_buf(),
            reason: format!(
                "the store file is group- or world-writable (mode {mode:o}); \
                 refusing to trust its rules"
            ),
        });
    }
    let owner = meta.uid();
    // SAFETY: `getuid` takes no arguments and cannot fail.
    let current = unsafe { libc::getuid() };
    if owner != current {
        return Err(SessionError::ApprovalStore {
            path: path.to_path_buf(),
            reason: format!(
                "the store file is owned by uid {owner}, not the current user (uid {current}); \
                 refusing to trust its rules"
            ),
        });
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_store_file_permissions(
    _path: &Path,
    _meta: &std::fs::Metadata,
) -> Result<(), SessionError> {
    Ok(())
}

/// Refuse a store path on a network filesystem, where this
/// module's `flock` and ownership checks are not trustworthy. Best effort:
/// an unrecognized or undetectable filesystem type is let through, since
/// over-refusing on an exotic local filesystem would be its own defect.
#[cfg(target_os = "macos")]
fn refuse_network_filesystem(path: &Path) -> Result<(), SessionError> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let absolute = lexically_normalize(path);
    let (existing, _tail) = nearest_existing_ancestor_and_tail(&absolute);
    let Ok(c_path) = CString::new(existing.as_os_str().as_bytes()) else {
        return Ok(()); // an interior NUL cannot name a real path; unrelated to this check
    };
    // SAFETY: `stat_buf` is zero-initialized and only read after `statfs`
    // reports success, at which point it has written every field.
    let mut stat_buf: libc::statfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statfs(c_path.as_ptr(), &mut stat_buf) };
    if rc != 0 {
        return Ok(()); // could not stat `existing`; do not block on an unrelated failure
    }
    let name_bytes: Vec<u8> = stat_buf
        .f_fstypename
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    let name = String::from_utf8_lossy(&name_bytes).to_lowercase();
    const NETWORK_FILESYSTEMS: &[&str] = &["nfs", "smbfs", "afpfs", "webdav", "cifs", "ftp"];
    if NETWORK_FILESYSTEMS.contains(&name.as_str()) {
        return Err(SessionError::ApprovalStore {
            path: path.to_path_buf(),
            reason: format!(
                "the store's filesystem ({name}) is a network filesystem; flock and ownership \
                 checks are not trustworthy there"
            ),
        });
    }
    Ok(())
}

/// Linux equivalent of the macOS check above, keyed on `f_type`'s magic
/// number instead of a name string. Unverified in this codebase's own test
/// environment (macOS); the magic numbers are the well-known constants from
/// `statfs(2)`.
#[cfg(target_os = "linux")]
fn refuse_network_filesystem(path: &Path) -> Result<(), SessionError> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    const NFS_SUPER_MAGIC: i64 = 0x6969;
    const SMB_SUPER_MAGIC: i64 = 0x517b;
    const CIFS_MAGIC_NUMBER: i64 = 0xff53_4d42_u32 as i64;
    const CODA_SUPER_MAGIC: i64 = 0x7375_7245;
    const NCP_SUPER_MAGIC: i64 = 0x564c;
    const AFS_SUPER_MAGIC: i64 = 0x5346_414f;

    let absolute = lexically_normalize(path);
    let (existing, _tail) = nearest_existing_ancestor_and_tail(&absolute);
    let Ok(c_path) = CString::new(existing.as_os_str().as_bytes()) else {
        return Ok(());
    };
    // SAFETY: as the macOS variant above.
    let mut stat_buf: libc::statfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statfs(c_path.as_ptr(), &mut stat_buf) };
    if rc != 0 {
        return Ok(());
    }
    // `f_type` is `i64` on glibc (aarch64 and x86_64) but a narrower type on other
    // libcs, so the conversion is kept and the lint that fires on glibc is allowed.
    #[allow(clippy::useless_conversion)]
    let f_type = i64::from(stat_buf.f_type);
    if [
        NFS_SUPER_MAGIC,
        SMB_SUPER_MAGIC,
        CIFS_MAGIC_NUMBER,
        CODA_SUPER_MAGIC,
        NCP_SUPER_MAGIC,
        AFS_SUPER_MAGIC,
    ]
    .contains(&f_type)
    {
        return Err(SessionError::ApprovalStore {
            path: path.to_path_buf(),
            reason: format!(
                "the store's filesystem (f_type {f_type:#x}) looks like a network filesystem; \
                 flock and ownership checks are not trustworthy there"
            ),
        });
    }
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn refuse_network_filesystem(_path: &Path) -> Result<(), SessionError> {
    Ok(())
}

/// An [`ApprovalHook`] that allows a call when a persisted rule for this
/// project covers it, and otherwise defers to the rest of the chain.
///
/// Read-only by design: adding a rule is a distinct, explicit action (a user
/// choosing "always allow" on an ask), not something a hook does to itself
/// mid-review.
///
/// **Current, not a copy from start-up.** Each review compares the store
/// file's identity (modification time, length, inode) with the one last
/// read, and reads the file again when it changed: a rule revoked while a
/// run is live stops allowing calls from the next review on. Writers
/// replace the file with a rename, so a read never sees half a write. A file
/// that cannot be read again allows nothing: the call is asked instead.
pub struct PersistedAllowHook {
    store: std::sync::Mutex<HookSnapshot>,
    project: String,
    /// Resolves a call's subject the same way the rest of the approval chain
    /// does: `Tool::permission_subject`, never a guess this
    /// hook makes about which input field holds a command string.
    registry: Arc<ToolRegistry>,
}

/// The store as the hook last read it, and the file identity it read.
struct HookSnapshot {
    store: PersistedAllowStore,
    /// `None` until the first review checks the file; `Some(None)` when the
    /// file did not exist then.
    stamp: Option<Option<FileStamp>>,
}

/// What tells one version of the store file from the next. Every reader of the
/// rules file (this hook, the orchestrator's ask desk) decides "unchanged" with
/// [`stamp_unchanged`], so they cannot disagree about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileStamp {
    modified: Option<SystemTime>,
    len: u64,
    inode: u64,
    /// The file changed within [`RACY_STAMP_WINDOW`] of this check. Two
    /// writes in one clock tick (a coarse filesystem timestamp) can leave the
    /// same time and length, and a freed inode can be reused by the next
    /// rename, so a stamp this young does not prove the file is unchanged.
    recent: bool,
}

/// How old a modification time must be before the stamp is trusted.
const RACY_STAMP_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);

/// Whether a read taken when the file had the stamp `read_at` still describes the
/// file, whose stamp is `now`. `read_at` is `None` before any read. A stamp of a
/// file that changed recently is never trusted: the same length, time and inode do
/// not prove the same bytes.
pub fn stamp_unchanged(read_at: Option<&Option<FileStamp>>, now: &Option<FileStamp>) -> bool {
    read_at == Some(now) && !now.as_ref().is_some_and(|s| s.recent)
}

/// The stamp of the file at `path`: `None` when it does not exist, an error when
/// it cannot be examined.
pub fn file_stamp(path: &Path) -> Result<Option<FileStamp>, SessionError> {
    match std::fs::metadata(path) {
        Ok(meta) => {
            #[cfg(unix)]
            let inode = std::os::unix::fs::MetadataExt::ino(&meta);
            #[cfg(not(unix))]
            let inode = 0;
            let modified = meta.modified().ok();
            // No readable time, or one in the future: not trusted.
            let recent = modified
                .and_then(|m| SystemTime::now().duration_since(m).ok())
                .is_none_or(|age| age < RACY_STAMP_WINDOW);
            Ok(Some(FileStamp {
                modified,
                len: meta.len(),
                inode,
                recent,
            }))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(SessionError::ApprovalStore {
            path: path.to_path_buf(),
            reason: e.to_string(),
        }),
    }
}

impl PersistedAllowHook {
    pub fn new(
        store: PersistedAllowStore,
        project: impl Into<String>,
        registry: Arc<ToolRegistry>,
    ) -> Self {
        Self {
            store: std::sync::Mutex::new(HookSnapshot { store, stamp: None }),
            project: project.into(),
            registry,
        }
    }

    /// The rule that would cover `tool`/`subject`, if any, for display, from
    /// the store as it is on disk now.
    pub fn matching_rule(
        &self,
        tool: &str,
        subject: Option<&str>,
    ) -> Result<Option<AllowRule>, SessionError> {
        let snapshot = self.current()?;
        Ok(snapshot
            .store
            .matching_rule(&self.project, tool, subject)
            .cloned())
    }

    /// The snapshot, read again first when the file changed since the last
    /// read.
    fn current(&self) -> Result<std::sync::MutexGuard<'_, HookSnapshot>, SessionError> {
        let mut snapshot = self.store.lock().unwrap_or_else(|p| p.into_inner());
        let now = file_stamp(&snapshot.store.path)?;
        if !stamp_unchanged(snapshot.stamp.as_ref(), &now) {
            // A missing file reads as no rules.
            snapshot.store.file = PersistedAllowStore::read_store_file(&snapshot.store.path)?;
            snapshot.stamp = Some(now);
        }
        Ok(snapshot)
    }

    fn subject_of(&self, call: &PendingCall) -> Option<String> {
        self.registry
            .get(&call.tool)?
            .tool()
            .permission_subject(&call.input)
    }
}

#[async_trait::async_trait]
impl ApprovalHook for PersistedAllowHook {
    fn name(&self) -> &str {
        "persisted-allow-rules"
    }

    async fn review(&self, call: &PendingCall) -> HookDecision {
        let subject = self.subject_of(call);
        match self.matching_rule(&call.tool, subject.as_deref()) {
            Ok(Some(_)) => HookDecision::Allow,
            Err(e) => HookDecision::Ask {
                question: format!(
                    "`{}`: the persisted allow rules could not be read again ({e}), so none \
                     applies",
                    call.tool
                ),
            },
            Ok(None) => HookDecision::Ask {
                question: format!(
                    "`{}` is not covered by a persisted allow rule for this project",
                    call.tool
                ),
            },
        }
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use kgraf_core::{AgentId, TaskId};
    use kgraf_tools::approval::{ApprovalChain, MemoryApprovalLog, Verdict};
    use kgraf_tools::{
        Access, CallPermit, DiscoveredTools, Exposure, Tool, ToolContext, ToolDefinition,
        ToolError, ToolOutput, ToolView,
    };
    use serde_json::{Value, json};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::tempdir;

    /// A shell-like test tool whose `permission_subject` is a minimal, but
    /// real, simple-command parser: it refuses to name a subject for anything
    /// that is not a single simple command, exactly the contract
    /// `crate::skills`'s module doc states a shell tool must follow. This is
    /// what the tests exercise: the *hook* never sees a subject
    /// for a compound command, because the tool itself refuses to report one.
    struct Shell {
        calls: Arc<AtomicUsize>,
    }

    impl Shell {
        /// Characters or sequences that make a command compound rather than
        /// simple: a command separator, a pipe, or a subshell.
        const COMPOUND_MARKERS: [&'static str; 5] = [";", "&&", "|", "$(", ")"];

        fn simple_command_subject(input: &Value) -> Option<String> {
            let cmd = input.get("cmd").and_then(Value::as_str)?;
            if Self::COMPOUND_MARKERS.iter().any(|m| cmd.contains(m)) {
                return None;
            }
            Some(cmd.to_owned())
        }
    }

    #[async_trait::async_trait]
    impl Tool for Shell {
        fn name(&self) -> &str {
            "test-command-tool"
        }
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "test-command-tool".into(),
                description: "d".into(),
                input_schema: json!({"type": "object"}),
            }
        }
        fn access(&self, _input: &Value) -> Result<Access, ToolError> {
            Ok(Access::none())
        }
        fn permission_subject(&self, input: &Value) -> Option<String> {
            Self::simple_command_subject(input)
        }
        async fn call(
            &self,
            _input: Value,
            _ctx: &ToolContext,
            _permit: &CallPermit,
        ) -> Result<ToolOutput, ToolError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ToolOutput::text("ran"))
        }
    }

    fn pending(tool: &str, input: Value) -> PendingCall {
        PendingCall {
            call_id: "c1".into(),
            tool: tool.into(),
            input,
            worker: AgentId::new("w1"),
            task: TaskId::new("t1"),
        }
    }

    fn ctx() -> ToolContext {
        ToolContext {
            worker: "w1".into(),
            task: "t1".into(),
            workspace_root: std::env::temp_dir(),
            cancel: tokio_util::sync::CancellationToken::new(),
            progress: kgraf_tools::ProgressSink::default(),
        }
    }

    fn outside_workspace() -> PathBuf {
        // A path this test's own tempdir cannot be a descendant of. Created
        // (not just named) because `is_inside` now canonicalizes the
        // workspace root, which requires it to exist.
        let dir = std::env::temp_dir().join(format!("kgraf-allow-store-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create outside_workspace");
        dir
    }

    #[test]
    fn a_rule_survives_a_reload_from_disk() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("allow.json");
        let workspace = outside_workspace();

        let (mut store, report) = PersistedAllowStore::open(&path, &workspace).expect("open");
        assert_eq!(report.rule_count, 0);
        store
            .allow_prefix("proj-a", "Bash", "git", "user: trust git")
            .expect("allow");
        assert!(
            store
                .matching_rule("proj-a", "Bash", Some("git status"))
                .is_some()
        );
        assert!(
            store
                .matching_rule("proj-a", "Bash", Some("rm -rf /"))
                .is_none()
        );
        assert!(
            store
                .matching_rule("proj-b", "Bash", Some("git status"))
                .is_none()
        );

        let (reloaded, report) = PersistedAllowStore::open(&path, &workspace).expect("reopen");
        assert_eq!(report.rule_count, 1, "the reload reports what it found");
        let rule = reloaded
            .matching_rule("proj-a", "Bash", Some("git status"))
            .expect("rule persists across a reload");
        assert_eq!(rule.note, "user: trust git");
        assert!(
            reloaded
                .matching_rule("proj-a", "Bash", Some("rm -rf /"))
                .is_none()
        );
    }

    #[test]
    fn opening_a_missing_store_starts_empty_rather_than_failing() {
        let dir = tempdir().expect("tempdir");
        let (store, report) =
            PersistedAllowStore::open(dir.path().join("absent.json"), &outside_workspace())
                .expect("open");
        assert!(store.rules().is_empty());
        assert_eq!(report.rule_count, 0);
    }

    #[test]
    fn a_store_path_inside_the_workspace_root_is_refused() {
        // The store must not live where a worker's own writes
        // can reach it.
        let workspace = tempdir().expect("tempdir");
        let path = workspace.path().join(".kgraf").join("allow.json");
        let err = PersistedAllowStore::open(&path, workspace.path())
            .expect_err("a store path inside the workspace must be refused");
        assert!(matches!(err, SessionError::ApprovalStore { .. }));
    }

    #[test]
    fn an_unknown_store_version_is_refused_not_guessed_at() {
        // The store version is checked on load.
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("allow.json");
        std::fs::write(&path, r#"{"version":99,"rules":[]}"#).expect("write");
        let err = PersistedAllowStore::open(&path, &outside_workspace())
            .expect_err("an unknown version must be refused");
        assert!(matches!(err, SessionError::ApprovalStore { .. }));
    }

    #[test]
    fn an_unknown_field_in_the_store_file_is_refused() {
        // `deny_unknown_fields`: a store written by a
        // future, wider schema must not be silently read as if narrower
        // fields (like a scope) were simply absent.
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("allow.json");
        std::fs::write(
            &path,
            r#"{"version":1,"rules":[],"extra_field_from_the_future":true}"#,
        )
        .expect("write");
        let err = PersistedAllowStore::open(&path, &outside_workspace())
            .expect_err("an unknown top-level field must be refused");
        assert!(matches!(err, SessionError::ApprovalStore { .. }));
    }

    #[test]
    fn a_missing_subject_prefix_field_does_not_default_to_matching_everything() {
        // Before this fix, `subject_prefix: Option<String>` read a
        // missing field as `None`, which matched every call. `scope` is
        // required and untagged variants are explicit, so a rule missing it
        // must fail to parse rather than silently become `AnyCall`.
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("allow.json");
        std::fs::write(
            &path,
            r#"{"version":1,"rules":[{"tool":"Bash","project":"p","created_at_ms":0,"note":"n"}]}"#,
        )
        .expect("write");
        let err = PersistedAllowStore::open(&path, &outside_workspace())
            .expect_err("a rule with no scope must not parse");
        assert!(matches!(err, SessionError::ApprovalStore { .. }));
    }

    #[test]
    fn an_empty_prefix_is_refused_any_call_requires_explicit_confirmation() {
        let dir = tempdir().expect("tempdir");
        let (mut store, _) =
            PersistedAllowStore::open(dir.path().join("allow.json"), &outside_workspace())
                .expect("open");
        assert!(
            store.allow_prefix("p", "Bash", "", "note").is_err(),
            "an empty prefix must be refused"
        );
        assert!(store.rules().is_empty());
        store
            .allow_any_call("p", "Bash", "user: fully trusts Bash", ConfirmAllowAnyCall)
            .expect("allow_any_call with the marker succeeds");
        assert!(store.matching_rule("p", "Bash", None).is_some());
        assert!(store.matching_rule("p", "Bash", Some("anything")).is_some());
    }

    #[test]
    fn word_boundary_matching_admits_git_status_but_not_gitk_or_a_compound_command() {
        // The exact matrix asked for.
        let dir = tempdir().expect("tempdir");
        let (mut store, _) =
            PersistedAllowStore::open(dir.path().join("allow.json"), &outside_workspace())
                .expect("open");
        store
            .allow_prefix("p", "Bash", "git", "trust git")
            .expect("allow");

        // Admitted: exactly "git", or "git " followed by anything.
        assert!(store.matching_rule("p", "Bash", Some("git")).is_some());
        assert!(
            store
                .matching_rule("p", "Bash", Some("git status"))
                .is_some()
        );

        // Refused: `gitk` is not `git` at a word boundary.
        assert!(store.matching_rule("p", "Bash", Some("gitk")).is_none());
        assert!(
            store
                .matching_rule("p", "Bash", Some("gitk status"))
                .is_none()
        );

        // Refused: compound commands. A real tool's `permission_subject`
        // reports `None` for every one of these (proven by the hook test
        // below), so the rule never even gets a subject to compare against.
        for compound in [
            "git status; rm -rf ~",
            "git status && rm -rf ~",
            "git status | rm -rf ~",
            "git status $(rm -rf ~)",
            "git status)",
        ] {
            assert!(
                Shell::simple_command_subject(&json!({"cmd": compound})).is_none(),
                "{compound} must not be reported as a simple subject"
            );
        }
    }

    /// A rule revoked through another handle while the hook is live stops
    /// allowing calls at the next review, and a rule added the same way
    /// applies without a restart.
    #[tokio::test]
    async fn a_rule_revoked_while_the_hook_is_live_stops_allowing_at_the_next_review() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("allow.json");
        let (mut store, _) = PersistedAllowStore::open(&path, &outside_workspace()).expect("open");
        store
            .allow_any_call("proj-a", "test-command-tool", "test", ConfirmAllowAnyCall)
            .expect("allow");
        let mut registry = ToolRegistry::new();
        registry
            .register_builtin(
                Arc::new(Shell {
                    calls: Arc::new(AtomicUsize::new(0)),
                }),
                Exposure::Prefix,
            )
            .expect("register");
        let registry = Arc::new(registry);
        let discovered = DiscoveredTools::new();
        let view = ToolView {
            registry: &registry,
            discovered: &discovered,
        };
        let hook = PersistedAllowHook::new(store, "proj-a", registry.clone());
        let chain = ApprovalChain::new(vec![Arc::new(hook)]);
        let review = || async {
            let mut log = MemoryApprovalLog::default();
            chain
                .review(
                    view,
                    pending("test-command-tool", json!({"cmd": "git status"})),
                    &mut log,
                )
                .await
                .expect("review")
        };
        assert!(
            matches!(review().await, Verdict::Run(_)),
            "allowed at first"
        );

        let (mut other, _) =
            PersistedAllowStore::open(&path, &outside_workspace()).expect("second handle");
        assert_eq!(
            other
                .revoke_any_call("proj-a", "test-command-tool")
                .expect("revoke"),
            1
        );
        assert!(
            matches!(review().await, Verdict::Ask(_)),
            "a revoked rule no longer allows the call"
        );

        other
            .allow_any_call("proj-a", "test-command-tool", "again", ConfirmAllowAnyCall)
            .expect("allow again");
        assert!(
            matches!(review().await, Verdict::Run(_)),
            "a rule added meanwhile applies"
        );
    }

    /// A rewrite in the same clock tick that leaves the modification time,
    /// length and inode as they were (a coarse timestamp, an in-place write)
    /// is still read: a stamp that young is not trusted.
    #[test]
    fn a_rewrite_that_keeps_the_file_stamp_is_still_read_again() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("allow.json");
        let (mut store, _) = PersistedAllowStore::open(&path, &outside_workspace()).expect("open");
        store
            .allow_any_call("proj-a", "test-command-tool", "test", ConfirmAllowAnyCall)
            .expect("allow");
        // A modification time in the future is never trusted as proof of "unchanged",
        // so the outcome does not depend on how long the test takes to run.
        let modified = SystemTime::now() + std::time::Duration::from_secs(3600);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("open in place")
            .set_modified(modified)
            .expect("set the time");
        let hook = PersistedAllowHook::new(store, "proj-a", Arc::new(ToolRegistry::new()));
        assert!(
            hook.matching_rule("test-command-tool", None)
                .expect("read")
                .is_some(),
            "the rule applies at first"
        );
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("open in place");
        let text = std::fs::read_to_string(&path).expect("read");
        let changed = text.replace("test-command-tool", "test-command-toom");
        assert_eq!(text.len(), changed.len());
        std::io::Write::write_all(&mut &file, changed.as_bytes()).expect("write");
        file.set_modified(modified).expect("restore the time");
        assert!(
            hook.matching_rule("test-command-tool", None)
                .expect("read again")
                .is_none(),
            "the rewritten file is read, not the stamp trusted"
        );
    }

    #[tokio::test]
    async fn the_hook_derives_the_subject_from_permission_subject_never_a_raw_field_read() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("allow.json");
        let (mut store, _) = PersistedAllowStore::open(&path, &outside_workspace()).expect("open");
        store
            .allow_prefix("proj-a", "test-command-tool", "git", "test")
            .expect("allow");

        let calls = Arc::new(AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        registry
            .register_builtin(
                Arc::new(Shell {
                    calls: calls.clone(),
                }),
                Exposure::Prefix,
            )
            .expect("register");
        let registry = Arc::new(registry);
        let discovered = DiscoveredTools::new();
        let view = ToolView {
            registry: &registry,
            discovered: &discovered,
        };

        let hook = PersistedAllowHook::new(store, "proj-a", registry.clone());
        let chain = ApprovalChain::new(vec![Arc::new(hook)]);

        // A simple command the rule covers: allowed.
        let mut log = MemoryApprovalLog::default();
        let allowed = chain
            .review(
                view,
                pending("test-command-tool", json!({"cmd": "git status"})),
                &mut log,
            )
            .await
            .expect("review");
        let Verdict::Run(approved) = allowed else {
            panic!("expected the persisted rule to allow the call");
        };
        registry.execute(approved, &ctx()).await.expect("execute");
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // `gitk`: word-boundary refused even though it starts with "git".
        let mut log = MemoryApprovalLog::default();
        let asked = chain
            .review(
                view,
                pending("test-command-tool", json!({"cmd": "gitk"})),
                &mut log,
            )
            .await
            .expect("review");
        assert!(matches!(asked, Verdict::Ask(_)));

        // A compound command: the tool reports no subject at all, so a
        // `Prefix("git")` rule cannot match it regardless of the raw text
        // containing "git".
        for compound in [
            "git status; rm -rf ~",
            "git status && rm -rf ~",
            "git status | rm -rf ~",
            "git status $(rm -rf ~)",
        ] {
            let mut log = MemoryApprovalLog::default();
            let asked = chain
                .review(
                    view,
                    pending("test-command-tool", json!({"cmd": compound})),
                    &mut log,
                )
                .await
                .expect("review");
            assert!(
                matches!(asked, Verdict::Ask(_)),
                "{compound} must not be silently allowed"
            );
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "only the simple command ran"
        );
    }

    #[test]
    fn concurrent_allow_calls_do_not_lose_a_rule() {
        // Two `PersistedAllowStore` handles open on the same
        // path, each adding a rule; both must survive, proving the
        // read-modify-write is not a lost-update race. Modeled with two
        // handles on one thread (deterministic) rather than real OS threads,
        // since the property under test is "insert re-reads under the lock
        // before writing," which this already exercises: handle `a`'s
        // in-memory `file` is stale by the time `b` has written, exactly as
        // two different processes' caches would be.
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("allow.json");
        let workspace = outside_workspace();
        let (mut a, _) = PersistedAllowStore::open(&path, &workspace).expect("open a");
        let (mut b, _) = PersistedAllowStore::open(&path, &workspace).expect("open b");
        a.allow_prefix("p", "Bash", "git", "from a").expect("a");
        b.allow_prefix("p", "Curl", "curl", "from b").expect("b");

        let (reloaded, report) = PersistedAllowStore::open(&path, &workspace).expect("reopen");
        assert_eq!(report.rule_count, 2, "neither insert must be lost");
        assert!(reloaded.matching_rule("p", "Bash", Some("git")).is_some());
        assert!(reloaded.matching_rule("p", "Curl", Some("curl")).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn the_store_file_is_created_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("allow.json");
        let (mut store, _) = PersistedAllowStore::open(&path, &outside_workspace()).expect("open");
        store.allow_prefix("p", "Bash", "git", "n").expect("allow");
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "{mode:o}");
    }

    #[test]
    fn a_failed_save_does_not_leave_a_phantom_rule_in_memory() {
        let dir = tempdir().expect("tempdir");
        // A path whose parent cannot be created (a file where a directory is
        // needed) makes `write_store_file` fail deterministically.
        let blocked_parent = dir.path().join("not-a-dir");
        std::fs::write(&blocked_parent, b"x").expect("write blocker file");
        let path = blocked_parent.join("allow.json");
        let mut store = PersistedAllowStore {
            path,
            file: StoreFile {
                version: 1,
                rules: Vec::new(),
            },
        };
        let result = store.allow_prefix("proj-a", "Bash", "git", "note");
        assert!(result.is_err());
        assert!(store.rules().is_empty(), "the failed rule must not linger");
    }

    // -- Regression tests --

    #[cfg(unix)]
    #[test]
    fn a_symlink_planted_inside_the_workspace_pointing_outside_is_still_refused() {
        // A purely lexical containment
        // check sees `workspace/cfg/allow.json` as outside `workspace` (the
        // literal string `cfg` never appears under `workspace`), when `cfg`
        // is actually a symlink pointing right back into `workspace` itself.
        let workspace = tempdir().expect("tempdir");
        let outside = tempdir().expect("tempdir");
        let link = outside.path().join("cfg");
        std::os::unix::fs::symlink(workspace.path(), &link).expect("symlink");
        let path = link.join("allow.json");

        let err = PersistedAllowStore::open(&path, workspace.path())
            .expect_err("a symlink resolving back into the workspace must be refused");
        assert!(matches!(err, SessionError::ApprovalStore { .. }));
        assert!(
            !workspace.path().join("allow.json").exists(),
            "no rule may ever be written inside the workspace"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn an_uppercased_spelling_of_the_same_apfs_path_is_still_refused() {
        // On the case-insensitive default
        // (APFS/HFS+), `WORKSPACE/allow.json` and `workspace/allow.json` name
        // the same file, but a case-sensitive `starts_with` would not see it.
        let workspace = tempdir().expect("tempdir");
        let uppercased = PathBuf::from(workspace.path().to_string_lossy().to_uppercase());
        let err = PersistedAllowStore::open(uppercased.join("allow.json"), workspace.path())
            .expect_err("an uppercased alias of the workspace path must be refused");
        assert!(matches!(err, SessionError::ApprovalStore { .. }));
    }

    #[test]
    fn a_canonicalized_alias_of_the_workspace_root_is_still_refused() {
        // On macOS, `TempDir::path()` returns a
        // path under `/var/folders/...`, while `canonicalize`d it resolves
        // through `/private/var/folders/...`. Both name the same directory.
        let workspace = tempdir().expect("tempdir");
        let canonical_workspace = workspace.path().canonicalize().expect("canonicalize");
        let err =
            PersistedAllowStore::open(canonical_workspace.join("allow.json"), workspace.path())
                .expect_err("the canonical alias of the workspace root must be refused");
        assert!(matches!(err, SessionError::ApprovalStore { .. }));
    }

    #[test]
    fn a_hand_planted_any_call_rule_in_a_world_writable_file_is_refused() {
        // A rule this
        // store never wrote (mode 0666, so anything on the box could have)
        // must not be trusted just because its JSON shape parses.
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("a.json");
        std::fs::write(
            &path,
            r#"{"version":1,"rules":[{"tool":"shell","scope":{"kind":"any_call"},"project":"p","created_at_ms":0,"note":""}]}"#,
        )
        .expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666))
                .expect("chmod 0666");
        }
        let result = PersistedAllowStore::open(&path, &outside_workspace());
        #[cfg(unix)]
        {
            let err = result.expect_err("a world-writable store file must be refused");
            assert!(matches!(err, SessionError::ApprovalStore { .. }));
        }
        #[cfg(not(unix))]
        {
            // No portable ownership/mode check exists off Unix; documented in
            // this module's doc comment as a best-effort, platform-dependent
            // protection, not a guarantee.
            let _ = result;
        }
    }

    #[test]
    fn shell_injection_constructs_the_test_parser_does_not_catch_are_a_known_gap_tracked_elsewhere()
    {
        // `Shell::simple_command_subject` here is
        // a *test* double standing in for a real tool's `permission_subject`
        // (see this module's doc comment); it only refuses `; && | $( )`.
        // Most of these constructs are equally dangerous but slip through it
        // today. This test pins that known gap so a change to the real parser
        // has something to
        // compare against, rather than the gap being silently rediscovered.
        let dangerous_but_currently_treated_as_simple = [
            "git status\nrm -rf ~",                    // embedded newline: a second line
            "git `rm -rf ~`",                          // backtick command substitution
            "git status & rm -rf ~",                   // backgrounding plus a second command
            "git -c core.sshCommand='rm -rf ~' fetch", // config-flag injection
            "git diff > ~/.bashrc",                    // output redirection
        ];
        for cmd in dangerous_but_currently_treated_as_simple {
            let subject = Shell::simple_command_subject(&json!({"cmd": cmd}));
            assert_eq!(
                subject.as_deref(),
                Some(cmd),
                "{cmd:?}: the test parser's known gap changed shape; \
                 update this pin and confirm the real parser closes it"
            );
        }
        // Not a gap: `<(...)` contains a `)`, which `COMPOUND_MARKERS`
        // already refuses (incidentally, not because it recognizes process
        // substitution specifically).
        assert_eq!(
            Shell::simple_command_subject(&json!({"cmd": "git log <(rm x)"})),
            None
        );
    }

    #[test]
    fn is_inside_refuses_both_the_symlink_and_the_case_alias_forms() {
        // A direct, non-store unit test of `is_inside` itself, complementing
        // the store-level tests above.
        let workspace = tempdir().expect("tempdir");
        let ws = workspace.path();
        assert!(is_inside(&ws.join("allow.json"), ws).expect("inside"));
        assert!(!is_inside(&outside_workspace(), ws).expect("outside"));
    }

    // -- Scope::Exact and the Prefix ban --

    #[test]
    fn allow_prefix_refuses_to_create_a_rule_for_a_prefix_forbidden_tool() {
        let dir = tempdir().expect("tempdir");
        let (mut store, _) =
            PersistedAllowStore::open(dir.path().join("allow.json"), &outside_workspace())
                .expect("open");
        let err = store
            .allow_prefix("p", "shell", "git", "note")
            .expect_err("Scope::Prefix must be refused for shell");
        assert!(matches!(err, SessionError::ApprovalStore { .. }));
        let message = err.to_string();
        assert!(
            message.contains("allow_exact"),
            "the refusal must name Exact as the alternative: {message}"
        );
        assert!(store.rules().is_empty());
    }

    #[test]
    fn allow_exact_matches_only_a_byte_identical_subject() {
        let dir = tempdir().expect("tempdir");
        let (mut store, _) =
            PersistedAllowStore::open(dir.path().join("allow.json"), &outside_workspace())
                .expect("open");
        let canonical = r#"["git","status"]"#;
        store
            .allow_exact("p", "shell", canonical, "user: trust this exact call")
            .expect("allow_exact");
        assert!(store.matching_rule("p", "shell", Some(canonical)).is_some());
        // A different, "wider" invocation that a Prefix rule on "git" would
        // have matched must not match an Exact rule.
        assert!(
            store
                .matching_rule("p", "shell", Some(r#"["git","status","--short"]"#))
                .is_none()
        );
        assert!(
            store
                .matching_rule(
                    "p",
                    "shell",
                    Some(r#"["git","-c","core.sshCommand=evil","fetch"]"#)
                )
                .is_none()
        );
    }

    /// A shell rule recorded under the earlier, wider subject check (here
    /// `python3 -c`, which `permission_subject` no longer names) can never
    /// match again. It is kept in the file and reported, never dropped
    /// silently; a rule the allowlist still accepts is not reported.
    #[test]
    fn a_shell_rule_no_subject_can_match_is_kept_and_reported_as_inert() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("allow.json");
        let (mut store, _) = PersistedAllowStore::open(&path, &outside_workspace()).expect("open");
        for subject in [
            r#"["python3","-c","print(1)"]"#,
            r#"["git","status"]"#,
            "git status",
        ] {
            store
                .allow_exact("p", "shell", subject, "recorded earlier")
                .expect("allow_exact");
        }
        store
            .allow_exact("p", "other", r#"["python3","-c","print(1)"]"#, "not shell")
            .expect("allow_exact");

        let (reopened, report) =
            PersistedAllowStore::open(&path, &outside_workspace()).expect("reopen");
        assert_eq!(report.rule_count, 4, "no rule is dropped");
        assert_eq!(report.inert_rule_count, 2);
        let inert: Vec<&Scope> = reopened.inert_rules().iter().map(|r| &r.scope).collect();
        assert_eq!(
            inert,
            [
                &Scope::Exact(r#"["python3","-c","print(1)"]"#.into()),
                &Scope::Exact("git status".into()),
            ]
        );
    }

    #[test]
    fn an_empty_exact_subject_is_refused() {
        let dir = tempdir().expect("tempdir");
        let (mut store, _) =
            PersistedAllowStore::open(dir.path().join("allow.json"), &outside_workspace())
                .expect("open");
        assert!(store.allow_exact("p", "shell", "", "note").is_err());
        assert!(store.rules().is_empty());
    }

    /// End-to-end with the real `ShellTool`, not the `Shell` test double: an
    /// `Exact` rule allows a byte-identical repeat of one approved command
    /// and nothing else, including a compound command built to start the
    /// same way.
    #[tokio::test]
    async fn the_real_shell_tool_is_allowed_only_by_an_exact_rule_never_a_prefix() {
        use kgraf_tools::execpolicy::{Decision, ExecPolicy};
        use kgraf_tools::{Exposure, ShellTool};

        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("allow.json");
        let (mut store, _) = PersistedAllowStore::open(&path, &outside_workspace()).expect("open");

        let (shell_tool, _hook) = ShellTool::new(ExecPolicy::new(Decision::Allow));
        let canonical_subject = shell_tool
            .permission_subject(&json!({"command": ["git", "status"]}))
            .expect("a plain command has a subject");
        store
            .allow_exact("proj-a", "shell", &canonical_subject, "test")
            .expect("allow_exact");

        let mut registry = ToolRegistry::new();
        registry
            .register_builtin(Arc::new(shell_tool), Exposure::Prefix)
            .expect("register");
        let registry = Arc::new(registry);
        let discovered = DiscoveredTools::new();
        let view = ToolView {
            registry: &registry,
            discovered: &discovered,
        };
        let hook = PersistedAllowHook::new(store, "proj-a", registry.clone());
        let chain = ApprovalChain::new(vec![Arc::new(hook)]);

        let mut log = MemoryApprovalLog::default();
        let allowed = chain
            .review(
                view,
                pending("shell", json!({"command": ["git", "status"]})),
                &mut log,
            )
            .await
            .expect("review");
        assert!(
            matches!(allowed, Verdict::Run(_)),
            "the exact repeat must be allowed"
        );

        // A longer invocation that merely starts the same way is not allowed
        // by the Exact rule (proof that this is not a Prefix rule in disguise).
        let mut log = MemoryApprovalLog::default();
        let asked = chain
            .review(
                view,
                pending("shell", json!({"command": ["git", "status", "--short"]})),
                &mut log,
            )
            .await
            .expect("review");
        assert!(matches!(asked, Verdict::Ask(_)));
    }
}

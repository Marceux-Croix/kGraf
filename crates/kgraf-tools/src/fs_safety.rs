//! Symlink-resistant, TOCTOU-free filesystem access confined to the workspace
//! root.
//!
//! **Why this exists.** `access::normalize` (`crate::normalize_relative`) is a
//! *lexical* check: it drops `.`, resolves a leading `..`, and refuses an
//! absolute path. It has no way to see a symlink. A workspace containing
//! `link -> /etc` makes `link/passwd` normalize to a path that looks entirely
//! inside the workspace, while resolving on disk to a file outside it. Four
//! tools once trusted the lexical check alone:
//! `read_file` read a planted secret through such a link, `list_directory` and
//! `search_files` walked through one, and `apply_patch` wrote
//! `d/authorized_keys` through a symlinked directory, overwrote a symlinked
//! `.zshrc`, and deleted a file outside the workspace through a symlinked
//! intermediate directory. A `stat`-then-`open` check does not close this: the
//! symlink can be swapped in between the two calls (TOCTOU).
//!
//! **The fix.** Every filesystem touch in this crate goes through a
//! [`cap_std::fs::Dir`] opened on the workspace root, never through
//! `std::fs` with a joined path. `Dir` resolves each path component against
//! an already-open directory descriptor with `openat`-family syscalls (a
//! manual per-component `O_NOFOLLOW`-and-retry loop where the kernel has
//! nothing better, `openat2(RESOLVE_BENEATH)` on Linux where it does), so a
//! resolution that would leave the directory tree is refused **inside the
//! same syscall that opens the path**: there is no window between a check
//! and an open for a symlink to change. A symlink that stays inside the
//! workspace still works normally; only an escape is refused. Ported: cap-std
//! (Bytecode Alliance; MIT OR Apache-2.0 OR Apache-2.0 WITH LLVM-exception;
//! reuse of existing code). Hand-rolling this loop over raw `openat`/`fcntl(F_GETPATH)` was
//! considered and rejected as exactly the security-critical, easy-to-misstep
//! code that should be reused rather than written from scratch; cap-std is the
//! primitive WASI's own sandboxing is built on.
//!
//! **What is refused beyond an escape.** Every file this module opens for
//! reading is checked, on the opened handle, to be a regular file: not a
//! FIFO, a socket, a device or a directory. A FIFO would otherwise block a
//! tool call forever waiting for a writer that will never come, and a device
//! node could read or write host hardware directly. The open itself carries
//! `O_NONBLOCK` (a no-op on a regular file) so that check runs even against a
//! FIFO, rather than hanging before the check gets a chance to refuse it.
//! Reads are capped in bytes so a tool cannot be made to buffer an unbounded
//! file into memory.
//!
//! **Hard links.** Confinement by directory
//! descriptor cannot see a hard link: a regular file inside the workspace whose
//! inode also has a name outside it (`ln ~/.ssh/authorized_keys keys`, made
//! before the worker ever ran) resolves entirely inside the tree, and a write
//! through it changes the outside file. Nothing about the inside name tells the
//! two cases apart except the link count. So a regular file whose link count is
//! greater than 1 is refused for **write, delete and move**, always, and for
//! **read** unless the user allowed that path ([`AllowedHardlinkReads`], empty
//! by default). The refusal names the path and the count
//! ([`crate::AccessError::HardLinked`]). The write path checks the count on the
//! opened descriptor **before** truncating: the open no longer truncates, the
//! file is emptied with `set_len(0)` only after the check, so a refusal leaves
//! the linked file untouched. A link created after that check needs a process
//! running concurrently with the write in the same workspace; the shell tool,
//! the one tool that could, declares exclusive access and so never overlaps a
//! file write. Git checkouts do not create hard links, so a normal worktree has
//! none; a count above 1 is either a deliberate setup or an attack.
//!
//! Off Unix the hard-link refusal is not made: `cap_std`'s portable metadata
//! has no link count on stable Rust (its Windows count needs the unstable
//! `windows_by_handle`), so [`link_count`] reports `1` there. This is a known
//! gap, not a checked one: on Windows a file hard-linked to one outside the
//! workspace can be written through. Closing it needs the count read from the
//! open handle (`GetFileInformationByHandle`), verified on a Windows host.

use crate::error::{AccessError, ToolError};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};

/// Bytes kept from one file read. Chosen generously for source and config
/// files while still bounding memory against an adversarial or enormous file;
/// callers needing a different limit (none do yet) can add one.
pub(crate) const MAX_READ_BYTES: u64 = 16 * 1024 * 1024;

/// Open the workspace root as a confinement boundary. Every subsequent path
/// in this module is resolved relative to the returned `Dir` and can never
/// resolve outside it.
pub(crate) fn open_workspace(tool: &str, root: &Path) -> Result<Dir, ToolError> {
    Dir::open_ambient_dir(root, ambient_authority()).map_err(|source| ToolError::Internal {
        tool: tool.into(),
        reason: format!("cannot open workspace root {}: {source}", root.display()),
    })
}

fn read_only_nonblocking() -> OpenOptions {
    let mut opts = OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NONBLOCK);
    }
    opts
}

/// `write(create)`, plus `O_NONBLOCK` on unix so opening an existing FIFO for
/// writing cannot block this call forever waiting for a reader (`dir.create`
/// alone hung on a FIFO). A no-op on a regular file.
///
/// **No `truncate`.** Truncating at open would empty a hard-linked file before
/// its link count could be checked on the descriptor; [`write_confined`]
/// empties the file with `set_len(0)` after the checks pass.
fn write_create_nonblocking() -> OpenOptions {
    let mut opts = OpenOptions::new();
    opts.write(true).create(true);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NONBLOCK);
    }
    opts
}

/// A protected path component (`kgraf_core::paths::is_protected_name`), at
/// any depth: `.git` and the other names that steer git, and kGraf's state
/// directory, however the file system may spell them (case, Unicode
/// normalization, the long s, zero-width characters, trailing dots, NTFS short
/// names and streams; `.GIT`, `.kGraf`, `GIT~1`, ...). Refused everywhere in
/// this module: an
/// unsandboxed tool rewriting `.git` (the gitlink, `.git/config`, a hook) can
/// change what a later, approval-free read-only `git` command does -- a
/// reproduction planted `core.fsmonitor` this
/// way, then had an ordinary `git status` execute it. `.kgraf` is
/// kGraf's own metadata/scratch convention (`.kgraf-shell-scratch`
/// and friends); the same argument applies to it once anything is stored
/// there that later execution trusts.
fn touches_git_or_kgraf(rel: &Path) -> bool {
    rel.components().any(|c| {
        // A name that is not UTF-8 cannot fold to a protected one: the lossy
        // form holds U+FFFD, which no protected name contains.
        kgraf_core::paths::is_protected_name(&c.as_os_str().to_string_lossy())
    })
}

fn reads_git_dir_or_kgraf(rel: &Path) -> bool {
    rel.components()
        .any(|c| kgraf_core::paths::is_unreadable_name(&c.as_os_str().to_string_lossy()))
}

/// The read-side guard: the `.git` directory and kGraf's state are refused;
/// repository text files such as `.gitmodules` stay readable.
pub(crate) fn refuses_git_dir_or_kgraf(tool: &str, rel: &Path) -> Result<(), ToolError> {
    if reads_git_dir_or_kgraf(rel) {
        return Err(ToolError::Access {
            tool: tool.into(),
            source: crate::AccessError::Escapes {
                path: format!(
                    "{}: is inside the `.git` directory or kGraf's own state, always refused",
                    rel.display()
                ),
            },
        });
    }
    Ok(())
}

pub(crate) fn refuses_git_or_kgraf(tool: &str, rel: &Path) -> Result<(), ToolError> {
    if touches_git_or_kgraf(rel) {
        return Err(ToolError::Access {
            tool: tool.into(),
            source: crate::AccessError::Escapes {
                path: format!(
                    "{}: touches repository metadata that steers git, or kGraf's own state, \
                     always refused",
                    rel.display()
                ),
            },
        });
    }
    Ok(())
}

/// Symbolic links followed while resolving one path, at most; a longer chain
/// is refused as a loop. Matches Linux's own `MAXSYMLINKS`.
const MAX_LINK_HOPS: usize = 40;

/// One step of a path being resolved: up one level, or into a named entry.
enum Step {
    Up,
    Name(std::ffi::OsString),
}

/// `path` as resolution steps; `None` for an absolute path.
fn steps_of(path: &Path) -> Option<std::collections::VecDeque<Step>> {
    use std::path::Component;
    let mut steps = std::collections::VecDeque::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => steps.push_back(Step::Up),
            Component::Normal(n) => steps.push_back(Step::Name(n.to_owned())),
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(steps)
}

/// `rel` with every symbolic link inside the workspace followed, resolved
/// lexically against the directory holding each link: the path an open will
/// actually reach. The component guards above are lexical, so a link
/// `g -> .git` makes `g/config` look ordinary while every open through it
/// reaches `.git/config`; checking the resolved path as well closes that.
///
/// A link whose target is absolute or climbs above the workspace is refused
/// here as an escape (the confined open would refuse it too). A component
/// that does not exist ends the lookups: nothing beyond it can be a link.
/// The resolution and the later open are two steps; a link swapped in
/// between needs a process writing this tree concurrently, and the shell
/// tool, the one tool that could, declares exclusive access.
pub(crate) fn resolve_links(tool: &str, dir: &Dir, rel: &Path) -> Result<PathBuf, ToolError> {
    let escape = || ToolError::Access {
        tool: tool.into(),
        source: AccessError::Escapes {
            path: format!("{}: a link in it leaves the workspace", rel.display()),
        },
    };
    let mut pending = steps_of(rel).ok_or_else(escape)?;
    let mut resolved = PathBuf::new();
    let mut hops = 0;
    let mut missing = false;
    while let Some(step) = pending.pop_front() {
        let name = match step {
            Step::Up => {
                if !resolved.pop() {
                    return Err(escape());
                }
                continue;
            }
            Step::Name(name) => name,
        };
        let candidate = resolved.join(&name);
        if missing {
            resolved = candidate;
            continue;
        }
        match dir.symlink_metadata(&candidate) {
            Ok(meta) if meta.file_type().is_symlink() => {
                hops += 1;
                if hops > MAX_LINK_HOPS {
                    return Err(escape());
                }
                let target = dir
                    .read_link_contents(&candidate)
                    .map_err(|e| open_error(tool, rel, e))?;
                let mut expanded = steps_of(&target).ok_or_else(escape)?;
                expanded.extend(pending);
                pending = expanded;
            }
            Ok(_) => resolved = candidate,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                missing = true;
                resolved = candidate;
            }
            Err(e) => return Err(open_error(tool, rel, e)),
        }
    }
    Ok(resolved)
}

fn through_link(tool: &str, rel: &Path, resolved: &Path) -> ToolError {
    ToolError::Access {
        tool: tool.into(),
        source: AccessError::Escapes {
            path: format!(
                "{}: a link in it resolves to {}, repository metadata or kGraf's own state, \
                 always refused",
                rel.display(),
                resolved.display()
            ),
        },
    }
}

/// The read guard on `rel` and on `rel` with its in-workspace links followed.
fn guard_read(tool: &str, dir: &Dir, rel: &Path) -> Result<(), ToolError> {
    refuses_git_dir_or_kgraf(tool, rel)?;
    let resolved = resolve_links(tool, dir, rel)?;
    refuses_git_dir_or_kgraf(tool, &resolved).map_err(|_| through_link(tool, rel, &resolved))
}

/// The write guard on `rel` and on `rel` with its in-workspace links
/// followed: a link to `.gitmodules` or into `.git` is refused, not written
/// through. Also the check a planning pass runs before anything is written.
pub(crate) fn guard_write(tool: &str, dir: &Dir, rel: &Path) -> Result<(), ToolError> {
    refuses_git_or_kgraf(tool, rel)?;
    let resolved = resolve_links(tool, dir, rel)?;
    refuses_git_or_kgraf(tool, &resolved).map_err(|_| through_link(tool, rel, &resolved))
}

/// Classify a `cap_std` open or metadata failure: an attempted escape (a
/// symlink resolving outside the workspace, refused by `Dir` itself) is
/// reported as [`ToolError::Access`]; anything else is [`ToolError::Internal`].
fn open_error(tool: &str, rel: &Path, source: std::io::Error) -> ToolError {
    let escapes = matches!(
        source.kind(),
        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::InvalidFilename
    ) || unix_escape_errno(&source);
    if escapes {
        ToolError::Access {
            tool: tool.into(),
            source: crate::AccessError::Escapes {
                path: rel.display().to_string(),
            },
        }
    } else {
        ToolError::Internal {
            tool: tool.into(),
            reason: format!("cannot open {}: {source}", rel.display()),
        }
    }
}

/// `ELOOP` (a symlink loop `cap_std` walked into while resolving a path component) and
/// `EXDEV` (a cross-device rename/link, which `cap_std` also uses to signal a refused
/// escape in some paths) are POSIX errno values with no Windows equivalent; `libc` is a
/// `cfg(unix)`-only dependency here (`Cargo.toml`), so this check must be too. This was
/// previously inlined, unguarded, directly in `open_error`, which failed every
/// non-unix build (cross-compiling `kgraf-cli` for
/// `x86_64-pc-windows-msvc`) with "cannot find module or crate `libc`" -- unix behavior
/// is unchanged; a non-unix build now falls through to the `ErrorKind` checks above,
/// which is what `cap_std`'s own Windows backend maps an escape to.
#[cfg(unix)]
fn unix_escape_errno(source: &std::io::Error) -> bool {
    source.raw_os_error() == Some(libc::ELOOP) || source.raw_os_error() == Some(libc::EXDEV)
}

#[cfg(not(unix))]
fn unix_escape_errno(_source: &std::io::Error) -> bool {
    false
}

fn not_a_regular_file(tool: &str, rel: &Path) -> ToolError {
    ToolError::Internal {
        tool: tool.into(),
        reason: format!(
            "{} is not a regular file (a FIFO, device, socket or directory is refused)",
            rel.display()
        ),
    }
}

fn too_large(tool: &str, rel: &Path, len: u64, cap: u64) -> ToolError {
    ToolError::Internal {
        tool: tool.into(),
        reason: format!(
            "{} is {len} bytes, over the {cap}-byte read cap",
            rel.display()
        ),
    }
}

/// Workspace-relative paths whose hard-linked files the file tools may still
/// **read** (never write, delete or move). Empty by default. Set from the
/// user's own configuration, which lives outside every worker's writable
/// roots; a list a worker could edit would let it allow itself.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AllowedHardlinkReads {
    paths: Vec<PathBuf>,
}

impl AllowedHardlinkReads {
    /// Allow reads of hard-linked files at or beneath each of `paths`, which
    /// are relative to the workspace root. An absolute or escaping path is
    /// refused rather than ignored.
    pub fn new(paths: impl IntoIterator<Item = PathBuf>) -> Result<Self, AccessError> {
        let paths = paths
            .into_iter()
            .map(|p| crate::normalize_relative(&p))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { paths })
    }

    /// Whether a hard-linked file at `rel` (workspace-relative) may be read.
    pub fn permits(&self, rel: &Path) -> bool {
        let Ok(rel) = crate::normalize_relative(rel) else {
            return false;
        };
        self.paths.iter().any(|allowed| rel.starts_with(allowed))
    }

    /// The configured paths, normalized.
    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }
}

/// The number of hard links to the inode `meta` describes. `1` off Unix, where
/// the portable metadata has no count: the known gap the module docs state.
fn link_count(meta: &cap_std::fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        use cap_std::fs::MetadataExt;
        meta.nlink()
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        1
    }
}

/// Refuse a regular file with more than one hard link.
fn refuse_hard_linked(
    tool: &str,
    rel: &Path,
    meta: &cap_std::fs::Metadata,
) -> Result<(), ToolError> {
    let links = link_count(meta);
    if meta.is_file() && links > 1 {
        return Err(ToolError::Access {
            tool: tool.into(),
            source: AccessError::HardLinked {
                path: rel.display().to_string(),
                links,
            },
        });
    }
    Ok(())
}

/// Refuse `rel` if it exists and is a hard-linked regular file. For a planning
/// pass (apply_patch's validate-then-commit) that must fail before any write.
pub(crate) fn refuse_if_hard_linked(tool: &str, dir: &Dir, rel: &Path) -> Result<(), ToolError> {
    match metadata_if_exists(tool, dir, rel)? {
        Some(meta) => refuse_hard_linked(tool, rel, &meta),
        None => Ok(()),
    }
}

/// Check that `rel` is a regular file this module would read, without reading
/// it: confined to `dir`, not `.git` or `.kgraf`, not a FIFO or device,
/// and not hard-linked unless `allow_hard_link`. For a tool whose content comes
/// from elsewhere (the graph) but which must honor the same rules as a read.
pub(crate) fn check_readable(
    tool: &str,
    dir: &Dir,
    rel: &Path,
    allow_hard_link: bool,
) -> Result<(), ToolError> {
    guard_read(tool, dir, rel)?;
    let file = dir
        .open_with(rel, &read_only_nonblocking())
        .map_err(|e| open_error(tool, rel, e))?;
    let meta = file.metadata().map_err(|e| open_error(tool, rel, e))?;
    if !meta.is_file() {
        return Err(not_a_regular_file(tool, rel));
    }
    if !allow_hard_link {
        refuse_hard_linked(tool, rel, &meta)?;
    }
    Ok(())
}

/// Read `rel`'s contents, confined to `dir`'s directory tree, refusing
/// anything that is not a plain file, anything over `max_bytes`, and a
/// hard-linked file unless `allow_hard_link` (the caller's
/// [`AllowedHardlinkReads`] verdict for the workspace-relative path).
pub(crate) fn read_capped(
    tool: &str,
    dir: &Dir,
    rel: &Path,
    max_bytes: u64,
    allow_hard_link: bool,
) -> Result<Vec<u8>, ToolError> {
    guard_read(tool, dir, rel)?;
    let file = dir
        .open_with(rel, &read_only_nonblocking())
        .map_err(|e| open_error(tool, rel, e))?;
    let meta = file.metadata().map_err(|e| open_error(tool, rel, e))?;
    if !meta.is_file() {
        return Err(not_a_regular_file(tool, rel));
    }
    if !allow_hard_link {
        refuse_hard_linked(tool, rel, &meta)?;
    }
    if meta.len() > max_bytes {
        return Err(too_large(tool, rel, meta.len(), max_bytes));
    }
    let mut buf = Vec::with_capacity(meta.len().min(max_bytes) as usize);
    // One extra byte: if the file grew between `metadata` and this read, the
    // cap is still enforced rather than silently accepting more.
    (&file)
        .take(max_bytes + 1)
        .read_to_end(&mut buf)
        .map_err(|e| open_error(tool, rel, e))?;
    if buf.len() as u64 > max_bytes {
        return Err(too_large(tool, rel, buf.len() as u64, max_bytes));
    }
    Ok(buf)
}

/// Read `rel` as UTF-8 text; a file that is not valid UTF-8 is refused rather
/// than lossily decoded, matching [`crate::OutputPart::from_bytes`]'s rule
/// that a caller is never shown replacement characters and told it is text.
pub(crate) fn read_capped_to_string(
    tool: &str,
    dir: &Dir,
    rel: &Path,
    max_bytes: u64,
    allow_hard_link: bool,
) -> Result<String, ToolError> {
    let bytes = read_capped(tool, dir, rel, max_bytes, allow_hard_link)?;
    String::from_utf8(bytes).map_err(|_| ToolError::Internal {
        tool: tool.into(),
        reason: format!("{} is not valid UTF-8", rel.display()),
    })
}

/// Open `rel` as a directory capability, confined to `dir`'s tree. A symlink
/// resolving outside the tree, or a `rel` that is not a directory at all, is
/// refused by the same syscall that opens it.
pub(crate) fn open_dir_confined(
    tool: &str,
    dir: &cap_std::fs::Dir,
    rel: &Path,
) -> Result<cap_std::fs::Dir, ToolError> {
    guard_read(tool, dir, rel)?;
    dir.open_dir(rel).map_err(|e| open_error(tool, rel, e))
}

/// Metadata for `rel`, symlinks resolved but confined to the workspace; `None`
/// if nothing exists there.
pub(crate) fn metadata_if_exists(
    tool: &str,
    dir: &Dir,
    rel: &Path,
) -> Result<Option<cap_std::fs::Metadata>, ToolError> {
    guard_read(tool, dir, rel)?;
    match dir.metadata(rel) {
        Ok(meta) => Ok(Some(meta)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(open_error(tool, rel, e)),
    }
}

/// Write `contents` to `rel`, creating parent directories as needed, confined
/// to the workspace. Refuses to write where a directory already exists.
pub(crate) fn write_confined(
    tool: &str,
    dir: &Dir,
    rel: &Path,
    contents: &[u8],
) -> Result<(), ToolError> {
    guard_write(tool, dir, rel)?;
    if let Some(parent) = rel.parent().filter(|p| !p.as_os_str().is_empty()) {
        dir.create_dir_all(parent)
            .map_err(|e| open_error(tool, parent, e))?;
    }
    // Checked before the open (fast: a `stat`, not a blocking open) so an
    // existing FIFO, device or socket at `rel` is refused with a clear reason
    // rather than reaching the open call below at all.
    if let Some(meta) = metadata_if_exists(tool, dir, rel)? {
        if meta.is_dir() {
            return Err(ToolError::Internal {
                tool: tool.into(),
                reason: format!("{} is a directory", rel.display()),
            });
        }
        if !meta.is_file() {
            return Err(not_a_regular_file(tool, rel));
        }
        refuse_hard_linked(tool, rel, &meta)?;
    }
    // `O_NONBLOCK`: if a FIFO was planted at `rel` in the window
    // between the check above and this open (TOCTOU), opening it for writing
    // would otherwise block this call forever waiting for a reader that will
    // never come; non-blocking makes that open fail immediately instead.
    let mut file = dir
        .open_with(rel, &write_create_nonblocking())
        .map_err(|e| open_error(tool, rel, e))?;
    // Checked again on the opened handle, not just the pre-check above: the
    // only trustworthy answer to "what did I just open" is a stat of the fd
    // itself, never a stat of the path taken before the open. The file has not
    // been truncated yet, so a refusal here leaves a linked file intact.
    let meta = file.metadata().map_err(|e| open_error(tool, rel, e))?;
    if !meta.is_file() {
        return Err(not_a_regular_file(tool, rel));
    }
    refuse_hard_linked(tool, rel, &meta)?;
    file.set_len(0).map_err(|e| open_error(tool, rel, e))?;
    use std::io::Write;
    file.write_all(contents)
        .map_err(|e| open_error(tool, rel, e))
}

/// Remove the regular file at `rel`, confined to the workspace. Refuses a
/// directory, a path that does not resolve to a plain file, and a hard-linked
/// file.
pub(crate) fn remove_file_confined(tool: &str, dir: &Dir, rel: &Path) -> Result<(), ToolError> {
    guard_write(tool, dir, rel)?;
    let meta = dir.metadata(rel).map_err(|e| open_error(tool, rel, e))?;
    if !meta.is_file() {
        return Err(not_a_regular_file(tool, rel));
    }
    refuse_hard_linked(tool, rel, &meta)?;
    dir.remove_file(rel).map_err(|e| open_error(tool, rel, e))
}

/// `(dev, ino)` for `rel`, symlinks resolved but confined to the workspace.
/// Used to tell whether two different spellings of a path -- a case-only
/// alias on a case-insensitive filesystem, or a symlink alias -- name the
/// *same* file: comparing the path
/// strings cannot see that; comparing identity on the resolved file can.
#[cfg(unix)]
pub(crate) fn file_identity(
    tool: &str,
    dir: &Dir,
    rel: &Path,
) -> Result<Option<(u64, u64)>, ToolError> {
    use cap_std::fs::MetadataExt;
    Ok(metadata_if_exists(tool, dir, rel)?.map(|m| (m.dev(), m.ino())))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tool guard folds names as the volume does, so a spelling that
    /// reaches `.git` or `.kgraf` on some file system is refused, not only the
    /// ASCII forms. Ordinary dot files stay open.
    #[test]
    fn the_guard_matches_every_spelling_of_a_protected_name() {
        for bad in [
            ".git/config",
            ".GIT/config",
            "a/.Git",
            ".haustoria/x",
            ".kgraf/x",
            ".git./config",
            ".git /config",
            "GIT~1/config",
            ".g\u{200B}it/config",
            ".git\u{FEFF}/config",
            ".gitmodule\u{17F}",
            ".git::$INDEX_ALLOCATION/config",
            ".\u{FF27}\u{FF29}\u{FF34}/config",
        ] {
            assert!(touches_git_or_kgraf(Path::new(bad)), "{bad:?}");
            assert!(
                refuses_git_or_kgraf("t", Path::new(bad)).is_err(),
                "{bad:?}"
            );
        }
        for ok in [
            "src/lib.rs",
            ".gitignore",
            ".gitlab-ci.yml",
            ".github/workflows/ci.yml",
            "git.rs",
            "kgraf.rs",
        ] {
            assert!(!touches_git_or_kgraf(Path::new(ok)), "{ok:?}");
        }
    }

    #[test]
    fn repository_text_files_are_readable_but_never_writable() {
        for readable in [
            ".gitmodules",
            ".gitattributes",
            ".gitconfig",
            "sub/.gitattributes",
        ] {
            assert!(
                refuses_git_dir_or_kgraf("t", Path::new(readable)).is_ok(),
                "{readable:?}"
            );
            assert!(
                refuses_git_or_kgraf("t", Path::new(readable)).is_err(),
                "{readable:?}"
            );
        }
        for unreadable in [
            ".git/config",
            ".GIT/HEAD",
            "GIT~1/config",
            ".kgraf/ledger.db",
            ".g\u{200B}it/config",
        ] {
            assert!(
                refuses_git_dir_or_kgraf("t", Path::new(unreadable)).is_err(),
                "{unreadable:?}"
            );
        }
    }

    fn workspace() -> (tempfile::TempDir, Dir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let opened = open_workspace("test", dir.path()).expect("open");
        (dir, opened)
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_escaping_the_workspace_is_refused_on_read() {
        let (root, dir) = workspace();
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("secret.txt"), "top secret").expect("seed");
        std::os::unix::fs::symlink(
            outside.path().join("secret.txt"),
            root.path().join("link.txt"),
        )
        .expect("symlink");
        let err = read_capped("test", &dir, Path::new("link.txt"), MAX_READ_BYTES, false)
            .expect_err("must refuse the escape");
        assert!(matches!(
            err,
            ToolError::Access { .. } | ToolError::Internal { .. }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_intermediate_directory_escaping_the_workspace_is_refused() {
        let (root, dir) = workspace();
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("authorized_keys"), "ssh-ed25519 AAAA").expect("seed");
        std::os::unix::fs::symlink(outside.path(), root.path().join("d")).expect("symlink");
        let err = write_confined("test", &dir, Path::new("d/authorized_keys"), b"pwned")
            .expect_err("must refuse the escape");
        assert!(matches!(
            err,
            ToolError::Access { .. } | ToolError::Internal { .. }
        ));
        assert_eq!(
            std::fs::read_to_string(outside.path().join("authorized_keys")).expect("unchanged"),
            "ssh-ed25519 AAAA"
        );
    }

    #[cfg(unix)]
    #[test]
    fn overwriting_a_symlink_to_an_outside_file_is_refused() {
        let (root, dir) = workspace();
        let outside = tempfile::tempdir().expect("outside");
        let target = outside.path().join(".zshrc");
        std::fs::write(&target, "original").expect("seed");
        std::os::unix::fs::symlink(&target, root.path().join(".zshrc")).expect("symlink");
        let err = write_confined("test", &dir, Path::new(".zshrc"), b"pwned")
            .expect_err("must refuse the escape");
        assert!(matches!(
            err,
            ToolError::Access { .. } | ToolError::Internal { .. }
        ));
        assert_eq!(
            std::fs::read_to_string(&target).expect("unchanged"),
            "original"
        );
    }

    #[cfg(unix)]
    #[test]
    fn deleting_through_a_symlinked_intermediate_directory_is_refused() {
        let (root, dir) = workspace();
        let outside = tempfile::tempdir().expect("outside");
        let target = outside.path().join("keep.txt");
        std::fs::write(&target, "do not delete me").expect("seed");
        std::os::unix::fs::symlink(outside.path(), root.path().join("d")).expect("symlink");
        let err = remove_file_confined("test", &dir, Path::new("d/keep.txt"))
            .expect_err("must refuse the escape");
        assert!(matches!(
            err,
            ToolError::Access { .. } | ToolError::Internal { .. }
        ));
        assert!(target.exists(), "the outside file must survive");
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_is_refused_not_blocked_on() {
        let (root, dir) = workspace();
        let fifo = root.path().join("pipe");
        let c_path = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
        assert_eq!(rc, 0, "mkfifo failed");
        let err = read_capped("test", &dir, Path::new("pipe"), MAX_READ_BYTES, false)
            .expect_err("a FIFO must be refused");
        assert!(matches!(err, ToolError::Internal { .. }));
    }

    #[test]
    fn a_file_within_the_workspace_reads_and_writes_normally() {
        let (_root, dir) = workspace();
        write_confined("test", &dir, Path::new("a/b.txt"), b"hello").expect("write");
        let read = read_capped_to_string("test", &dir, Path::new("a/b.txt"), MAX_READ_BYTES, false)
            .expect("read");
        assert_eq!(read, "hello");
        remove_file_confined("test", &dir, Path::new("a/b.txt")).expect("remove");
        assert!(
            metadata_if_exists("test", &dir, Path::new("a/b.txt"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_file_over_the_cap_is_refused() {
        let (_root, dir) = workspace();
        write_confined("test", &dir, Path::new("big.txt"), &[b'x'; 100]).expect("write");
        let err =
            read_capped("test", &dir, Path::new("big.txt"), 10, false).expect_err("must refuse");
        assert!(matches!(err, ToolError::Internal { .. }));
    }

    /// Regression: `write_confined` used to open an existing FIFO
    /// with plain `dir.create`, which blocks forever waiting for a reader.
    /// This must return an error quickly instead.
    #[cfg(unix)]
    #[test]
    fn writing_to_an_existing_fifo_does_not_hang() {
        let (root, dir) = workspace();
        let fifo = root.path().join("f");
        let c_path = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        let started = std::time::Instant::now();
        let err = write_confined("test", &dir, Path::new("f"), b"x").expect_err("must refuse");
        assert!(matches!(err, ToolError::Internal { .. }));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "took {:?}, must not block waiting for a reader",
            started.elapsed()
        );
    }

    /// `.git` and `.kgraf` are refused at any depth, in any case,
    /// for read, write and delete.
    #[test]
    fn dot_git_and_dot_kgraf_are_refused_everywhere() {
        let (root, dir) = workspace();
        std::fs::create_dir_all(root.path().join(".git")).expect("seed .git dir");
        std::fs::write(root.path().join(".git/config"), "").expect("seed .git/config");
        for rel in [
            ".git",
            ".git/config",
            ".GIT/config",
            "sub/.git/hooks/pre-commit",
            ".kgraf",
            ".kGraf/scratch",
        ] {
            let path = Path::new(rel);
            assert!(
                matches!(
                    read_capped("test", &dir, path, MAX_READ_BYTES, false),
                    Err(ToolError::Access { .. })
                ),
                "{rel} must refuse a read"
            );
            assert!(
                matches!(
                    write_confined("test", &dir, path, b"x"),
                    Err(ToolError::Access { .. })
                ),
                "{rel} must refuse a write"
            );
            assert!(
                matches!(
                    remove_file_confined("test", &dir, path),
                    Err(ToolError::Access { .. })
                ),
                "{rel} must refuse a delete"
            );
        }
        assert_eq!(
            std::fs::read_to_string(root.path().join(".git/config")).expect("unchanged"),
            ""
        );
    }

    #[cfg(unix)]
    #[test]
    fn file_identity_recognizes_a_case_only_alias_on_this_filesystem() {
        let (root, dir) = workspace();
        write_confined("test", &dir, Path::new("Readme.md"), b"keep").expect("write");
        let a = file_identity("test", &dir, Path::new("Readme.md"))
            .expect("identity")
            .expect("exists");
        let b = file_identity("test", &dir, Path::new("README.MD")).expect("identity");
        // Whether this filesystem is case-insensitive varies by host; either
        // way the function must not error, and if it does exist under the
        // other case it must be the identical file.
        if let Some(b) = b {
            assert_eq!(
                a, b,
                "same directory entry, different case, must be one file"
            );
        }
        let _ = root;
    }

    /// A workspace holding `keys`, a hard link to a file outside it, the setup
    /// a planted link needs: nothing inside the workspace resolves outside it.
    #[cfg(unix)]
    fn hard_linked_workspace() -> (tempfile::TempDir, tempfile::TempDir, Dir) {
        let (root, dir) = workspace();
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("authorized_keys"), "ssh-ed25519 AAAA").expect("seed");
        std::fs::hard_link(
            outside.path().join("authorized_keys"),
            root.path().join("keys"),
        )
        .expect("hard link");
        (root, outside, dir)
    }

    // Only called from this file's `#[cfg(unix)]` hard-link tests below (Windows has no
    // equivalent hard-link-escape test here); a bare fn left this "never used" on a
    // non-unix `--all-targets` build (Windows cross-compilation under
    // `-D warnings` found this).
    #[cfg(unix)]
    fn hard_linked_refusal(err: ToolError) -> (String, u64) {
        match err {
            ToolError::Access {
                source: AccessError::HardLinked { path, links },
                ..
            } => (path, links),
            other => panic!("expected a hard-link refusal, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_hard_linked_file_is_refused_for_write_delete_and_read_and_the_outside_file_survives() {
        let (root, outside, dir) = hard_linked_workspace();
        let target = outside.path().join("authorized_keys");

        let (path, links) = hard_linked_refusal(
            write_confined("test", &dir, Path::new("keys"), b"pwned").expect_err("write"),
        );
        assert_eq!((path.as_str(), links), ("keys", 2));
        assert_eq!(
            std::fs::read_to_string(&target).expect("outside"),
            "ssh-ed25519 AAAA",
            "a refused write must not have truncated the linked file either"
        );

        hard_linked_refusal(
            remove_file_confined("test", &dir, Path::new("keys")).expect_err("delete"),
        );
        assert!(root.path().join("keys").exists());

        let (path, _) = hard_linked_refusal(
            read_capped("test", &dir, Path::new("keys"), MAX_READ_BYTES, false).expect_err("read"),
        );
        assert_eq!(path, "keys");
        hard_linked_refusal(
            check_readable("test", &dir, Path::new("keys"), false).expect_err("check"),
        );
        hard_linked_refusal(
            refuse_if_hard_linked("test", &dir, Path::new("keys")).expect_err("plan"),
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_allowed_path_may_read_a_hard_linked_file_but_never_write_it() {
        let (_root, outside, dir) = hard_linked_workspace();
        let allow = AllowedHardlinkReads::new([PathBuf::from("keys")]).expect("valid");
        assert!(allow.permits(Path::new("keys")));
        assert!(allow.permits(Path::new("./keys")));
        assert!(!allow.permits(Path::new("other")));
        let read = read_capped_to_string(
            "test",
            &dir,
            Path::new("keys"),
            MAX_READ_BYTES,
            allow.permits(Path::new("keys")),
        )
        .expect("an allowed read");
        assert_eq!(read, "ssh-ed25519 AAAA");
        hard_linked_refusal(
            write_confined("test", &dir, Path::new("keys"), b"x").expect_err("still no write"),
        );
        assert_eq!(
            std::fs::read_to_string(outside.path().join("authorized_keys")).expect("outside"),
            "ssh-ed25519 AAAA"
        );
    }

    #[test]
    fn the_allowed_list_is_empty_by_default_and_refuses_escaping_entries() {
        assert!(!AllowedHardlinkReads::default().permits(Path::new("keys")));
        assert!(AllowedHardlinkReads::new([PathBuf::from("../x")]).is_err());
        assert!(AllowedHardlinkReads::new([PathBuf::from("/etc/passwd")]).is_err());
        let dir = AllowedHardlinkReads::new([PathBuf::from("vendor/data")]).expect("valid");
        assert!(dir.permits(Path::new("vendor/data/blob.bin")));
        assert!(!dir.permits(Path::new("vendor/database")));
    }

    /// A link inside the workspace is followed before the guard decides: a
    /// read through a link into `.git` or `.kgraf` is refused, a link to an
    /// ordinary file still works, and a link to `.gitmodules` reads but never
    /// writes.
    #[cfg(unix)]
    #[test]
    fn the_guards_follow_links_inside_the_workspace() {
        let (root, dir) = workspace();
        let r = root.path();
        std::fs::create_dir_all(r.join(".git")).expect("git");
        std::fs::write(r.join(".git/config"), "[core]\n").expect("seed");
        std::fs::create_dir_all(r.join(".kgraf")).expect("state");
        std::fs::write(r.join(".kgraf/ledger.db"), "x").expect("seed");
        std::fs::write(r.join(".gitmodules"), "m").expect("seed");
        std::fs::write(r.join("plain.txt"), "plain").expect("seed");
        std::fs::create_dir_all(r.join("a/b")).expect("dirs");
        std::os::unix::fs::symlink(".git", r.join("g")).expect("link");
        std::os::unix::fs::symlink("../../.kgraf", r.join("a/b/k")).expect("link");
        std::os::unix::fs::symlink("../.gitmodules", r.join("a/mods")).expect("link");
        std::os::unix::fs::symlink("../plain.txt", r.join("a/p")).expect("link");
        std::os::unix::fs::symlink("loop2", r.join("loop1")).expect("link");
        std::os::unix::fs::symlink("loop1", r.join("loop2")).expect("link");

        for refused in ["g/config", "a/b/k/ledger.db", "loop1"] {
            assert!(
                matches!(
                    read_capped("t", &dir, Path::new(refused), MAX_READ_BYTES, false),
                    Err(ToolError::Access { .. })
                ),
                "{refused}"
            );
        }
        assert!(matches!(
            open_dir_confined("t", &dir, Path::new("g")),
            Err(ToolError::Access { .. })
        ));
        assert_eq!(
            read_capped_to_string("t", &dir, Path::new("a/p"), MAX_READ_BYTES, false)
                .expect("an ordinary link reads"),
            "plain"
        );
        assert_eq!(
            read_capped_to_string("t", &dir, Path::new("a/mods"), MAX_READ_BYTES, false)
                .expect("repository text reads through a link"),
            "m"
        );
        assert!(matches!(
            write_confined("t", &dir, Path::new("a/mods"), b"evil"),
            Err(ToolError::Access { .. })
        ));
        assert!(matches!(
            write_confined("t", &dir, Path::new("g/hooks/pre-commit"), b"evil"),
            Err(ToolError::Access { .. })
        ));
        write_confined("t", &dir, Path::new("a/p"), b"changed").expect("ordinary write");
        assert_eq!(
            std::fs::read_to_string(r.join("plain.txt")).unwrap(),
            "changed"
        );
        assert_eq!(std::fs::read_to_string(r.join(".gitmodules")).unwrap(), "m");
        assert!(!r.join(".git/hooks").exists());
    }

    #[test]
    fn a_single_link_file_is_rewritten_whole_after_the_link_check() {
        let (_root, dir) = workspace();
        write_confined("test", &dir, Path::new("a.txt"), b"a long first version").expect("w1");
        write_confined("test", &dir, Path::new("a.txt"), b"short").expect("w2");
        assert_eq!(
            read_capped_to_string("test", &dir, Path::new("a.txt"), MAX_READ_BYTES, false)
                .expect("read"),
            "short",
            "the open no longer truncates, so set_len must"
        );
    }
}

//! What a tool call touches, declared before it runs.
//!
//! Two tools touching the same graph node
//! serialize and independent ones overlap. The runtime compares the [`Access`] of
//! each pending call with every running one through [`Access::conflicts_with`].
//!
//! **Node ids are compared by equality only.** Graph node ids are signature-derived
//! hashes (decision 0050), so containment is not visible in the id string. A tool
//! that edits a function and so also changes its file declares both the node and the
//! file path; expanding a node to its ancestors is the runtime's job, through the
//! graph, not something this type can infer.
//!
//! **Paths are relative to the workspace root and may not leave it.** An absolute
//! path, or a `..` that climbs above the root, is refused when declared: two
//! spellings of one file outside the root could not be compared, so the conflict
//! check would pass them as independent.
//!
//! **Paths conflict by component prefix.** A write to `src/` conflicts with a read of
//! `src/lib.rs`.
//!
//! **Case.** On a case-insensitive filesystem (the macOS and Windows defaults),
//! `Src/Lib.rs` and `src/lib.rs` are one file. The runtime detects the workspace's
//! behavior once with [`detect_path_case`] and passes it to
//! [`Access::conflicts_with`]. On a case-sensitive one, names are still
//! compared in Unicode NFC: case-sensitive APFS treats an NFC and an NFD
//! spelling as one file.
//!
//! **Symlinks are not resolved here.** Two paths reaching one file through a
//! symlink compare as different. Resolving them needs the filesystem at call time,
//! which is the sandbox layer's job (`kgraf-sandbox`): it refuses a write
//! through a symlink that leaves the worktree, and it is where a symlinked alias
//! inside the worktree must be canonicalized before access is declared.

use crate::error::AccessError;
use kgraf_core::NodeId;
use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

/// How the workspace's filesystem compares names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathCase {
    Sensitive,
    Insensitive,
}

/// Detect whether `root`'s filesystem is case-insensitive, by creating a probe file
/// with a lowercase name and looking it up in uppercase. The probe is removed.
pub fn detect_path_case(root: &Path) -> std::io::Result<PathCase> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let lower = format!(".kgraf-case-probe-{}-{nanos}", std::process::id());
    let probe = root.join(&lower);
    std::fs::write(&probe, b"")?;
    let upper = root.join(lower.to_uppercase());
    let insensitive = std::fs::symlink_metadata(&upper).is_ok();
    std::fs::remove_file(&probe)?;
    Ok(if insensitive {
        PathCase::Insensitive
    } else {
        PathCase::Sensitive
    })
}

/// The graph nodes and paths one call reads and writes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Access {
    reads_nodes: BTreeSet<NodeId>,
    writes_nodes: BTreeSet<NodeId>,
    reads_paths: BTreeSet<PathBuf>,
    writes_paths: BTreeSet<PathBuf>,
    exclusive: bool,
    /// Decision 0084: an exclusive call whose writes the OS sandbox confines
    /// to the worktree and its scratch directory ([`Access::sandboxed_build`]).
    sandboxed_build: bool,
}

impl Access {
    /// Touches nothing shared: a pure computation. Overlaps with everything except
    /// an exclusive call. A tool must opt into this explicitly.
    pub fn none() -> Self {
        Self::default()
    }

    /// Footprint unknown. Conflicts with every other call, including another
    /// exclusive one. The default for a tool that does not declare its access.
    pub fn exclusive() -> Self {
        Self {
            exclusive: true,
            ..Self::default()
        }
    }

    /// A build or test command on the allowlist, run by a workspace-bound
    /// shell tool (decision 0084). Exclusive for scheduling (its footprint in
    /// the worktree is unknown), but its writes are confined by the OS sandbox
    /// to the worktree and its scratch directory, so a worker with a declared
    /// write scope may run it; the verifier's scope check on the committed
    /// attempt then refuses any change outside the declared writes. Only the
    /// shell tool declares this.
    pub fn sandboxed_build() -> Self {
        Self {
            exclusive: true,
            sandboxed_build: true,
            ..Self::default()
        }
    }

    /// Whether this is [`Access::sandboxed_build`].
    pub fn is_sandboxed_build(&self) -> bool {
        self.sandboxed_build
    }

    /// Add a node this call reads.
    pub fn read_node(mut self, node: impl Into<NodeId>) -> Self {
        self.reads_nodes.insert(node.into());
        self
    }

    /// Add a node this call writes.
    pub fn write_node(mut self, node: impl Into<NodeId>) -> Self {
        self.writes_nodes.insert(node.into());
        self
    }

    /// Add a path this call reads, relative to the workspace root.
    pub fn read_path(mut self, path: impl AsRef<Path>) -> Result<Self, AccessError> {
        self.reads_paths.insert(normalize(path.as_ref())?);
        Ok(self)
    }

    /// Add a path this call writes, relative to the workspace root.
    pub fn write_path(mut self, path: impl AsRef<Path>) -> Result<Self, AccessError> {
        self.writes_paths.insert(normalize(path.as_ref())?);
        Ok(self)
    }

    /// Whether this call's footprint is unknown.
    pub fn is_exclusive(&self) -> bool {
        self.exclusive
    }

    /// Nodes read.
    pub fn reads_nodes(&self) -> &BTreeSet<NodeId> {
        &self.reads_nodes
    }

    /// Nodes written.
    pub fn writes_nodes(&self) -> &BTreeSet<NodeId> {
        &self.writes_nodes
    }

    /// Paths read, normalized.
    pub fn reads_paths(&self) -> &BTreeSet<PathBuf> {
        &self.reads_paths
    }

    /// Paths written, normalized.
    pub fn writes_paths(&self) -> &BTreeSet<PathBuf> {
        &self.writes_paths
    }

    /// Whether the two calls must not run at the same time.
    ///
    /// They conflict when either is exclusive, or when one writes something the
    /// other reads or writes. Two reads never conflict. The relation is symmetric.
    pub fn conflicts_with(&self, other: &Access, case: PathCase) -> bool {
        if self.exclusive || other.exclusive {
            return true;
        }
        self.writes_against(other, case) || other.writes_against(self, case)
    }

    /// Whether `self`'s writes touch anything `other` reads or writes.
    fn writes_against(&self, other: &Access, case: PathCase) -> bool {
        let node_hit = self
            .writes_nodes
            .iter()
            .any(|n| other.reads_nodes.contains(n) || other.writes_nodes.contains(n));
        let path_hit = self.writes_paths.iter().any(|w| {
            other
                .reads_paths
                .iter()
                .chain(other.writes_paths.iter())
                .any(|p| overlaps(w, p, case))
        });
        node_hit || path_hit
    }
}

/// Whether one path contains the other, component-wise.
fn overlaps(a: &Path, b: &Path, case: PathCase) -> bool {
    match case {
        // Case-sensitive APFS still treats an NFC and an NFD spelling of one
        // name as one file, so the comparison normalizes; on a volume that
        // does not (ext4), two such names only serialize, which is safe.
        PathCase::Sensitive => {
            // Two ASCII paths are already in NFC, so the plain component-wise
            // comparison gives the same answer without allocating; a run of
            // calls compares every pair of paths, and nearly all are ASCII.
            if let (Some(x), Some(y)) = (a.to_str(), b.to_str())
                && x.is_ascii()
                && y.is_ascii()
            {
                return a.starts_with(b) || b.starts_with(a);
            }
            let norm = |p: &Path| PathBuf::from(kgraf_core::paths::normalize_path(p));
            let (a, b) = (norm(a), norm(b));
            a.starts_with(&b) || b.starts_with(&a)
        }
        PathCase::Insensitive => {
            // The same fold the write-scope and merge checks use (NFKC, case
            // folding, ignorable code points, trailing dots and spaces), so
            // an NFC and an NFD spelling of one name are one path here too.
            let fold = |p: &Path| PathBuf::from(kgraf_core::paths::fold_path(p));
            let (a, b) = (fold(a), fold(b));
            a.starts_with(&b) || b.starts_with(&a)
        }
    }
}

/// The lexical normalization alone, without joining to a root. `apply_patch`
/// uses this to reject a hunk path that normalizes to the empty path (the
/// workspace root itself, for example `x/..`): a *file* operation naming the
/// whole workspace is nonsensical, even though an empty relative path is a
/// legitimate "the workspace root" for a *directory* read (`list_directory`'s
/// default).
pub(crate) fn normalize_relative(path: &Path) -> Result<PathBuf, AccessError> {
    normalize(path)
}

/// Lexical normalization inside the root: drop `.`, resolve `..` against earlier
/// components, refuse an absolute path or a `..` that would leave the root.
fn normalize(path: &Path) -> Result<PathBuf, AccessError> {
    let shown = || path.to_string_lossy().into_owned();
    let mut out: Vec<&std::ffi::OsStr> = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => out.push(part),
            Component::ParentDir => {
                if out.pop().is_none() {
                    return Err(AccessError::Escapes { path: shown() });
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(AccessError::Absolute { path: shown() });
            }
        }
    }
    Ok(out.iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: PathCase = PathCase::Sensitive;

    fn read(p: &str) -> Access {
        Access::none().read_path(p).expect("valid")
    }

    fn write(p: &str) -> Access {
        Access::none().write_path(p).expect("valid")
    }

    /// The allocation-free comparison of ASCII paths agrees with the normalizing
    /// one on every pair, and a non-ASCII pair still normalizes.
    #[test]
    fn the_ascii_fast_path_agrees_with_the_normalizing_comparison() {
        let paths = [
            "a",
            "a/b",
            "a/b/c.rs",
            "ab",
            "a/bc",
            "b",
            "src/lib.rs",
            "src",
            "",
        ];
        let normalizing = |a: &Path, b: &Path| {
            let norm = |p: &Path| PathBuf::from(kgraf_core::paths::normalize_path(p));
            let (a, b) = (norm(a), norm(b));
            a.starts_with(&b) || b.starts_with(&a)
        };
        for x in paths {
            for y in paths {
                let (x, y) = (Path::new(x), Path::new(y));
                assert_eq!(overlaps(x, y, S), normalizing(x, y), "{x:?} {y:?}");
            }
        }
        // NFC `e` with an acute accent against its NFD spelling: one file.
        assert!(overlaps(
            Path::new("caf\u{e9}/a"),
            Path::new("cafe\u{301}"),
            S
        ));
    }

    #[test]
    fn two_writes_to_the_same_node_serialize() {
        let a = Access::none().write_node("n1");
        let b = Access::none().write_node("n1");
        assert!(a.conflicts_with(&b, S));
    }

    #[test]
    fn a_write_and_a_read_of_the_same_node_serialize_in_either_order() {
        let w = Access::none().write_node("n1");
        let r = Access::none().read_node("n1");
        assert!(w.conflicts_with(&r, S));
        assert!(r.conflicts_with(&w, S));
    }

    #[test]
    fn two_reads_overlap() {
        let a = read("src/lib.rs").read_node("n1");
        let b = read("src/lib.rs").read_node("n1");
        assert!(!a.conflicts_with(&b, S));
    }

    #[test]
    fn independent_writes_overlap() {
        let a = write("src/a.rs").write_node("n1");
        let b = write("src/b.rs").write_node("n2");
        assert!(!a.conflicts_with(&b, S));
    }

    #[test]
    fn a_directory_write_conflicts_with_a_file_inside_it() {
        assert!(write("src").conflicts_with(&read("src/deep/lib.rs"), S));
        // Component-wise, not string prefix: `src2` is not inside `src`.
        assert!(!write("src").conflicts_with(&read("src2/lib.rs"), S));
    }

    #[test]
    fn paths_are_normalized_before_comparison() {
        let a = write("./src/x/../lib.rs");
        assert!(a.conflicts_with(&read("src/lib.rs"), S));
        assert!(a.writes_paths().contains(Path::new("src/lib.rs")));
    }

    #[test]
    fn absolute_and_escaping_paths_are_refused() {
        assert!(matches!(
            Access::none().write_path("/etc/passwd"),
            Err(AccessError::Absolute { .. })
        ));
        assert!(matches!(
            Access::none().write_path("../outside"),
            Err(AccessError::Escapes { .. })
        ));
        assert!(matches!(
            Access::none().read_path("src/../../outside"),
            Err(AccessError::Escapes { .. })
        ));
        // Climbing and coming back inside is fine.
        assert!(Access::none().read_path("src/../lib.rs").is_ok());
    }

    #[test]
    fn case_differences_conflict_only_on_a_case_insensitive_filesystem() {
        let a = write("Src/Lib.rs");
        let b = read("src/lib.rs");
        assert!(!a.conflicts_with(&b, PathCase::Sensitive));
        assert!(a.conflicts_with(&b, PathCase::Insensitive));
    }

    /// On a folding volume, spellings the file system resolves to one name
    /// overlap: NFC and NFD, the long s, a trailing dot, a zero-width joiner.
    /// A case-sensitive volume keeps them apart, except NFC and NFD, which
    /// case-sensitive APFS still resolves to one name.
    #[test]
    fn folded_spellings_conflict_on_a_folding_filesystem() {
        let (nfc, nfd) = (write("caf\u{e9}/a.rs"), write("cafe\u{301}/a.rs"));
        assert!(nfc.conflicts_with(&nfd, PathCase::Insensitive));
        assert!(nfc.conflicts_with(&nfd, PathCase::Sensitive));
        for (x, y) in [
            ("sample.rs", "\u{17F}ample.rs"),
            ("a/b.rs", "a./b.rs"),
            ("src/lib.rs", "src/li\u{200B}b.rs"),
            ("Dir", "dir/inner.rs"),
        ] {
            let (a, b) = (write(x), write(y));
            assert!(a.conflicts_with(&b, PathCase::Insensitive), "{x:?} {y:?}");
            assert!(!a.conflicts_with(&b, PathCase::Sensitive), "{x:?} {y:?}");
        }
        assert!(!write("a/b.rs").conflicts_with(&write("a/c.rs"), PathCase::Insensitive));
    }

    #[test]
    fn case_detection_matches_the_filesystem() {
        let dir = std::env::temp_dir().join(format!("kgraf-case-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let detected = detect_path_case(&dir).expect("detect");
        std::fs::write(dir.join("probe-file"), b"").expect("write");
        let upper_exists = dir.join("PROBE-FILE").exists();
        std::fs::remove_dir_all(&dir).expect("cleanup");
        assert_eq!(detected == PathCase::Insensitive, upper_exists);
    }

    #[test]
    fn an_exclusive_call_conflicts_with_everything_even_a_pure_one() {
        let ex = Access::exclusive();
        assert!(ex.conflicts_with(&Access::none(), S));
        assert!(Access::none().conflicts_with(&ex, S));
        assert!(ex.conflicts_with(&Access::exclusive(), S));
        assert!(!Access::none().conflicts_with(&Access::none(), S));
    }
}

//! Gitignore-aware corpus traversal.
//!
//! Uses the `ignore` crate (the walker ripgrep ships) rather than
//! `code2prompt-core`. `code2prompt-core` wraps this same walker plus git2,
//! handlebars, tiktoken-rs, rayon, csv and colored for its own CLI
//! prompt-templating job; none of that is wanted here, and pulling it in
//! would mean carrying five-plus transitive dependencies for one function
//! call. `ignore` gives the exact capability the spec asks for -- respecting
//! `.gitignore`, `.ignore` and hidden-file conventions -- with nothing else
//! attached (reuse existing code).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use ignore::WalkBuilder;

use crate::error::IngestError;

/// Walk `root`, returning every regular file the gitignore rules admit, in a
/// deterministic (sorted) order so a repeated ingest of an unchanged tree
/// produces the same document order.
///
/// Directories, symlinks-to-directories and anything `.gitignore`/`.ignore`
/// excludes are not returned at all; they are not "failures" because they
/// were never meant to be read; the standing rule is about files a document
/// walker is asked to parse and then drops, not about respecting an ignore
/// file the user wrote on purpose.
#[cfg(test)]
pub fn walk_corpus(root: &Path) -> Result<Vec<PathBuf>, IngestError> {
    walk_corpus_excluding(root, &BTreeSet::new())
}

/// [`walk_corpus`], never entering a directory at a root-relative path in
/// `dir_paths` (forward slashes).
pub fn walk_corpus_excluding(
    root: &Path,
    dir_paths: &BTreeSet<String>,
) -> Result<Vec<PathBuf>, IngestError> {
    std::fs::metadata(root).map_err(|source| IngestError::RootUnreadable {
        path: root.to_path_buf(),
        source,
    })?;

    let mut files = Vec::new();
    let mut builder = WalkBuilder::new(root);
    // `ignore` only honors `.gitignore` inside an actual git repository by
    // default (it looks for `.git`). A document corpus is frequently not a
    // git checkout at all, and the spec asks for gitignore-aware walking
    // unconditionally, so that requirement is turned off here.
    builder.require_git(false);
    if !dir_paths.is_empty() {
        let (paths, base) = (dir_paths.clone(), root.to_path_buf());
        builder.filter_entry(move |entry| {
            if !entry.file_type().is_some_and(|t| t.is_dir()) || entry.depth() == 0 {
                return true;
            }
            let relative = entry
                .path()
                .strip_prefix(&base)
                .unwrap_or(entry.path())
                .to_string_lossy()
                .replace('\\', "/");
            !paths.contains(&relative)
        });
    }
    for entry in builder.build() {
        let entry = entry.map_err(|source| IngestError::Walk {
            path: root.to_path_buf(),
            source,
        })?;
        let is_file = entry
            .file_type()
            .map(|file_type| file_type.is_file())
            .unwrap_or(false);
        if is_file {
            files.push(entry.into_path());
        }
    }
    files.sort();
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walk_finds_files_and_skips_gitignored_ones() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(".gitignore"), "ignored.txt\n").expect("write gitignore");
        std::fs::write(dir.path().join("kept.txt"), "kept").expect("write kept");
        std::fs::write(dir.path().join("ignored.txt"), "ignored").expect("write ignored");

        let files = walk_corpus(dir.path()).expect("walk");
        let names: Vec<_> = files
            .iter()
            .filter_map(|p| p.file_name().and_then(|n| n.to_str()))
            .collect();

        assert!(names.contains(&"kept.txt"));
        assert!(!names.contains(&"ignored.txt"));
        // Dotfiles, including `.gitignore` itself, are hidden by the
        // walker's default hidden-file convention; the corpus this crate
        // reads is prose and code, not repository plumbing.
        assert!(!names.contains(&".gitignore"));
    }

    #[test]
    fn walk_reports_root_unreadable_rather_than_an_empty_result() {
        let missing = Path::new("/does/not/exist/kgraf-ingest-test");
        let err = walk_corpus(missing).expect_err("missing root must error");
        assert!(matches!(err, IngestError::RootUnreadable { .. }));
    }
}

//! Which paths a worker may never write, and path comparison as the file system
//! sees it.
//!
//! A name is compared the way the most permissive common file system resolves
//! it, because the check must hold on every volume a worktree can live on:
//!
//! - **case-insensitively**: APFS and NTFS by default, so `.GIT` is `.git`;
//! - **Unicode-normalized and compatibility-folded** (NFKC, then case folding):
//!   APFS is normalization-insensitive and folds the long s (U+017F) to `s` and
//!   the ligatures U+FB05 and U+FB06 to `st`;
//! - **ignoring default-ignorable code points** (zero-width joiners and the like),
//!   which HFS+ drops when it resolves a name (git's CVE-2014-9390 class);
//! - **ignoring trailing dots and spaces**, which NTFS drops (`.git.` is `.git`),
//!   and matching the NTFS short names `GIT~1` and `KGRAF~1`.
//!
//! - **ignoring NTFS alternate data streams**: `.git::$INDEX_ALLOCATION` is
//!   the directory `.git`, so everything from the first `:` on is dropped.
//!
//! A component that folds to `.kgraf`, to `.haustoria` (the state directory
//! under kGraf's former name, decision 0062), to a short name of either, or to
//! a name that steers git is refused. Those are `.git` itself, `.gitmodules`,
//! `.gitattributes`, `.gitconfig` (a repository can include it from its
//! config), the `.githooks` hooks directory, a `.git~` name, and the
//! NTFS short names `GIT~N`, `GITMOD~N` and `GITATT~N`. Other dot files that
//! only start with `.git` (`.gitlab-ci.yml`, `.gitpod.yml`,
//! `.git-blame-ignore-revs`, `.gitleaks.toml`, `.gitignore`, `.gitkeep`) and
//! the `.github` folder are ordinary files that git never reads as
//! configuration, so they are writable. The runtime refuses such a write
//! ([`forbidden_path`], with the reason from [`forbidden_reason`]), and
//! `kgraf-sandbox`'s merge-back refuses an attempt that touches such a path
//! through [`is_protected_name`] itself, so the two apply one rule. It lives in
//! this crate so merge-back can call it without depending on the runtime.
//!
//! Written new: git's own `is_ntfs_dotgit`/`is_hfs_dotgit` are C in a GPL project,
//! not a reuse candidate; the rule here is stated, not ported.

use std::path::{Component, Path};

/// The migration lock beside the state directory (decision 0062), as
/// `kgraf_host::migrate::MIGRATION_LOCK` names it; protected like the
/// directory itself.
pub const MIGRATION_LOCK_NAME: &str = ".kgraf.migrating";
use unicode_normalization::UnicodeNormalization;

/// Whether `c` is a default-ignorable code point a file system may drop.
fn ignorable(c: char) -> bool {
    matches!(c,
        '\u{00AD}'
        | '\u{034F}'
        | '\u{061C}'
        | '\u{115F}'..='\u{1160}'
        | '\u{17B4}'..='\u{17B5}'
        | '\u{180B}'..='\u{180F}'
        | '\u{200B}'..='\u{200F}'
        | '\u{202A}'..='\u{202E}'
        | '\u{2060}'..='\u{206F}'
        | '\u{3164}'
        | '\u{FE00}'..='\u{FE0F}'
        | '\u{FEFF}'
        | '\u{FFA0}'
        | '\u{FFF0}'..='\u{FFF8}'
        | '\u{1BCA0}'..='\u{1BCA3}'
        | '\u{1D173}'..='\u{1D17A}'
        | '\u{E0000}'..='\u{E0FFF}')
}

/// The characters full Unicode case folding maps that `to_lowercase` leaves
/// alone (`ß` folds to `ss`, a final sigma to sigma, the long s and the
/// ligatures are handled by NFKC).
fn case_fold_extra(c: char, out: &mut String) {
    match c {
        '\u{00DF}' | '\u{1E9E}' => out.push_str("ss"),
        '\u{03C2}' => out.push('\u{03C3}'),
        '\u{0131}' => out.push('i'),
        _ => out.push(c),
    }
}

/// One name as the file system resolves it: NFKC (which turns the long s
/// `ſ` into `s` and the ligatures `ﬅ` and `ﬆ` into `st`, as APFS does),
/// ignorable code points removed, lowercase with the extra case foldings,
/// NFKC again (lowercasing can produce a decomposable form), trailing dots
/// and spaces removed. Stricter than plain lowercasing: two names that fold
/// equal are one file on at least one common volume.
pub fn fold_name(name: &str) -> String {
    let first: String = name.nfkc().filter(|c| !ignorable(*c)).collect();
    let mut lowered = String::with_capacity(first.len());
    for c in first.to_lowercase().chars() {
        case_fold_extra(c, &mut lowered);
    }
    let folded: String = lowered.nfkc().filter(|c| !ignorable(*c)).collect();
    folded.trim_end_matches(['.', ' ']).to_owned()
}

/// A whole relative path folded component by component.
pub fn fold_path(path: &Path) -> String {
    path.components()
        .map(|c| fold_name(&c.as_os_str().to_string_lossy()))
        .collect::<Vec<_>>()
        .join("/")
}

/// Whether two relative paths name one file on a case- and
/// normalization-insensitive file system.
pub fn same_path(a: &Path, b: &Path) -> bool {
    fold_path(a) == fold_path(b)
}

/// Whether `f` is `stem` plus `~` and digits: an NTFS short name of that stem.
fn short_name_of(f: &str, stem: &str) -> bool {
    f.strip_prefix(stem)
        .and_then(|rest| rest.strip_prefix('~'))
        .is_some_and(|digits| !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()))
}

/// Why one path component is protected, or `None`. The reason names what the
/// name is, so a refusal can say why it was refused.
pub fn protected_name_reason(name: &str) -> Option<&'static str> {
    let folded = fold_name(name);
    // An NTFS stream suffix names the same file or directory
    // (`.git::$INDEX_ALLOCATION`); the part before the first `:` is the name.
    let f = folded
        .split(':')
        .next()
        .unwrap_or_default()
        .trim_end_matches(['.', ' '])
        .to_owned();
    let f = f.as_str();
    if is_kgraf_state_name(f) {
        return Some("it is kGraf's own state, which a worker never writes");
    }
    if f == ".git"
        || matches!(
            f,
            ".gitmodules" | ".gitattributes" | ".gitconfig" | ".githooks"
        )
        || f.starts_with(".git~")
        || short_name_of(f, "git")
        || short_name_of(f, "gitmod")
        || short_name_of(f, "gitatt")
    {
        return Some(
            "it is repository metadata that steers git (`.git`, `.gitmodules`, `.gitattributes`, `.gitconfig`, \
             a hooks directory, or an NTFS short name of one)",
        );
    }
    None
}

/// Whether one path component may not even be read by a worker tool: the
/// `.git` directory itself (its config, hooks and objects) and kGraf's own
/// state. Repository files such as `.gitmodules` and `.gitattributes` are
/// ordinary project text a worker may read; only writing them is refused
/// (see [`is_protected_name`]).
pub fn is_unreadable_name(name: &str) -> bool {
    let folded = fold_name(name);
    let f = folded
        .split(':')
        .next()
        .unwrap_or_default()
        .trim_end_matches(['.', ' '])
        .to_owned();
    let f = f.as_str();
    is_kgraf_state_name(f) || f == ".git" || f.starts_with(".git~") || short_name_of(f, "git")
}

/// Whether one path component names repository metadata or kGraf's state.
pub fn is_protected_name(name: &str) -> bool {
    protected_name_reason(name).is_some()
}

fn is_kgraf_state_name(f: &str) -> bool {
    // `KGRAF~1` is the NTFS short name of `.kgraf`. The former name's
    // directory stays protected too: a project not yet migrated still has it.
    // `HAUST~1` was matched before the rename; `HAUSTO~1` is the short name
    // NTFS actually generates (leading dot dropped, six characters, `~1`).
    // `.kgraf.migrating` is the state directory's migration lock and pending
    // mark (kgraf-host's `migrate`); a planted one could make the next start
    // resume a move that never happened. Its NTFS short name is `KGRAF~N.MIG`.
    f == crate::legacy::STATE_DIR
        || f == MIGRATION_LOCK_NAME
        || (f.starts_with("kgraf~") && f.ends_with(".mig"))
        || f == "kgraf~1"
        || f == crate::legacy::LEGACY_STATE_DIR
        || f == "hausto~1"
        || f == "haust~1"
}

/// Whether `path` is cargo's or rustup's configuration: a `.cargo` folder
/// (or anything in one) or a `rust-toolchain` file, at any depth. Cargo reads
/// `.cargo/config.toml` in the folder it runs in and every parent, and rustup
/// reads `rust-toolchain(.toml)` the same way; either can name programs that
/// every later build runs (a `build.rustc-wrapper`, a `path` toolchain, an
/// `[alias]`). Compared as [`fold_name`] resolves names; `CARGO~1` is the
/// NTFS short name of `.cargo`.
pub fn is_build_config_path(path: &Path) -> bool {
    path.components().any(|c| match c {
        Component::Normal(name) => {
            let f = fold_name(&name.to_string_lossy());
            // `RUST-T~1.TOM` is the NTFS short name of `rust-toolchain.toml`
            // (`RUST-T~1` for the extensionless file).
            f == ".cargo"
                || f == "cargo~1"
                || f.starts_with("rust-toolchain")
                || f.starts_with("rust-t~")
        }
        _ => false,
    })
}

/// Whether a worker may never write `path`: absolute, escaping (`..`), or
/// through a protected component.
///
/// Lexical only: it never looks at the disk, so it cannot see a symbolic link
/// (`gen -> ../../outside` passes as `gen/x.rs`). A host-side write into a
/// tree must also go through `kgraf_sandbox::write_confined` (or its append
/// and remove siblings), which refuse a link at any component (decision 0083).
pub fn forbidden_path(path: &Path) -> bool {
    forbidden_reason(path).is_some()
}

/// Why a worker may never write `path`, or `None` when it may. The text
/// The text follows "`<path>` is not writable by a worker: ".
pub fn forbidden_reason(path: &Path) -> Option<String> {
    if path.as_os_str().is_empty() {
        return Some("the path is empty".to_owned());
    }
    path.components().find_map(|c| match c {
        Component::Normal(name) => protected_name_reason(&name.to_string_lossy())
            .map(|why| format!("the component `{}`: {why}", name.to_string_lossy())),
        Component::CurDir => None,
        Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
            Some("the path is absolute or leaves the worktree".to_owned())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_metadata_is_refused_however_it_is_spelled() {
        for bad in [
            ".git/config",
            ".GIT/config",
            ".Git/HEAD",
            "src/.GIT",
            ".git\u{200C}/config",
            ".g\u{FEFF}it/hooks/pre-commit",
            ".git./config",
            ".git /config",
            "GIT~1/config",
            ".gitattributes",
            ".GITMODULES",
            ".githooks/pre-commit",
            "a/.gitconfig",
            "GITMOD~1",
            "gitmod~2",
            "GITATT~1",
            "sub/GITATT~1",
            ".git~1/x",
            ".git::$INDEX_ALLOCATION/config",
            ".git:$I30:$INDEX_ALLOCATION/config",
            ".GIT::$index_allocation",
            "GIT~1::$INDEX_ALLOCATION/x",
            ".gitmodules::$DATA",
            ".git .::$INDEX_ALLOCATION",
            ".g\u{200B}itmodules",
            ".gitmodule\u{17F}",
            ".kgraf/ledger.db",
            ".KGRAF/x",
            "KGRAF~1/x",
            ".haustoria/ledger.db",
            ".kgraf.migrating",
            ".KGRAF.Migrating",
            "sub/.kgraf.migrating.",
            "KGRAF~1.MIG",
            "KGRAF~2.MIG",
            ".Haustoria/x",
            "HAUSTO~1/x",
            "HAUST~1/x",
            "../x",
            "/etc/passwd",
            "",
        ] {
            assert!(forbidden_path(Path::new(bad)), "{bad:?}");
        }
        for ok in [
            "src/lib.rs",
            ".gitignore",
            ".GITIGNORE",
            ".github/workflows/ci.yml",
            ".gitlab-ci.yml",
            ".gitpod.yml",
            ".git-blame-ignore-revs",
            ".gitleaks.toml",
            ".GITLEAKS.toml",
            ".github",
            "docs/git.md",
            "a..b.rs",
            "./src/x.rs",
        ] {
            assert!(!forbidden_path(Path::new(ok)), "{ok:?}");
        }
    }

    /// A refusal says what the name is, so the worker can tell a protected
    /// name from a path that leaves the worktree.
    #[test]
    fn a_refusal_states_its_reason() {
        let git = forbidden_reason(Path::new("src/.git/config")).expect("refused");
        assert!(git.contains(".git") && git.contains("steers git"), "{git}");
        let state = forbidden_reason(Path::new(".kgraf/x")).expect("refused");
        assert!(state.contains("kGraf's own state"), "{state}");
        let escape = forbidden_reason(Path::new("../x")).expect("refused");
        assert!(escape.contains("leaves the worktree"), "{escape}");
        assert!(forbidden_reason(Path::new(".gitlab-ci.yml")).is_none());
    }

    /// APFS folds the long s and the st ligatures; the fold must too, or a
    /// protected name is reachable by a spelling the volume resolves to it.
    #[test]
    fn compatibility_spellings_fold_as_the_volume_folds_them() {
        assert_eq!(fold_name("ru\u{17F}t"), "rust");
        assert_eq!(fold_name("ru\u{FB05}"), "rust");
        assert_eq!(fold_name("ru\u{FB06}"), "rust");
        assert_eq!(fold_name("Stra\u{DF}e"), "strasse");
        assert!(forbidden_path(Path::new(".hau\u{17F}toria/ledger.db")));
        assert!(forbidden_path(Path::new(".hau\u{FB05}oria/x")));
        assert!(forbidden_path(Path::new(".hau\u{FB06}oria/x")));
        assert!(is_protected_name(".hau\u{17F}toria"));
        assert!(same_path(
            Path::new("ru\u{17F}t/a.rs"),
            Path::new("rust/a.rs")
        ));
        for yes in [
            "ru\u{17F}t-toolchain.toml",
            "ru\u{FB05}-toolchain",
            "ru\u{FB06}-toolchain.toml",
            "a/RU\u{17F}T-toolchain",
            "RUST-T~1.TOM",
            "crates/a/rust-t~1",
        ] {
            assert!(is_build_config_path(Path::new(yes)), "{yes:?}");
        }
        // Fullwidth letters fold too. `.cargo` and `.kgraf` hold no long s
        // or ligature, so compatibility letters are what could reach them.
        assert!(is_build_config_path(Path::new(
            "\u{FF0E}\u{FF43}\u{FF41}\u{FF52}\u{FF47}\u{FF4F}/config.toml"
        )));
        assert!(forbidden_path(Path::new(
            "\u{FF0E}\u{FF4B}\u{FF47}\u{FF52}\u{FF41}\u{FF46}/x"
        )));
        assert!(forbidden_path(Path::new(".KGRAF/x")));
        assert!(is_build_config_path(Path::new(".CARGO/config.toml")));
    }

    #[test]
    fn paths_compare_as_a_case_and_normalization_insensitive_volume_does() {
        assert!(same_path(Path::new("Src/Lib.RS"), Path::new("src/lib.rs")));
        // "é" precomposed and decomposed.
        assert!(same_path(
            Path::new("caf\u{e9}.rs"),
            Path::new("cafe\u{301}.rs")
        ));
        assert!(!same_path(Path::new("src/a.rs"), Path::new("src/b.rs")));
    }

    #[test]
    fn cargo_and_rustup_configuration_is_found_at_any_depth_however_spelled() {
        for yes in [
            ".cargo/config.toml",
            ".cargo/config",
            ".cargo",
            ".CARGO/config.toml",
            ".cargo./config.toml",
            ".car\u{200B}go/config.toml",
            "CARGO~1/config.toml",
            "crates/a/.cargo/config.toml",
            ".cargo/credentials.toml",
            "rust-toolchain",
            "rust-toolchain.toml",
            "crates/a/Rust-Toolchain.TOML",
        ] {
            assert!(is_build_config_path(Path::new(yes)), "{yes:?}");
        }
        for no in [
            "Cargo.toml",
            "Cargo.lock",
            "src/cargo.rs",
            "docs/toolchain-notes.md",
            "cargo/config.toml",
            "src/toolchain.rs",
        ] {
            assert!(!is_build_config_path(Path::new(no)), "{no:?}");
        }
    }
}

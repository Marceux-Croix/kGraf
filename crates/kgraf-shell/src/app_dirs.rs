//! The shell's per-user directories under kGraf's former name (decision 0062).
//!
//! Settings (`settings.toml`) and the log file live in the platform
//! directories `directories::ProjectDirs` names for an application. Under the
//! former name those were `Haustoria` (macOS, Windows) or `haustoria` (Linux);
//! now they are `kGraf` / `kgraf`. [`migrate`] moves each one that exists only
//! under the former name, once, before anything reads settings or opens the
//! log, and returns one notice per move.
//!
//! Where a directory exists under both names, nothing is moved and the notice
//! says so: the new one is used and the old one is left for the user to check
//! and remove. That is loud without refusing to start the window.
//!
//! On macOS the bundle identifier changed with the name, which moves nothing
//! here: the shell is not sandboxed, so it has no container, and it keeps no
//! state in `NSUserDefaults`.

use std::path::{Path, PathBuf};

/// The application name `directories::ProjectDirs` is asked for.
pub const APP_NAME: &str = "kGraf";

/// The same, under the former name.
pub const LEGACY_APP_NAME: &str = "Haustoria";

/// What happened to one directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirOutcome {
    /// Moved from the former name.
    Moved { from: PathBuf, to: PathBuf },
    /// Both exist: the new one is used, the old one left as it was.
    BothExist { old: PathBuf, new: PathBuf },
}

impl DirOutcome {
    /// The notice for this outcome.
    pub fn notice(&self) -> String {
        match self {
            DirOutcome::Moved { from, to } => format!(
                "kGraf: moved {} to {} (Haustoria is kGraf's former name, decision 0062).",
                from.display(),
                to.display()
            ),
            DirOutcome::BothExist { old, new } => format!(
                "warning: both {} and {} exist; kGraf uses {} and ignores {} (the directory \
                 under kGraf's former name, Haustoria). Move anything you need from it, then \
                 remove it.",
                old.display(),
                new.display(),
                new.display(),
                old.display()
            ),
        }
    }
}

/// A move that failed.
#[derive(Debug, thiserror::Error)]
#[error("cannot move {from} to {to}: {source}")]
pub struct MoveError {
    pub from: PathBuf,
    pub to: PathBuf,
    #[source]
    pub source: std::io::Error,
}

/// Move this user's shell directories from the former name. Empty when there
/// is no home directory or nothing to move.
pub fn migrate() -> Result<Vec<DirOutcome>, MoveError> {
    let (Some(old), Some(new)) = (
        directories::ProjectDirs::from("", "", LEGACY_APP_NAME),
        directories::ProjectDirs::from("", "", APP_NAME),
    ) else {
        return Ok(Vec::new());
    };
    let kinds = |d: &directories::ProjectDirs| {
        vec![
            d.config_dir().to_path_buf(),
            d.data_dir().to_path_buf(),
            d.data_local_dir().to_path_buf(),
            d.cache_dir().to_path_buf(),
            d.preference_dir().to_path_buf(),
        ]
    };
    let pairs: Vec<(PathBuf, PathBuf)> = kinds(&old).into_iter().zip(kinds(&new)).collect();
    move_dirs(&pairs)
}

/// The application-named ancestors of `old` and `new`: the closest pair
/// whose names differ. Windows puts `config` and `data` under the
/// application folder, and moving the folder moves both, leaving nothing
/// empty behind.
fn named_ancestors(old: &Path, new: &Path) -> Option<(PathBuf, PathBuf)> {
    let (mut o, mut n) = (old, new);
    loop {
        if o.file_name() != n.file_name() {
            return Some((o.to_path_buf(), n.to_path_buf()));
        }
        o = o.parent()?;
        n = n.parent()?;
    }
}

/// Move each `(old, new)` pair that exists only under the old name. Pure over
/// the file system, so tested with temporary directories.
pub fn move_dirs(pairs: &[(PathBuf, PathBuf)]) -> Result<Vec<DirOutcome>, MoveError> {
    let mut seen: Vec<(PathBuf, PathBuf)> = Vec::new();
    for (old, new) in pairs {
        if let Some(pair) = named_ancestors(old, new)
            && !seen.contains(&pair)
        {
            seen.push(pair);
        }
    }
    let mut outcomes = Vec::new();
    for (old, new) in seen {
        if !old.is_dir() {
            continue;
        }
        if new.exists() {
            outcomes.push(DirOutcome::BothExist { old, new });
            continue;
        }
        if move_one(&old, &new)? {
            outcomes.push(DirOutcome::Moved { from: old, to: new });
        }
    }
    Ok(outcomes)
}

/// Rename `old` to `new`. `Ok(false)` when the rename failed because a
/// concurrent first launch already made it (the old directory is gone and
/// the new one exists): that launch printed the notice (L6).
fn move_one(old: &Path, new: &Path) -> Result<bool, MoveError> {
    let fail = |source| MoveError {
        from: old.to_path_buf(),
        to: new.to_path_buf(),
        source,
    };
    if let Some(parent) = new.parent() {
        std::fs::create_dir_all(parent).map_err(fail)?;
    }
    match std::fs::rename(old, new) {
        Ok(()) => Ok(true),
        Err(_) if !old.exists() && new.is_dir() => Ok(false),
        Err(e) => Err(fail(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A first launch that loses the rename to a concurrent one
    /// (the old directory already gone, the new one there) is not an error.
    #[test]
    fn a_rename_lost_to_a_concurrent_launch_is_success() {
        let home = tempfile::tempdir().expect("home");
        let old = home.path().join("Haustoria");
        let new = home.path().join("kGraf");
        std::fs::create_dir_all(&new).expect("the other launch moved it");
        assert!(!move_one(&old, &new).expect("not an error"));
        // Any other failure is still an error: old gone, new missing.
        let other = home.path().join("missing-new");
        assert!(move_one(&old, &other).is_err());
    }

    #[test]
    fn a_directory_under_the_former_name_moves_with_its_settings() {
        let home = tempfile::tempdir().expect("home");
        let old = home.path().join("Haustoria/config");
        std::fs::create_dir_all(&old).expect("old");
        std::fs::write(old.join("settings.toml"), "compaction = 70\n").expect("write");
        let new = home.path().join("kGraf/config");
        let data_old = home.path().join("Haustoria/data");
        let data_new = home.path().join("kGraf/data");

        let outcomes =
            move_dirs(&[(old.clone(), new.clone()), (data_old, data_new)]).expect("moved");
        assert_eq!(
            outcomes,
            vec![DirOutcome::Moved {
                from: home.path().join("Haustoria"),
                to: home.path().join("kGraf"),
            }],
            "the application folder moves once, for both kinds"
        );
        assert_eq!(
            std::fs::read_to_string(new.join("settings.toml")).expect("read"),
            "compaction = 70\n"
        );
        assert!(!home.path().join("Haustoria").exists());
        assert!(outcomes[0].notice().contains("former name"));
        // Once only.
        assert_eq!(move_dirs(&[(old, new)]).expect("again"), Vec::new());
    }

    #[test]
    fn both_present_keeps_the_new_one_and_says_so() {
        let home = tempfile::tempdir().expect("home");
        let old = home.path().join("haustoria");
        let new = home.path().join("kgraf");
        std::fs::create_dir_all(&old).expect("old");
        std::fs::create_dir_all(&new).expect("new");
        let outcomes = move_dirs(&[(old.clone(), new.clone())]).expect("ok");
        assert_eq!(
            outcomes,
            vec![DirOutcome::BothExist {
                old: old.clone(),
                new
            }]
        );
        assert!(old.is_dir(), "left as it was");
        assert!(outcomes[0].notice().contains("ignores"));
    }
}

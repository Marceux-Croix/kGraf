//! The one work folder a run creates, its removal, and the check that
//! nothing is left.
//!
//! Everything a run makes lives under `<temp>\kgraf-wintest-<run id>`:
//! the source mirror the test binaries read their fixtures from, the test
//! processes' `TEMP`, kGraf's diagnostics, settings and permission files, the
//! end-to-end repositories. The only thing outside it is the drive letter
//! that maps the mirror (see `mirror_drive`), which is removed first.
//!
//! Removal never follows a junction or a symbolic link (`std::fs::remove_dir_all`
//! removes the link, not its target), and refuses any path that is not a
//! work folder this module named. The platform calls go through [`Ops`], so
//! the order, the retry and the verification are tested with a fake.

use std::path::{Path, PathBuf};

use serde::Serialize;

pub const PREFIX: &str = "kgraf-wintest-";

/// The folders inside one work folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    pub root: PathBuf,
    /// The mirror's root: the drive letter maps here.
    pub mirror: PathBuf,
    /// `TEMP` and `TMP` of every child process.
    pub tmp: PathBuf,
    /// kGraf's per-user files for this run (diagnostics, settings, permissions).
    pub kgraf_home: PathBuf,
    /// End-to-end repositories and the sandbox-run folder.
    pub e2e: PathBuf,
    /// Captured output of each step.
    pub logs: PathBuf,
}

/// A run id from the start time: `20261007-153012-<pid>`.
pub fn run_id(utc_compact: &str, pid: u32) -> String {
    format!("{utc_compact}-{pid}")
}

pub fn layout(base: &Path, id: &str) -> Layout {
    let root = base.join(format!("{PREFIX}{id}"));
    Layout {
        mirror: root.join("m"),
        tmp: root.join("t"),
        kgraf_home: root.join("k"),
        e2e: root.join("e"),
        logs: root.join("l"),
        root,
    }
}

/// True only for a folder this module would have named: a direct child of
/// `base` whose name starts with [`PREFIX`] and has an id after it.
pub fn is_work_folder(path: &Path, base: &Path) -> bool {
    let named = path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.len() > PREFIX.len() && n.starts_with(PREFIX));
    named && path.parent() == Some(base)
}

/// Something the run made outside plain files in the work folder.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Made {
    /// The work folder itself.
    Folder { path: String },
    /// A `subst` drive letter that maps the mirror.
    Drive { letter: char, target: String },
}

/// What the platform does for a cleanup. The real one is in `win.rs`.
pub trait Ops {
    fn remove_tree(&self, path: &Path) -> std::io::Result<()>;
    /// Reset the permissions of everything under `path` to inherited ones
    /// (the sandbox adds entries to its writable roots); links not followed.
    fn reset_acl(&self, path: &Path) -> std::io::Result<()>;
    fn remove_drive(&self, letter: char) -> std::io::Result<()>;
    fn drive_mapped(&self, letter: char) -> bool;
    fn exists(&self, path: &Path) -> bool;
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Cleanup {
    pub created: Vec<Made>,
    pub removed: Vec<String>,
    /// What is still there after the cleanup, with the reason.
    pub left: Vec<String>,
    /// True when the check after the removal found nothing left.
    pub verified: bool,
    pub notes: Vec<String>,
}

/// Remove what was made, newest first (the drive before the folder it maps),
/// retry a folder once after resetting its permissions, then check that
/// every item is gone.
pub fn clean_up(made: &[Made], base: &Path, ops: &impl Ops) -> Cleanup {
    let mut out = Cleanup {
        created: made.to_vec(),
        ..Cleanup::default()
    };
    for item in made.iter().rev() {
        match item {
            Made::Drive { letter, .. } => match ops.remove_drive(*letter) {
                Ok(()) => out.removed.push(format!("drive {letter}:")),
                Err(e) => out.notes.push(format!("drive {letter}: not removed: {e}")),
            },
            Made::Folder { path } => {
                let p = Path::new(path);
                if !is_work_folder(p, base) {
                    out.notes
                        .push(format!("{path} is not a work folder; it was not touched"));
                    continue;
                }
                if !ops.exists(p) {
                    out.removed.push(format!("{path} (already gone)"));
                    continue;
                }
                let first = ops.remove_tree(p);
                let result = match first {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        out.notes.push(format!(
                            "first removal of {path} failed ({e}); permissions reset and retried"
                        ));
                        if let Err(acl) = ops.reset_acl(p) {
                            out.notes.push(format!("permission reset failed: {acl}"));
                        }
                        ops.remove_tree(p)
                    }
                };
                match result {
                    Ok(()) => out.removed.push(path.clone()),
                    Err(e) => out.notes.push(format!("{path} not removed: {e}")),
                }
            }
        }
    }
    out.left = verify(made, ops);
    out.verified = out.left.is_empty();
    out
}

/// Every item that is still there.
pub fn verify(made: &[Made], ops: &impl Ops) -> Vec<String> {
    made.iter()
        .filter_map(|m| match m {
            Made::Drive { letter, target } => ops
                .drive_mapped(*letter)
                .then(|| format!("drive {letter}: still maps {target}")),
            Made::Folder { path } => ops
                .exists(Path::new(path))
                .then(|| format!("{path} still exists")),
        })
        .collect()
}

/// The real file system, for platforms without drive letters (and for the
/// tests of the folder removal).
pub struct LocalOps;

impl Ops for LocalOps {
    fn remove_tree(&self, path: &Path) -> std::io::Result<()> {
        std::fs::remove_dir_all(path)
    }
    fn reset_acl(&self, _path: &Path) -> std::io::Result<()> {
        Ok(())
    }
    fn remove_drive(&self, _letter: char) -> std::io::Result<()> {
        Ok(())
    }
    fn drive_mapped(&self, _letter: char) -> bool {
        false
    }
    fn exists(&self, path: &Path) -> bool {
        path.symlink_metadata().is_ok()
    }
}

/// Things outside the work folder a test could leave if Windows let an
/// escape test do what it must not: each is named by the test, so a hit is
/// this run's, and is removed and reported as a finding.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Traces {
    /// `%USERPROFILE%\kgraf-escape-*.txt`.
    pub profile_files: Vec<String>,
    /// `HKCU\...\Run` values named `kgraf-escape-*`.
    pub run_values: Vec<String>,
    /// Entries of kGraf's per-user folders (`%APPDATA%\kGraf`, `%LOCALAPPDATA%\kGraf`).
    pub kgraf_app_data: Vec<String>,
}

/// What appeared between two snapshots.
pub fn new_traces(before: &Traces, after: &Traces) -> Traces {
    let fresh = |a: &[String], b: &[String]| -> Vec<String> {
        b.iter().filter(|x| !a.contains(x)).cloned().collect()
    };
    Traces {
        profile_files: fresh(&before.profile_files, &after.profile_files),
        run_values: fresh(&before.run_values, &after.run_values),
        kgraf_app_data: fresh(&before.kgraf_app_data, &after.kgraf_app_data),
    }
}

/// Names a test escape would leave (`kgraf-escape-<pid>`, with an extension
/// for a file).
pub fn is_escape_trace(name: &str) -> bool {
    name.to_ascii_lowercase().starts_with("kgraf-escape-")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeSet;

    /// A fake platform: a set of existing paths and drives, and a removal
    /// that fails until the permissions are reset.
    #[derive(Default)]
    struct Fake {
        paths: RefCell<BTreeSet<PathBuf>>,
        drives: RefCell<BTreeSet<char>>,
        locked_until_reset: RefCell<bool>,
        log: RefCell<Vec<String>>,
    }

    impl Ops for Fake {
        fn remove_tree(&self, path: &Path) -> std::io::Result<()> {
            self.log
                .borrow_mut()
                .push(format!("remove {}", path.display()));
            if *self.locked_until_reset.borrow() {
                return Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
            }
            self.paths.borrow_mut().retain(|p| !p.starts_with(path));
            Ok(())
        }
        fn reset_acl(&self, path: &Path) -> std::io::Result<()> {
            self.log
                .borrow_mut()
                .push(format!("reset {}", path.display()));
            *self.locked_until_reset.borrow_mut() = false;
            Ok(())
        }
        fn remove_drive(&self, letter: char) -> std::io::Result<()> {
            self.log.borrow_mut().push(format!("unmap {letter}"));
            self.drives.borrow_mut().remove(&letter);
            Ok(())
        }
        fn drive_mapped(&self, letter: char) -> bool {
            self.drives.borrow().contains(&letter)
        }
        fn exists(&self, path: &Path) -> bool {
            self.paths.borrow().contains(path)
        }
    }

    fn base() -> PathBuf {
        PathBuf::from("/tmp-base")
    }

    #[test]
    fn the_layout_is_inside_one_named_folder() {
        let l = layout(&base(), "20261007-1-2");
        assert!(is_work_folder(&l.root, &base()));
        for p in [&l.mirror, &l.tmp, &l.kgraf_home, &l.e2e, &l.logs] {
            assert!(p.starts_with(&l.root), "{}", p.display());
        }
    }

    #[test]
    fn only_a_named_work_folder_is_ever_removed() {
        assert!(!is_work_folder(&base(), &base()));
        assert!(!is_work_folder(Path::new("/tmp-base/Documents"), &base()));
        assert!(!is_work_folder(
            Path::new("/tmp-base/kgraf-wintest-"),
            &base()
        ));
        assert!(!is_work_folder(
            Path::new("/other/kgraf-wintest-1"),
            &base()
        ));
        assert!(!is_work_folder(
            Path::new("/tmp-base/x/kgraf-wintest-1"),
            &base()
        ));
        let fake = Fake::default();
        fake.paths
            .borrow_mut()
            .insert(PathBuf::from("/tmp-base/Documents"));
        let made = vec![Made::Folder {
            path: "/tmp-base/Documents".into(),
        }];
        let c = clean_up(&made, &base(), &fake);
        assert!(fake.log.borrow().is_empty(), "{:?}", fake.log.borrow());
        assert!(!c.verified);
        assert!(c.notes[0].contains("not a work folder"));
    }

    #[test]
    fn the_drive_goes_first_and_a_locked_folder_is_retried_after_a_reset() {
        let l = layout(&base(), "id");
        let fake = Fake::default();
        fake.paths.borrow_mut().insert(l.root.clone());
        fake.paths.borrow_mut().insert(l.tmp.join("x"));
        fake.drives.borrow_mut().insert('W');
        *fake.locked_until_reset.borrow_mut() = true;
        let made = vec![
            Made::Folder {
                path: l.root.display().to_string(),
            },
            Made::Drive {
                letter: 'W',
                target: l.mirror.display().to_string(),
            },
        ];
        let c = clean_up(&made, &base(), &fake);
        let root = l.root.display().to_string();
        assert_eq!(
            *fake.log.borrow(),
            vec![
                "unmap W".to_owned(),
                format!("remove {root}"),
                format!("reset {root}"),
                format!("remove {root}"),
            ]
        );
        assert!(c.verified, "{c:?}");
        assert!(c.left.is_empty());
        assert_eq!(c.removed, vec!["drive W:".to_owned(), root]);
    }

    #[test]
    fn what_is_still_there_is_reported_and_not_verified() {
        let l = layout(&base(), "id");
        let fake = Fake::default();
        fake.drives.borrow_mut().insert('W');
        let made = vec![Made::Drive {
            letter: 'W',
            target: l.mirror.display().to_string(),
        }];
        assert_eq!(verify(&made, &fake).len(), 1);
        let fake_stuck = Fake::default();
        fake_stuck.paths.borrow_mut().insert(l.root.clone());
        *fake_stuck.locked_until_reset.borrow_mut() = true;
        // The reset does not help: the second removal fails as well.
        struct Stuck(Fake);
        impl Ops for Stuck {
            fn remove_tree(&self, _: &Path) -> std::io::Result<()> {
                Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
            }
            fn reset_acl(&self, p: &Path) -> std::io::Result<()> {
                self.0.reset_acl(p)
            }
            fn remove_drive(&self, l: char) -> std::io::Result<()> {
                self.0.remove_drive(l)
            }
            fn drive_mapped(&self, l: char) -> bool {
                self.0.drive_mapped(l)
            }
            fn exists(&self, p: &Path) -> bool {
                self.0.exists(p)
            }
        }
        let made = vec![Made::Folder {
            path: l.root.display().to_string(),
        }];
        let c = clean_up(&made, &base(), &Stuck(fake_stuck));
        assert!(!c.verified);
        assert_eq!(c.left, vec![format!("{} still exists", l.root.display())]);
    }

    #[test]
    fn a_real_folder_with_a_link_out_is_removed_without_touching_the_target() {
        let base_dir = tempfile::tempdir().expect("base");
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("keep.txt"), "keep").expect("write");
        let l = layout(base_dir.path(), "real");
        std::fs::create_dir_all(&l.tmp).expect("mkdir");
        std::fs::write(l.tmp.join("f"), "x").expect("write");
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), l.tmp.join("link")).expect("link");
        let made = vec![Made::Folder {
            path: l.root.display().to_string(),
        }];
        let c = clean_up(&made, base_dir.path(), &LocalOps);
        assert!(c.verified, "{c:?}");
        assert!(!l.root.exists());
        assert_eq!(
            std::fs::read_to_string(outside.path().join("keep.txt")).expect("kept"),
            "keep"
        );
    }

    #[test]
    fn only_new_traces_count() {
        let before = Traces {
            kgraf_app_data: vec!["settings.toml".into()],
            ..Traces::default()
        };
        let after = Traces {
            profile_files: vec!["kgraf-escape-12.txt".into()],
            run_values: vec![],
            kgraf_app_data: vec!["settings.toml".into(), "diagnostics.jsonl".into()],
        };
        let n = new_traces(&before, &after);
        assert_eq!(n.profile_files, vec!["kgraf-escape-12.txt".to_owned()]);
        assert_eq!(n.kgraf_app_data, vec!["diagnostics.jsonl".to_owned()]);
        assert!(is_escape_trace("KGRAF-ESCAPE-1.txt"));
        assert!(!is_escape_trace("kgraf.txt"));
    }
}

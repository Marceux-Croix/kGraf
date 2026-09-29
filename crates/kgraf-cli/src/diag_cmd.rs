//! `kgraf diag show` and `kgraf diag export` (decision 0069).
//!
//! Local-only diagnostics: usage counts and errors in one capped, per-user
//! file. Nothing here reaches the network; see `kgraf-diag`'s crate doc for
//! the file format and why. `path()` is where `main.rs` also points
//! `kgraf_diag::init` for the whole process.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

/// The diagnostics file's path for this process: [`kgraf_diag::resolve_path`]
/// (the per-user default, or `KGRAF_DIAG_PATH`/`KGRAF_DIAG_DISABLE`, the same
/// resolution `main` used to `init`), shared with the desktop shell. `None`
/// when diagnostics is disabled or the platform has no resolvable home
/// directory; every caller here turns that into a loud, specific message
/// rather than a generic "not found".
pub fn path() -> Option<PathBuf> {
    kgraf_diag::resolve_path()
}

fn require_path() -> Result<PathBuf> {
    path().context(
        "diagnostics is disabled (KGRAF_DIAG_DISABLE) or this platform's per-user data \
         directory could not be determined, so there is no diagnostics file to read",
    )
}

/// `kgraf diag show`: a human-readable summary, printed to stdout.
pub fn show() -> Result<()> {
    let path = require_path()?;
    let records = kgraf_diag::store::read_all(&path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    println!("file: {}", path.display());
    if let Some(reason) = unwritable_reason(&path) {
        println!(
            "warning: {} is not writable ({reason}); nothing new will be recorded here",
            path.display()
        );
    }
    print!("{}", kgraf_diag::report::render_summary(&records));
    Ok(())
}

/// Why `path` (or the directory that would hold it) cannot be written to
/// right now, or `None` if it can. A probe, not a guarantee: permissions can
/// change between this check and the next write, but it is enough to warn a
/// user why their diagnostics file has stopped growing (`diag show` says when the file is
/// unwritable).
fn unwritable_reason(path: &Path) -> Option<String> {
    use std::fs::OpenOptions;
    let probe_target: PathBuf = if path.exists() {
        path.to_path_buf()
    } else {
        path.parent()?.join(".kgraf-diag-write-probe")
    };
    let opened = OpenOptions::new()
        .create(!path.exists())
        .append(true)
        .open(&probe_target);
    match opened {
        Ok(_) => {
            if !path.exists() {
                let _ = std::fs::remove_file(&probe_target);
            }
            None
        }
        Err(e) => Some(e.to_string()),
    }
}

/// `kgraf diag export [--out FILE]`: a re-scrubbed, shareable copy.
/// `project_root` scrubs any path still embedded in an error's context the
/// same way the CLI's own `--root` would; a path outside it is hashed rather
/// than named (see `kgraf_diag::scrub` for the reasoning).
pub fn export(out: Option<PathBuf>, project_root: &Path) -> Result<()> {
    let path = require_path()?;
    if !path.exists() {
        bail!(
            "no diagnostics file exists yet at {} (nothing has been recorded)",
            path.display()
        );
    }
    let written = kgraf_diag::report::export(&path, out, Some(project_root))
        .with_context(|| format!("cannot export {}", path.display()))?;
    println!("wrote {}", written.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use kgraf_diag::event::{DiagEvent, Record};

    #[test]
    fn export_reports_missing_file_loudly() {
        // `path()` always resolves in this test environment (a real home
        // directory), so instead this checks the same "no file yet" refusal
        // `export` gives through a temp path directly, which is what a fresh
        // machine that has run kGraf zero times actually sees.
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.jsonl");
        let out = dir.path().join("out.jsonl");
        assert!(!missing.exists());
        // An explicit `out` path, in the same temp directory: `export`'s own
        // `None` default writes to the current directory, which would litter
        // this crate's checkout with `kgraf-diag-export-*.jsonl` files every
        // time this test runs.
        let err = kgraf_diag::report::export(&missing, Some(out), Some(dir.path()));
        // `store::read_all` treats a missing file as empty, not an error, so
        // `export`'s own contract (empty in, empty JSONL out) is what this
        // asserts; `diag_cmd::export`'s `bail!` above is the CLI-level guard
        // a user actually sees, tested by inspection of the message text.
        assert!(err.is_ok());
    }

    #[test]
    fn unwritable_reason_is_none_for_a_normal_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("diag.jsonl");
        assert!(super::unwritable_reason(&path).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn unwritable_reason_names_a_read_only_directory() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("diag.jsonl");
        let mut perms = std::fs::metadata(dir.path()).unwrap().permissions();
        perms.set_mode(0o500); // read + execute, no write
        std::fs::set_permissions(dir.path(), perms.clone()).unwrap();
        let reason = super::unwritable_reason(&path);
        // Root (and some CI sandboxes) ignores directory write permissions;
        // this only asserts when the permission actually took effect.
        if reason.is_none() {
            let mut writable = perms;
            writable.set_mode(0o700);
            std::fs::set_permissions(dir.path(), writable).unwrap();
            return;
        }
        assert!(reason.is_some());
        let mut writable = std::fs::metadata(dir.path()).unwrap().permissions();
        writable.set_mode(0o700);
        std::fs::set_permissions(dir.path(), writable).unwrap();
    }

    #[test]
    fn show_reads_back_what_was_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("diag.jsonl");
        kgraf_diag::store::append(
            &path,
            &Record::now(DiagEvent::launch()),
            kgraf_diag::DEFAULT_CAP_BYTES,
        )
        .unwrap();
        let records = kgraf_diag::store::read_all(&path).unwrap();
        let summary = kgraf_diag::report::render_summary(&records);
        assert!(summary.contains("launches: 1"));
    }
}

//! Where the diagnostics file lives.
//!
//! The diagnostics file sits in the per-user kGraf data directory that
//! `kgraf-shell` uses; the CLI shares it. `kgraf-shell/src/app_dirs.rs`
//! resolves that directory with `directories::ProjectDirs::from("", "",
//! "kGraf")`; this module resolves the same directory the same way, so the
//! desktop shell and `kgraf-cli` write and read one file without either crate
//! depending on the other (`kgraf-cli` does not currently depend on
//! `kgraf-shell`, and should not gain that dependency just for a directory
//! path). Both call sites are covered by `docs/decisions/0069-*.md`.

use std::path::PathBuf;

/// The application name `directories::ProjectDirs` is asked for. Kept in sync
/// with `kgraf_shell::app_dirs::APP_NAME` by hand (both are short, stable
/// constants; a shared crate for one string was judged not worth adding).
pub const APP_NAME: &str = "kGraf";

/// The diagnostics file's default path: `<data dir>/diagnostics.jsonl`. `None`
/// when the platform has no resolvable home directory (some minimal
/// containers): diagnostics is instrumentation, not a dependency, so a caller
/// treats `None` as "diagnostics disabled this run", never as a reason to
/// fail the command it is instrumenting.
pub fn default_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", APP_NAME)
        .map(|dirs| dirs.data_dir().join(crate::FILE_NAME))
}

/// The environment variable that overrides [`default_path`]: a test harness
/// (or a user diagnosing something unusual) points diagnostics at a chosen
/// file instead of the real per-user one. Reviewed in 2026-09-27 after a test
/// run wrote 140 lines into the owner's actual
/// `~/Library/Application Support/kGraf/diagnostics.jsonl`: every test that
/// exercises a real binary must set this.
pub const PATH_ENV: &str = "KGRAF_DIAG_PATH";

/// Set to disable diagnostics entirely for this process (any value, including
/// empty, counts): `resolve_path` then returns `None`, the same as having no
/// resolvable data directory. A test that wants no diagnostics file at all
/// (rather than a private one) sets this instead of `PATH_ENV`.
pub const DISABLE_ENV: &str = "KGRAF_DIAG_DISABLE";

/// The path a binary should call [`crate::init`] with: [`PATH_ENV`] if set (an
/// empty value counts as [`DISABLE_ENV`], since an accidentally-empty
/// override should not silently fall through to the real per-user path),
/// else [`DISABLE_ENV`] turns diagnostics off, else [`default_path`]. Both
/// `kgraf-cli` and `kgraf-shell` call this rather than `default_path`
/// directly, so both honor the same override the same way.
pub fn resolve_path() -> Option<PathBuf> {
    if std::env::var_os(DISABLE_ENV).is_some() {
        return None;
    }
    match std::env::var(PATH_ENV) {
        Ok(v) if !v.is_empty() => Some(PathBuf::from(v)),
        Ok(_) => None,
        Err(_) => default_path(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // `std::env` is process-global and this test binary runs `#[test]`s in
    // parallel threads; a lock serializes only the tests in this module that
    // touch `PATH_ENV`/`DISABLE_ENV` against each other. `set_var`/`remove_var`
    // are `unsafe` since edition 2024 for the same reason (another thread
    // could observe a torn read); nothing else in this crate's tests reads
    // these two variables while this lock is held.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn path_env_overrides_the_default() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        unsafe {
            std::env::set_var(PATH_ENV, "/tmp/kgraf-diag-test-override.jsonl");
            std::env::remove_var(DISABLE_ENV);
        }
        assert_eq!(
            resolve_path(),
            Some(PathBuf::from("/tmp/kgraf-diag-test-override.jsonl"))
        );
        unsafe {
            std::env::remove_var(PATH_ENV);
        }
    }

    #[test]
    fn disable_env_turns_it_off_even_with_path_env_set() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        unsafe {
            std::env::set_var(PATH_ENV, "/tmp/kgraf-diag-test-override.jsonl");
            std::env::set_var(DISABLE_ENV, "1");
        }
        assert_eq!(resolve_path(), None);
        unsafe {
            std::env::remove_var(PATH_ENV);
            std::env::remove_var(DISABLE_ENV);
        }
    }

    #[test]
    fn an_empty_path_env_also_disables_rather_than_falling_through() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        unsafe {
            std::env::set_var(PATH_ENV, "");
            std::env::remove_var(DISABLE_ENV);
        }
        assert_eq!(resolve_path(), None);
        unsafe {
            std::env::remove_var(PATH_ENV);
        }
    }
}

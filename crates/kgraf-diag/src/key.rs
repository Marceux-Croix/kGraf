//! The per-install random key a keyed hash (`scrub.rs`) uses for a path
//! outside the project root.
//!
//! **Why keyed, not plain BLAKE3.** An unkeyed
//! hash of a path is reversible by guessing: `blake3("/home/<name>/...")`
//! for a dictionary of common account names, project folder names and file
//! names is an offline attack an adversary can run from the diagnostics file
//! alone, with no access to the machine it came from. Keying the hash with 32
//! random bytes that are generated on first use, live only on the machine,
//! and are never written into the diagnostics file itself defeats that:
//! reversing a hash now requires the key, which requires the machine (or a
//! copy of the key file, which `kgraf diag export` never includes).
//!
//! The key is **not** "stable within one process" as an earlier version of
//! this crate's documentation claimed; it is stable for the life of the
//! install (persisted next to the diagnostics file), so two different runs,
//! weeks apart, still hash the same repeated path to the same value, which is
//! what makes "the same secret path shows up three times" visible in
//! `kgraf diag show` without naming the path.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub const KEY_LEN: usize = 32;

/// The key file for the diagnostics file at `diagnostics_path`: a sibling
/// file, `diagnostics.key`, never `diagnostics.jsonl` itself and never a
/// field inside it, so sharing or exporting the diagnostics file never hands
/// over the key that makes its hashed paths correlatable.
pub fn key_path(diagnostics_path: &Path) -> PathBuf {
    let mut p = diagnostics_path.to_path_buf();
    p.set_file_name("diagnostics.key");
    p
}

/// The key at `path`, generating and persisting one if none exists yet (or if
/// the existing file is the wrong size, which is treated as absent rather
/// than trusted partially). Two processes racing to create it for the first
/// time never end with two different keys: each writes its own candidate to
/// a private temp file (mode 0600 on Unix), then publishes it with a
/// hard link, which fails if the key file already exists, so exactly one
/// candidate wins. Every caller returns what is persisted, read back after
/// the attempt, never its own candidate unless that is what was persisted.
pub fn load_or_create(path: &Path) -> std::io::Result<[u8; KEY_LEN]> {
    if let Some(key) = read_key(path) {
        // A key that an older version or another tool left readable by
        // others is tightened, not trusted as it is.
        tighten(path)?;
        return Ok(key);
    }
    let mut candidate = [0u8; KEY_LEN];
    getrandom::fill(&mut candidate).map_err(std::io::Error::other)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let tmp = path.with_file_name(format!(
        "diagnostics.key.tmp.{}.{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    write_private(&tmp, &candidate)?;
    let published = publish(&tmp, path);
    let _ = std::fs::remove_file(&tmp);
    published?;
    let key = read_key_settled(path)
        .ok_or_else(|| std::io::Error::other("the diagnostics key was not persisted"))?;
    tighten(path)?;
    Ok(key)
}

/// Owner-only permissions on an existing key file. An error when they cannot
/// be set: a key others can read makes the hashed paths reversible.
fn tighten(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)?.permissions().mode() & 0o777;
        if mode != 0o600 {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// [`read_key`], waiting a few milliseconds for a key that another process
/// is in the middle of writing (the file exists and is not yet whole).
fn read_key_settled(path: &Path) -> Option<[u8; KEY_LEN]> {
    for _ in 0..10 {
        if let Some(key) = read_key(path) {
            return Some(key);
        }
        if !path.exists() {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    None
}

/// Link `tmp` to `path` unless `path` holds a valid key already. A file of
/// the wrong size is removed once and the link retried. A filesystem that
/// cannot hard-link falls back to `rename` ([`publish_by_rename`]), and the
/// caller's read-back still returns the persisted key.
fn publish(tmp: &Path, path: &Path) -> std::io::Result<()> {
    for _ in 0..2 {
        match std::fs::hard_link(tmp, path) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // A file that is not a whole key yet may be another process's
                // key in the middle of being written: give it a moment before
                // it is called wrong.
                if read_key_settled(path).is_some() {
                    return Ok(());
                }
                let _ = std::fs::remove_file(path);
            }
            Err(_) => return publish_by_rename(tmp, path),
        }
    }
    Err(std::io::Error::other(
        "could not publish the diagnostics key",
    ))
}

/// Publish `tmp` at `path` with an atomic `rename`, for a filesystem that
/// cannot hard-link. A reader never sees a partial file, and a file that is not
/// a whole key (a zero-byte or truncated leftover) is replaced, not kept. A
/// valid key already at `path` is kept. Two first runs that both pass the check
/// can still both rename; the caller's read-back returns what won, and this
/// path exists only where no atomic no-replace publish is available.
fn publish_by_rename(tmp: &Path, path: &Path) -> std::io::Result<()> {
    if read_key_settled(path).is_some() {
        return Ok(());
    }
    std::fs::rename(tmp, path)
}

/// Create `path` with owner-only permissions and write `bytes`. The key makes
/// the hashed paths in a shared diagnostics file reversible, so it is not
/// world-readable.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut f = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    f.write_all(bytes)
}

fn read_key(path: &Path) -> Option<[u8; KEY_LEN]> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() != KEY_LEN {
        return None;
    }
    let mut key = [0u8; KEY_LEN];
    key.copy_from_slice(&bytes);
    Some(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_directory_gets_a_persisted_key_that_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let diag_path = dir.path().join("diagnostics.jsonl");
        let key_path = key_path(&diag_path);
        assert_eq!(key_path.file_name().unwrap(), "diagnostics.key");
        let a = load_or_create(&key_path).unwrap();
        let b = load_or_create(&key_path).unwrap();
        assert_eq!(a, b, "the same install must keep hashing to the same key");
    }

    #[test]
    fn two_installs_get_different_keys() {
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        let a = load_or_create(&dir_a.path().join("diagnostics.key")).unwrap();
        let b = load_or_create(&dir_b.path().join("diagnostics.key")).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn a_wrong_sized_existing_file_is_replaced_not_trusted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("diagnostics.key");
        std::fs::write(&path, b"too short").unwrap();
        let key = load_or_create(&path).unwrap();
        assert_eq!(key.len(), KEY_LEN);
    }

    /// The link-free fallback keeps a valid published key, and replaces a
    /// zero-byte or truncated file instead of failing on it forever.
    #[test]
    fn the_link_free_fallback_keeps_a_valid_key_and_repairs_a_bad_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("diagnostics.key");
        let winner = load_or_create(&path).unwrap();
        let tmp = dir.path().join("candidate");
        write_private(&tmp, &[7u8; KEY_LEN]).unwrap();
        publish_by_rename(&tmp, &path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), winner, "a valid key stays");

        for bad in [&b""[..], &b"short"[..]] {
            std::fs::write(&path, bad).unwrap();
            write_private(&tmp, &[7u8; KEY_LEN]).unwrap();
            publish_by_rename(&tmp, &path).unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), vec![7u8; KEY_LEN]);
            assert!(!tmp.exists(), "the candidate was renamed into place");
        }
    }

    /// A key file left readable by others is tightened when it is loaded.
    #[cfg(unix)]
    #[test]
    fn an_existing_key_with_open_permissions_is_tightened() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("diagnostics.key");
        std::fs::write(&path, [3u8; KEY_LEN]).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let key = load_or_create(&path).unwrap();
        assert_eq!(key, [3u8; KEY_LEN], "the key itself is kept");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    /// Threads that all find no key at once end with one key: the one on disk.
    #[test]
    fn concurrent_first_runs_agree_on_the_persisted_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("diagnostics.key");
        let keys: Vec<[u8; KEY_LEN]> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..16)
                .map(|_| scope.spawn(|| load_or_create(&path).unwrap()))
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let on_disk = std::fs::read(&path).unwrap();
        for key in &keys {
            assert_eq!(
                &key[..],
                &on_disk[..],
                "a caller kept a key that was not persisted"
            );
        }
        let leftovers = std::fs::read_dir(dir.path()).unwrap().count();
        assert_eq!(leftovers, 1, "a temp file was left behind");
    }

    #[cfg(unix)]
    #[test]
    fn the_key_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("diagnostics.key");
        load_or_create(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}

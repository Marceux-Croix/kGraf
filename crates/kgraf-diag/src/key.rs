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
/// time never truncate each other's: each writes its own candidate to a
/// per-process temp name, then `rename`s over `path`; the loser reads back
/// the winner's key instead of keeping two different keys alive.
pub fn load_or_create(path: &Path) -> std::io::Result<[u8; KEY_LEN]> {
    if let Some(key) = read_key(path) {
        return Ok(key);
    }
    let mut candidate = [0u8; KEY_LEN];
    getrandom::fill(&mut candidate).map_err(std::io::Error::other)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_file_name(format!("diagnostics.key.tmp.{}", std::process::id()));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&candidate)?;
    }
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(candidate),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            // Another process's `rename` may have won the race in between;
            // its key is exactly as good as ours, so read it back rather
            // than fail a diagnostics path over a benign race.
            read_key(path).ok_or(e)
        }
    }
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
}

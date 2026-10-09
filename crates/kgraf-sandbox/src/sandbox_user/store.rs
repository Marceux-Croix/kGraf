//! Where the owner's stored sandbox passwords live, and how the launch path
//! opens the one password a command needs.
//!
//! `kgraf sandbox setup` writes the file ([`super::blob`]); a sandboxed
//! command on Windows reads it to start as the sandbox user. A missing file
//! means setup has not run, and the caller falls back to the token sandbox
//! with a notice, unless a tag names the setup to use ([`missing_store`]). A
//! file that is present but cannot be read or opened is an error: running
//! with less confinement than setup promised would be silent degradation.

use std::path::{Path, PathBuf};

use super::blob::{BlobError, Protector, StoredUser, decode_users};
use super::names::{MAX_SLOTS, Role};
use super::password::Password;

/// The file name for `tag` (a test run's own users) or the owner's default.
pub fn credentials_file_name(tag: Option<&str>) -> String {
    match tag {
        Some(t) => format!("sandbox-credentials-{t}.bin"),
        None => "sandbox-credentials.bin".to_owned(),
    }
}

/// What it means that no stored sandbox user was found for `tag`, at
/// `store` (`None` when no configuration folder exists).
///
/// Without a tag, setup has not run on this machine, and the token sandbox
/// runs: `Ok(())`. With a tag, the caller asked for the users of one tagged
/// setup by name, so their absence is an error, never a quiet fall back to
/// the token sandbox: a test that meant to run as the sandbox user would
/// otherwise pass while it tested the other backend.
pub fn missing_store(tag: Option<&str>, store: Option<&Path>) -> Result<(), String> {
    let Some(tag) = tag else {
        return Ok(());
    };
    let place = match store {
        Some(path) => format!("{} holds no sandbox user", path.display()),
        None => "no kGraf configuration folder was found".to_owned(),
    };
    Err(format!(
        "the sandbox users tagged {tag:?} were asked for, but {place}; run `kgraf sandbox \
         setup --tag {tag}` first, or unset the tag"
    ))
}

/// The file in this user's kGraf configuration folder, or `None` when no
/// configuration folder can be found.
pub fn credentials_path(tag: Option<&str>) -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", kgraf_diag::paths::APP_NAME)
        .map(|dirs| dirs.config_dir().join(credentials_file_name(tag)))
}

/// The folder of the worker slots' table and lock files ([`super::slots`]),
/// beside the store at `store`.
pub fn slots_dir(store: &Path) -> PathBuf {
    store.with_extension("slots")
}

/// The list of the owner's folders that hold an entry for the sandbox users,
/// beside the store at `store`. Grants on the owner's toolchain, `PATH`
/// folders, git directory and package stores stay in place between
/// commands, because writing a folder's DACL rewrites the inherited entries
/// of every file below it; `kgraf sandbox remove` removes each one listed.
pub fn grants_path(store: &Path) -> PathBuf {
    store.with_extension("grants")
}

/// The folders a grants list names: one absolute path per line, each once,
/// in the order first written. Blank and relative lines are skipped, so a
/// damaged line never names a folder relative to where `remove` runs.
pub fn parse_grants(text: &str) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for line in text.lines() {
        let path = PathBuf::from(line.trim_end_matches('\r'));
        if path.is_absolute() && !out.contains(&path) {
            out.push(path);
        }
    }
    out
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LoadError {
    #[error(transparent)]
    Blob(#[from] BlobError),
    #[error("the stored passwords file holds no {0} sandbox user; run `kgraf sandbox setup` again")]
    NoUser(String),
    #[error("the stored passwords file holds the {0} sandbox user twice")]
    Duplicate(String),
    #[error(
        "the stored passwords file names {0} but not every slot below it; run `kgraf sandbox \
         setup` again"
    )]
    SlotGap(String),
}

/// The user of `role` and its opened password. Only that user's password is
/// opened; the other stays sealed.
pub fn load_user(
    bytes: &[u8],
    role: Role,
    protector: &dyn Protector,
) -> Result<(StoredUser, Password), LoadError> {
    let mut found = decode_users(bytes)?
        .into_iter()
        .filter(|(user, _)| user.role == role);
    let (user, sealed) = found.next().ok_or(LoadError::NoUser(role.key()))?;
    if found.next().is_some() {
        return Err(LoadError::Duplicate(role.key()));
    }
    let plain = protector.unprotect(&sealed).map_err(BlobError::Unprotect)?;
    let text = String::from_utf8(plain.to_vec()).map_err(|_| BlobError::Entry)?;
    let password = Password::from_received(text).map_err(|_| BlobError::Entry)?;
    Ok((user, password))
}

/// The stored users of `role(0)`, `role(1)`, and on, up to the first slot
/// the file does not hold, each with its opened password. The end of the
/// list is where setup stopped making slots, so a slot the file does not
/// hold ends it and is not an error. A file that holds a later slot past
/// that gap is refused: setup never writes one, and acting on part of it
/// would run two folders as one user without saying so.
pub fn load_slot_users(
    bytes: &[u8],
    role: fn(u8) -> Role,
    protector: &dyn Protector,
) -> Result<Vec<(StoredUser, Password)>, LoadError> {
    let users = decode_users(bytes)?;
    let holds = |r: Role| users.iter().any(|(user, _)| user.role == r);
    let mut found = Vec::new();
    let mut next = 0;
    while next < MAX_SLOTS && holds(role(next)) {
        found.push(load_user(bytes, role(next), protector)?);
        next += 1;
    }
    if let Some(later) = (next..MAX_SLOTS).map(role).find(|r| holds(*r)) {
        return Err(LoadError::SlotGap(later.key()));
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use zeroize::Zeroizing;

    use super::super::blob::encode_credentials;
    use super::*;

    /// XOR with a key byte behind a tag; counts the passwords it opens.
    struct CountingProtector {
        key: u8,
        opened: Cell<usize>,
    }

    impl CountingProtector {
        fn new(key: u8) -> Self {
            Self {
                key,
                opened: Cell::new(0),
            }
        }
    }

    impl Protector for CountingProtector {
        fn protect(&self, plain: &[u8]) -> Result<Vec<u8>, String> {
            let mut out = b"SEAL".to_vec();
            out.extend(plain.iter().map(|b| b ^ self.key));
            Ok(out)
        }
        fn unprotect(&self, sealed: &[u8]) -> Result<Zeroizing<Vec<u8>>, String> {
            self.opened.set(self.opened.get() + 1);
            let body = sealed.strip_prefix(b"SEAL").ok_or("not sealed")?;
            Ok(Zeroizing::new(body.iter().map(|b| b ^ self.key).collect()))
        }
    }

    fn user(role: Role, rid: u32) -> StoredUser {
        StoredUser {
            role,
            name: format!(
                "kgraf-{}-1a2b3c4d",
                if role == Role::Offline { "off" } else { "on" }
            ),
            sid: format!("S-1-5-21-1-2-3-{rid}"),
        }
    }

    #[test]
    fn the_grants_list_sits_beside_the_store_and_parses_absolute_paths_once() {
        let store = Path::new("/cfg/sandbox-credentials-t1.bin");
        assert_eq!(
            grants_path(store),
            Path::new("/cfg/sandbox-credentials-t1.grants")
        );
        let abs = if cfg!(windows) {
            r"C:\Users\o\.cargo\bin"
        } else {
            "/home/o/.cargo/bin"
        };
        let other = if cfg!(windows) {
            r"C:\Users\o\.rustup"
        } else {
            "/home/o/.rustup"
        };
        let text = format!("{abs}\r\n\nrelative\\x\n{other}\n{abs}\n");
        assert_eq!(
            parse_grants(&text),
            vec![PathBuf::from(abs), PathBuf::from(other)]
        );
    }

    #[test]
    fn the_file_name_carries_the_tag() {
        assert_eq!(credentials_file_name(None), "sandbox-credentials.bin");
        assert_eq!(
            credentials_file_name(Some("t1")),
            "sandbox-credentials-t1.bin"
        );
        if let Some(path) = credentials_path(Some("t1")) {
            assert!(path.ends_with("sandbox-credentials-t1.bin"));
        }
    }

    #[test]
    fn only_the_requested_users_password_is_opened() {
        let off = Password::generate(&[]).unwrap();
        let on = Password::generate(&[]).unwrap();
        let p = CountingProtector::new(0x33);
        let bytes = encode_credentials(
            &[
                (user(Role::Offline, 1010), &off),
                (user(Role::Online, 1011), &on),
            ],
            &p,
        )
        .unwrap();
        for (role, want) in [(Role::Offline, &off), (Role::Online, &on)] {
            let before = p.opened.get();
            let (u, pw) = load_user(&bytes, role, &p).unwrap();
            assert_eq!(u.role, role);
            assert_eq!(pw.expose(), want.expose());
            assert_eq!(p.opened.get(), before + 1, "one password opened per load");
        }
    }

    /// Setup with four slots stores slot users 0 to 3 of each pool. Loading
    /// stops at the first slot the file does not hold; before, that slot was
    /// an error, so no stored setup ever loaded and every command was
    /// refused.
    #[test]
    fn slot_users_load_up_to_the_first_slot_the_file_does_not_hold() {
        let pw = Password::generate(&[]).unwrap();
        let p = CountingProtector::new(7);
        let mut entries = vec![
            (user(Role::Offline, 1010), &pw),
            (user(Role::Online, 1011), &pw),
        ];
        for i in 0..4u8 {
            entries.push((user(Role::Slot(i), 1100 + u32::from(i)), &pw));
            entries.push((user(Role::OnlineSlot(i), 1200 + u32::from(i)), &pw));
        }
        let bytes = encode_credentials(&entries, &p).unwrap();
        for role in [Role::Slot as fn(u8) -> Role, Role::OnlineSlot] {
            let found = load_slot_users(&bytes, role, &p).unwrap();
            let roles: Vec<Role> = found.iter().map(|(u, _)| u.role).collect();
            assert_eq!(roles, (0..4u8).map(role).collect::<Vec<_>>());
        }
        let none = encode_credentials(&entries[..2], &p).unwrap();
        assert!(load_slot_users(&none, Role::Slot, &p).unwrap().is_empty());
    }

    #[test]
    fn a_slot_past_a_gap_is_refused() {
        let pw = Password::generate(&[]).unwrap();
        let p = CountingProtector::new(9);
        let gap = encode_credentials(
            &[
                (user(Role::Offline, 1010), &pw),
                (user(Role::Slot(0), 1100), &pw),
                (user(Role::Slot(2), 1102), &pw),
            ],
            &p,
        )
        .unwrap();
        assert_eq!(
            load_slot_users(&gap, Role::Slot, &p).unwrap_err(),
            LoadError::SlotGap(Role::Slot(2).key())
        );
    }

    #[test]
    fn a_missing_or_doubled_role_is_refused() {
        let pw = Password::generate(&[]).unwrap();
        let p = CountingProtector::new(1);
        let only_off = encode_credentials(&[(user(Role::Offline, 1010), &pw)], &p).unwrap();
        assert_eq!(
            load_user(&only_off, Role::Online, &p).unwrap_err(),
            LoadError::NoUser("online".to_owned())
        );
        let twice = encode_credentials(
            &[
                (user(Role::Offline, 1010), &pw),
                (user(Role::Offline, 1012), &pw),
            ],
            &p,
        )
        .unwrap();
        assert_eq!(
            load_user(&twice, Role::Offline, &p).unwrap_err(),
            LoadError::Duplicate("offline".to_owned())
        );
        assert_eq!(p.opened.get(), 0, "no password opened on a refusal");
    }

    #[test]
    fn a_damaged_file_or_a_failed_unseal_is_an_error() {
        let p = CountingProtector::new(1);
        assert_eq!(
            load_user(b"junk", Role::Offline, &p).unwrap_err(),
            LoadError::Blob(BlobError::Magic)
        );
        let pw = Password::generate(&[]).unwrap();
        let bytes = encode_credentials(&[(user(Role::Offline, 1010), &pw)], &p).unwrap();
        assert!(matches!(
            load_user(&bytes, Role::Offline, &CountingProtector::new(0x81)),
            Err(LoadError::Blob(BlobError::Entry))
        ));
    }

    #[test]
    fn a_missing_store_is_an_error_only_when_a_tag_names_it() {
        assert_eq!(missing_store(None, None), Ok(()));
        assert_eq!(
            missing_store(None, Some(Path::new("/c/sandbox-credentials.bin"))),
            Ok(())
        );
        let e = missing_store(
            Some("run8"),
            Some(Path::new("/c/sandbox-credentials-run8.bin")),
        )
        .unwrap_err();
        assert!(e.contains("\"run8\""), "{e}");
        assert!(e.contains("sandbox-credentials-run8.bin"), "{e}");
        assert!(e.contains("kgraf sandbox setup --tag run8"), "{e}");
        let e = missing_store(Some("run8"), None).unwrap_err();
        assert!(e.contains("configuration folder"), "{e}");
    }
}

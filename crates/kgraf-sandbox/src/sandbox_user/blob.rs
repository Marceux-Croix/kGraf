//! The owner's stored passwords: one file in kGraf's per-user configuration
//! folder, each password protected by DPAPI for the current user.
//!
//! The elevated step may run as another administrator account, so it does
//! not protect the passwords itself: a blob protected for that account would
//! not open for the owner. It sends them over the setup pipe and the owner's
//! own, unelevated process protects them ([`Protector`]; on Windows,
//! `CryptProtectData` without `CRYPTPROTECT_LOCAL_MACHINE`, so no other
//! account on the machine can open them).
//!
//! The names and SIDs stay readable, so `kgraf sandbox status` can report
//! what is stored without opening a password.
//!
//! ```text
//! "KGSBCRED" | version u8 = 2 | count u8
//! per user: role u8 (0 offline, 1 online, 16 + i slot i without the
//!           network, 128 + i slot i with it)
//!           name length u16 LE | name (UTF-8)
//!           SID length u16 LE  | SID (UTF-8)
//!           blob length u32 LE | protected password
//! ```

use zeroize::Zeroizing;

use super::names::{MAX_SLOTS, MAX_USER_NAME, Role, is_sid_string};
use super::password::Password;

pub const MAGIC: &[u8; 8] = b"KGSBCRED";
pub const VERSION: u8 = 2;

/// The first role byte of a worker slot: slot `i` is `SLOT_BYTE + i`. A
/// version 1 file held only the two fixed roles, so a kGraf that reads one
/// asks for a fresh setup instead of guessing.
pub const SLOT_BYTE: u8 = 16;

/// The first role byte of a worker slot with the network open: slot `i` is
/// `ONLINE_SLOT_BYTE + i`. The version stays 2: a file from before these
/// users names slot users without the network and none with it, and the
/// launch path refuses that and asks for a fresh setup, while `kgraf sandbox
/// remove` still reads the file.
pub const ONLINE_SLOT_BYTE: u8 = 128;

/// The largest protected password accepted (a DPAPI blob of a 40-character
/// password is a few hundred bytes).
const MAX_BLOB: usize = 64 * 1024;

/// Seals and opens a password. On Windows, DPAPI for the current user.
pub trait Protector {
    fn protect(&self, plain: &[u8]) -> Result<Vec<u8>, String>;
    fn unprotect(&self, sealed: &[u8]) -> Result<Zeroizing<Vec<u8>>, String>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredUser {
    pub role: Role,
    pub name: String,
    pub sid: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BlobError {
    #[error("the stored passwords file is not one kGraf wrote")]
    Magic,
    #[error("the stored passwords file has version {0}, which this kGraf does not read")]
    Version(u8),
    #[error("the stored passwords file is cut short or damaged")]
    Truncated,
    #[error("the stored passwords file holds a malformed entry")]
    Entry,
    #[error("the stored passwords file holds bytes after its last entry")]
    Trailing,
    #[error("protecting a password failed: {0}")]
    Protect(String),
    #[error("opening a stored password failed: {0}")]
    Unprotect(String),
}

fn role_byte(r: Role) -> u8 {
    match r {
        Role::Offline => 0,
        Role::Online => 1,
        Role::Slot(i) => SLOT_BYTE.saturating_add(i),
        Role::OnlineSlot(i) => ONLINE_SLOT_BYTE.saturating_add(i),
    }
}

fn role_of_byte(b: u8) -> Option<Role> {
    match b {
        0 => Some(Role::Offline),
        1 => Some(Role::Online),
        _ if b >= ONLINE_SLOT_BYTE => {
            let i = b - ONLINE_SLOT_BYTE;
            (i < MAX_SLOTS).then_some(Role::OnlineSlot(i))
        }
        _ => {
            let i = b.checked_sub(SLOT_BYTE)?;
            (i < MAX_SLOTS).then_some(Role::Slot(i))
        }
    }
}

/// How many users one file may hold: the two fixed roles and both users of
/// every slot.
const MAX_ENTRIES: usize = 2 + 2 * MAX_SLOTS as usize;

/// The file's bytes for these users and passwords.
pub fn encode_credentials(
    entries: &[(StoredUser, &Password)],
    protector: &dyn Protector,
) -> Result<Vec<u8>, BlobError> {
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    if entries.len() > MAX_ENTRIES {
        return Err(BlobError::Entry);
    }
    out.push(u8::try_from(entries.len()).map_err(|_| BlobError::Entry)?);
    for (user, password) in entries {
        if user.name.is_empty() || user.name.len() > MAX_USER_NAME || !is_sid_string(&user.sid) {
            return Err(BlobError::Entry);
        }
        let sealed = protector
            .protect(password.expose().as_bytes())
            .map_err(BlobError::Protect)?;
        if sealed.len() > MAX_BLOB {
            return Err(BlobError::Entry);
        }
        out.push(role_byte(user.role));
        for field in [user.name.as_bytes(), user.sid.as_bytes()] {
            out.extend_from_slice(&(field.len() as u16).to_le_bytes());
            out.extend_from_slice(field);
        }
        out.extend_from_slice(&(sealed.len() as u32).to_le_bytes());
        out.extend_from_slice(&sealed);
    }
    Ok(out)
}

struct Cursor<'a>(&'a [u8]);

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], BlobError> {
        if self.0.len() < n {
            return Err(BlobError::Truncated);
        }
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Ok(head)
    }

    fn u8(&mut self) -> Result<u8, BlobError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<usize, BlobError> {
        let b = self.take(2)?;
        Ok(usize::from(u16::from_le_bytes([b[0], b[1]])))
    }

    fn u32(&mut self) -> Result<usize, BlobError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
    }

    fn text(&mut self) -> Result<String, BlobError> {
        let n = self.u16()?;
        String::from_utf8(self.take(n)?.to_vec()).map_err(|_| BlobError::Entry)
    }
}

/// The users the file holds and their sealed passwords, without opening any.
pub fn decode_users(bytes: &[u8]) -> Result<Vec<(StoredUser, Vec<u8>)>, BlobError> {
    let mut c = Cursor(bytes);
    if c.take(MAGIC.len()).map_err(|_| BlobError::Magic)? != MAGIC {
        return Err(BlobError::Magic);
    }
    let version = c.u8()?;
    if version != VERSION {
        return Err(BlobError::Version(version));
    }
    let count = c.u8()?;
    if usize::from(count) > MAX_ENTRIES {
        return Err(BlobError::Entry);
    }
    let mut out = Vec::with_capacity(usize::from(count));
    for _ in 0..count {
        let role = role_of_byte(c.u8()?).ok_or(BlobError::Entry)?;
        let name = c.text()?;
        let sid = c.text()?;
        if name.is_empty() || name.len() > MAX_USER_NAME || !is_sid_string(&sid) {
            return Err(BlobError::Entry);
        }
        let n = c.u32()?;
        if n > MAX_BLOB {
            return Err(BlobError::Entry);
        }
        let sealed = c.take(n)?.to_vec();
        out.push((StoredUser { role, name, sid }, sealed));
    }
    if !c.0.is_empty() {
        return Err(BlobError::Trailing);
    }
    Ok(out)
}

/// The users and their opened passwords.
pub fn decode_credentials(
    bytes: &[u8],
    protector: &dyn Protector,
) -> Result<Vec<(StoredUser, Password)>, BlobError> {
    decode_users(bytes)?
        .into_iter()
        .map(|(user, sealed)| {
            let plain = protector.unprotect(&sealed).map_err(BlobError::Unprotect)?;
            let text = String::from_utf8(plain.to_vec()).map_err(|_| BlobError::Entry)?;
            let password = Password::from_received(text).map_err(|_| BlobError::Entry)?;
            Ok((user, password))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in for DPAPI: XOR with a key byte and a 4-byte tag, so a
    /// sealed password does not hold the plain text and a damaged one fails.
    struct FakeProtector(u8);

    impl Protector for FakeProtector {
        fn protect(&self, plain: &[u8]) -> Result<Vec<u8>, String> {
            let mut out = b"SEAL".to_vec();
            out.extend(plain.iter().map(|b| b ^ self.0));
            Ok(out)
        }
        fn unprotect(&self, sealed: &[u8]) -> Result<Zeroizing<Vec<u8>>, String> {
            let body = sealed
                .strip_prefix(b"SEAL")
                .ok_or("not sealed by this key")?;
            Ok(Zeroizing::new(body.iter().map(|b| b ^ self.0).collect()))
        }
    }

    struct FailingProtector;

    impl Protector for FailingProtector {
        fn protect(&self, _: &[u8]) -> Result<Vec<u8>, String> {
            Err("CryptProtectData failed: 5".into())
        }
        fn unprotect(&self, _: &[u8]) -> Result<Zeroizing<Vec<u8>>, String> {
            Err("CryptUnprotectData failed: 13".into())
        }
    }

    fn users() -> Vec<StoredUser> {
        vec![
            StoredUser {
                role: Role::Offline,
                name: "kgraf-off-1a2b3c4d".into(),
                sid: "S-1-5-21-1-2-3-1010".into(),
            },
            StoredUser {
                role: Role::Online,
                name: "kgraf-on-1a2b3c4d".into(),
                sid: "S-1-5-21-1-2-3-1011".into(),
            },
        ]
    }

    #[test]
    fn credentials_round_trip_and_hold_no_plain_password() {
        let pws = [
            Password::generate(&[]).unwrap(),
            Password::generate(&[]).unwrap(),
        ];
        let entries: Vec<(StoredUser, &Password)> = users().into_iter().zip(pws.iter()).collect();
        let p = FakeProtector(0x5a);
        let bytes = encode_credentials(&entries, &p).unwrap();
        assert!(bytes.starts_with(MAGIC));
        for pw in &pws {
            let plain = pw.expose().as_bytes();
            assert!(
                !bytes.windows(plain.len()).any(|w| w == plain),
                "a password is in the file in plain text"
            );
        }
        let back = decode_credentials(&bytes, &p).unwrap();
        assert_eq!(back.len(), 2);
        for ((u, pw), (want_u, want_pw)) in back.iter().zip(entries.iter()) {
            assert_eq!(u, want_u);
            assert_eq!(pw.expose(), want_pw.expose());
        }
        let listed: Vec<StoredUser> = decode_users(&bytes)
            .unwrap()
            .into_iter()
            .map(|(u, _)| u)
            .collect();
        assert_eq!(listed, users());
    }

    #[test]
    fn another_key_cannot_open_the_passwords() {
        let pw = Password::generate(&[]).unwrap();
        let bytes = encode_credentials(&[(users()[0].clone(), &pw)], &FakeProtector(1)).unwrap();
        // Same tag, other key: opens to bytes that are not a valid password
        // (every byte gains the high bit, so it is not even UTF-8).
        assert_eq!(
            decode_credentials(&bytes, &FakeProtector(0x81)).unwrap_err(),
            BlobError::Entry
        );
        assert!(matches!(
            decode_credentials(&bytes, &FailingProtector),
            Err(BlobError::Unprotect(_))
        ));
        assert!(matches!(
            encode_credentials(&[(users()[0].clone(), &pw)], &FailingProtector),
            Err(BlobError::Protect(_))
        ));
    }

    #[test]
    fn damaged_files_are_refused() {
        let pw = Password::generate(&[]).unwrap();
        let p = FakeProtector(7);
        let good = encode_credentials(&[(users()[0].clone(), &pw)], &p).unwrap();
        assert_eq!(decode_users(b"").unwrap_err(), BlobError::Magic);
        assert_eq!(
            decode_users(b"NOTKGRAF\x01\x00").unwrap_err(),
            BlobError::Magic
        );
        // A version this kGraf does not read, newer or older: the file of a
        // setup from before worker slots is refused, not read as if it had
        // none.
        for version in [1u8, 3] {
            let mut other = good.clone();
            other[8] = version;
            assert_eq!(
                decode_users(&other).unwrap_err(),
                BlobError::Version(version)
            );
        }
        for cut in [9, 10, 12, good.len() - 1] {
            assert_eq!(
                decode_users(&good[..cut]).unwrap_err(),
                BlobError::Truncated,
                "cut at {cut}"
            );
        }
        let mut extra = good.clone();
        extra.push(0);
        assert_eq!(decode_users(&extra).unwrap_err(), BlobError::Trailing);
        let mut bad_role = good.clone();
        bad_role[10] = 9;
        assert_eq!(decode_users(&bad_role).unwrap_err(), BlobError::Entry);
        // A count larger than the entries present.
        let mut more = good.clone();
        more[9] = 2;
        assert_eq!(decode_users(&more).unwrap_err(), BlobError::Truncated);
    }

    #[test]
    fn malformed_users_are_not_written() {
        let pw = Password::generate(&[]).unwrap();
        let mut u = users()[0].clone();
        u.sid = "not a sid".into();
        assert_eq!(
            encode_credentials(&[(u, &pw)], &FakeProtector(1)).unwrap_err(),
            BlobError::Entry
        );
        let mut u = users()[0].clone();
        u.name = "x".repeat(21);
        assert_eq!(
            encode_credentials(&[(u, &pw)], &FakeProtector(1)).unwrap_err(),
            BlobError::Entry
        );
    }

    #[test]
    fn slot_users_round_trip_through_their_role_bytes() {
        let pw = Password::generate(&[]).unwrap();
        let p = FakeProtector(0x2a);
        let slots: Vec<StoredUser> = [0u8, 1, 7, MAX_SLOTS - 1]
            .into_iter()
            .map(|i| StoredUser {
                role: Role::Slot(i),
                name: format!("kgraf-s{i:02}-1a2b3c4d"),
                sid: format!("S-1-5-21-1-2-3-{}", 1100 + u32::from(i)),
            })
            .chain([0u8, 3, MAX_SLOTS - 1].into_iter().map(|i| StoredUser {
                role: Role::OnlineSlot(i),
                name: format!("kgraf-o{i:02}-1a2b3c4d"),
                sid: format!("S-1-5-21-1-2-3-{}", 1300 + u32::from(i)),
            }))
            .collect();
        let entries: Vec<(StoredUser, &Password)> =
            slots.iter().cloned().map(|u| (u, &pw)).collect();
        let bytes = encode_credentials(&entries, &p).unwrap();
        let back: Vec<StoredUser> = decode_users(&bytes)
            .unwrap()
            .into_iter()
            .map(|(u, _)| u)
            .collect();
        assert_eq!(back, slots);
        // A byte between the fixed roles and the first slot names no role.
        let mut bad = bytes.clone();
        bad[10] = SLOT_BYTE - 1;
        assert_eq!(decode_users(&bad).unwrap_err(), BlobError::Entry);
        // A slot above the highest one does not either.
        let mut high = bytes.clone();
        high[10] = SLOT_BYTE + MAX_SLOTS;
        assert_eq!(decode_users(&high).unwrap_err(), BlobError::Entry);
        // Nor does one above the highest online slot.
        let mut high = bytes.clone();
        high[10] = ONLINE_SLOT_BYTE + MAX_SLOTS;
        assert_eq!(decode_users(&high).unwrap_err(), BlobError::Entry);
        // More entries than there can be users.
        let many: Vec<(StoredUser, &Password)> = (0..=MAX_ENTRIES)
            .map(|i| {
                (
                    StoredUser {
                        role: Role::Slot(0),
                        name: format!("kgraf-s{:02}-1a2b3c4d", i % 100),
                        sid: "S-1-5-21-1-2-3-1100".to_owned(),
                    },
                    &pw,
                )
            })
            .collect();
        assert_eq!(encode_credentials(&many, &p).unwrap_err(), BlobError::Entry);
    }
}

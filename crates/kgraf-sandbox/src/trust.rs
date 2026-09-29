//! Whether a file or directory is one only its owner can change: the test
//! the Linux sandbox applies to each cargo file it lets a command read above
//! a worktree, and the host applies to what lies above a check scratch root.
//!
//! Trusted: owned by this user or root; never writable by others; writable
//! by its group only when that group is this user's own private group. A
//! user private group (UPG: the group named after the user, with no member
//! but the user) is the default on Debian, Ubuntu and Fedora, whose umask
//! 0002 makes every new directory group-writable; refusing those refused
//! every desktop checkout. A shared group (a setgid `/srv` team directory)
//! lets its other members write, so group-write is refused there.
//!
//! A group is this user's private group only when all of these hold: it is
//! the user's primary group; it is named for the user; it lists no member
//! but the user; and no other account has it as its primary group (a second
//! user made with `useradd -g ann bob` is in `ann` without being listed in
//! it). That last test walks the whole password database; a walk that fails,
//! or that does not return this user (NSS enumeration off, as with sssd or
//! LDAP by default), cannot prove the group private, so the answer is "not
//! private" (fail closed: group-write is then refused).
//!
//! Not considered: group passwords (`/etc/gshadow`, `newgrp`). A user who
//! knows a group's password can join it for a session without being listed
//! anywhere this module reads. Such passwords are rare and are set by an
//! administrator; a machine that uses one on a user's private group should
//! not rely on group-write being refused.

use std::os::unix::fs::MetadataExt;
use std::sync::Mutex;

/// Why another user could change the object `meta` describes, or `None`
/// when only its owner can. The reason reads after the path it is about:
/// "`<path>` is group-writable (mode 775) ...".
pub fn untrusted_reason(meta: &std::fs::Metadata) -> Option<String> {
    reason_for(meta.uid(), meta.gid(), meta.mode(), euid(), private_group())
}

/// [`untrusted_reason`] with every input explicit, so each branch is
/// testable without another user or group on the machine.
fn reason_for(uid: u32, gid: u32, mode: u32, me: u32, upg: Option<u32>) -> Option<String> {
    let perm = mode & 0o7777;
    if uid != me && uid != 0 {
        return Some(format!(
            "is owned by user {uid}, not by this user ({me}) or root"
        ));
    }
    if perm & 0o002 != 0 {
        return Some(format!("is writable by others (mode {:o})", perm & 0o777));
    }
    if perm & 0o020 != 0 && upg != Some(gid) {
        return Some(format!(
            "is group-writable (mode {:o}) by group {gid}, which is not this user's \
             private group",
            perm & 0o777
        ));
    }
    None
}

fn euid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// The cached answer of [`private_group`]: `None` until first read.
static UPG: Mutex<Option<Option<u32>>> = Mutex::new(None);

/// This user's primary group id when it is a user private group (see the
/// module docs), else `None`. Cached: every sandboxed command asks, and the
/// answer walks the whole password database. [`refresh_private_group`]
/// reads it again; the host calls that each time a run opens, so a
/// long-lived process (the tray host) sees a group change at its next run.
pub fn private_group() -> Option<u32> {
    let mut cached = UPG.lock().unwrap_or_else(|e| e.into_inner());
    *cached.get_or_insert_with(read_private_group)
}

/// Read the password and group databases again and cache the answer.
pub fn refresh_private_group() -> Option<u32> {
    let fresh = read_private_group();
    *UPG.lock().unwrap_or_else(|e| e.into_inner()) = Some(fresh);
    fresh
}

fn read_private_group() -> Option<u32> {
    let me = euid();
    let (user, gid) = passwd_entry(me)?;
    let (group, members) = group_entry(gid)?;
    // `None`: the walk failed or did not see this user; not provable.
    let others = other_primary_users(me, gid)?;
    is_private_group(&user, &group, &members, others).then_some(gid)
}

/// A user private group: named after the user, with no member listed but
/// the user (a primary group's own user is usually not listed at all), and
/// no other account whose primary group it is.
fn is_private_group(
    user: &[u8],
    group: &[u8],
    members: &[Vec<u8>],
    another_primary_user: bool,
) -> bool {
    group == user && members.iter().all(|m| m.as_slice() == user) && !another_primary_user
}

/// Whether an account other than `me` has `gid` as its primary group, from
/// one full walk of the password database; `None` when the walk fails or
/// never returns `me` (enumeration off or incomplete), so the caller fails
/// closed. getpwent(3) keeps one cursor per process and is not reentrant
/// (`getpwent_r` is not on macOS): the walk holds a lock, and nothing else in
/// kGraf walks the database.
fn other_primary_users(me: u32, gid: u32) -> Option<bool> {
    static WALK: Mutex<()> = Mutex::new(());
    let _one = WALK.lock().unwrap_or_else(|e| e.into_inner());
    let mut seen_me = false;
    let mut other = false;
    // SAFETY: setpwent/getpwent/endpwent are called under `WALK`; each
    // returned entry is read before the next call.
    unsafe {
        libc::setpwent();
        loop {
            clear_errno();
            let entry = libc::getpwent();
            if entry.is_null() {
                let e = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
                libc::endpwent();
                // End of the database: errno untouched, or ENOENT from some
                // NSS modules. Anything else is a failed walk.
                if e != 0 && e != libc::ENOENT {
                    return None;
                }
                break;
            }
            let (uid, primary) = ((*entry).pw_uid, (*entry).pw_gid);
            if uid == me {
                seen_me = true;
            } else if primary == gid {
                other = true;
            }
        }
    }
    seen_me.then_some(other)
}

/// Set this thread's errno to 0, so a null from getpwent(3) can be told
/// apart: end of the database leaves it 0, a failure sets it.
fn clear_errno() {
    // SAFETY: the errno location is this thread's own and valid for its life;
    // one int is written.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    unsafe {
        *libc::__errno_location() = 0;
    }
    // SAFETY: as above, on macOS and the BSDs.
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    unsafe {
        *libc::__error() = 0;
    }
}

/// Call a `get*_r` function with a buffer that grows on `ERANGE`, up to 1 MiB.
fn with_buffer<T>(
    mut call: impl FnMut(&mut [libc::c_char]) -> Result<Option<T>, i32>,
) -> Option<T> {
    let mut size = 4096;
    loop {
        let mut buf = vec![0 as libc::c_char; size];
        match call(&mut buf) {
            Ok(found) => return found,
            Err(libc::ERANGE) if size < 1 << 20 => size *= 2,
            Err(_) => return None,
        }
    }
}

/// `(name, primary gid)` of `uid` from getpwuid_r(3).
fn passwd_entry(uid: u32) -> Option<(Vec<u8>, u32)> {
    with_buffer(|buf| {
        let mut pwd = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: every pointer is valid for the call and `buf.len()` is its
        // size; getpwuid_r sets `found` to `pwd` or null.
        let rc = unsafe {
            libc::getpwuid_r(
                uid,
                pwd.as_mut_ptr(),
                buf.as_mut_ptr(),
                buf.len(),
                &mut found,
            )
        };
        if rc != 0 {
            return Err(rc);
        }
        if found.is_null() {
            return Ok(None);
        }
        // SAFETY: `found` points at `pwd`, filled by the successful call; its
        // strings point into `buf`, alive here.
        let entry = unsafe { &*found };
        if entry.pw_name.is_null() {
            return Ok(None);
        }
        // SAFETY: a non-null `pw_name` is a NUL-terminated string in `buf`.
        let name = unsafe { std::ffi::CStr::from_ptr(entry.pw_name) }
            .to_bytes()
            .to_vec();
        Ok(Some((name, entry.pw_gid)))
    })
}

/// `(name, listed members)` of `gid` from getgrgid_r(3).
fn group_entry(gid: u32) -> Option<(Vec<u8>, Vec<Vec<u8>>)> {
    with_buffer(|buf| {
        let mut grp = std::mem::MaybeUninit::<libc::group>::uninit();
        let mut found: *mut libc::group = std::ptr::null_mut();
        // SAFETY: as in `passwd_entry`, for getgrgid_r.
        let rc = unsafe {
            libc::getgrgid_r(
                gid,
                grp.as_mut_ptr(),
                buf.as_mut_ptr(),
                buf.len(),
                &mut found,
            )
        };
        if rc != 0 {
            return Err(rc);
        }
        if found.is_null() {
            return Ok(None);
        }
        // SAFETY: `found` points at `grp`, filled by the successful call.
        let entry = unsafe { &*found };
        if entry.gr_name.is_null() {
            return Ok(None);
        }
        // SAFETY: a non-null `gr_name` is a NUL-terminated string in `buf`.
        let name = unsafe { std::ffi::CStr::from_ptr(entry.gr_name) }
            .to_bytes()
            .to_vec();
        let mut members = Vec::new();
        let mut at = entry.gr_mem;
        // SAFETY: `gr_mem` is a null-terminated array of NUL-terminated
        // strings in `buf` (or null); each step reads one pointer of it.
        unsafe {
            while !at.is_null() && !(*at).is_null() {
                members.push(std::ffi::CStr::from_ptr(*at).to_bytes().to_vec());
                at = at.add(1);
            }
        }
        Ok(Some((name, members)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: u32 = 1000;
    const UPG: u32 = 1000;

    #[test]
    fn owner_root_and_strangers() {
        assert_eq!(reason_for(ME, UPG, 0o40755, ME, Some(UPG)), None);
        assert_eq!(reason_for(0, 0, 0o100644, ME, Some(UPG)), None);
        let stranger = reason_for(65534, UPG, 0o100644, ME, Some(UPG)).unwrap();
        assert!(stranger.contains("owned by user 65534"), "{stranger}");
    }

    #[test]
    fn other_write_is_never_trusted() {
        let r = reason_for(ME, UPG, 0o40757, ME, Some(UPG)).unwrap();
        assert!(r.contains("writable by others (mode 757)"), "{r}");
    }

    /// Umask 0002 in a user private group is trusted; group-write
    /// by any other group (a shared team group, or no UPG at all) is not.
    #[test]
    fn group_write_is_trusted_only_for_the_users_private_group() {
        assert_eq!(reason_for(ME, UPG, 0o40775, ME, Some(UPG)), None);
        assert_eq!(reason_for(ME, UPG, 0o100664, ME, Some(UPG)), None);
        let shared = reason_for(ME, 2000, 0o42775, ME, Some(UPG)).unwrap();
        assert!(
            shared.contains("group-writable (mode 775) by group 2000"),
            "{shared}"
        );
        assert!(reason_for(ME, UPG, 0o40775, ME, None).is_some());
    }

    #[test]
    fn a_private_group_is_named_for_the_user_and_has_no_other_member() {
        let v = |s: &[&str]| s.iter().map(|m| m.as_bytes().to_vec()).collect::<Vec<_>>();
        assert!(is_private_group(b"ann", b"ann", &v(&[]), false));
        assert!(is_private_group(b"ann", b"ann", &v(&["ann"]), false));
        assert!(!is_private_group(
            b"ann",
            b"ann",
            &v(&["ann", "bob"]),
            false
        ));
        assert!(!is_private_group(b"ann", b"staff", &v(&[]), false));
        assert!(!is_private_group(b"ann", b"team", &v(&["ann"]), false));
    }

    /// Review M1: `useradd -U ann; useradd -g ann bob` leaves `ann` named for
    /// ann with no listed member, yet bob, whose primary group it is, can
    /// write ann's group-writable directories. Another primary user makes it
    /// shared.
    #[test]
    fn another_primary_user_makes_the_group_shared() {
        assert!(!is_private_group(b"ann", b"ann", &[], true));
    }

    /// The databases are read on this machine: the answer is whatever they
    /// say, but reading them must work and agree with a second read.
    #[test]
    fn the_private_group_is_cached_and_a_refresh_agrees() {
        let first = private_group();
        assert_eq!(first, private_group());
        assert_eq!(first, refresh_private_group());
        assert!(
            passwd_entry(euid()).is_some(),
            "this user has a passwd entry"
        );
        // The walk sees this user wherever the database enumerates.
        assert!(other_primary_users(euid(), u32::MAX).is_some());
    }

    /// For a container run as a named user (a container
    /// check): `KGRAF_TEST_EXPECT_UPG`
    /// is `none` or `some`, and the answer for the running user must match.
    /// Ignored by default: it asserts the machine's databases, not code.
    #[test]
    #[ignore = "run explicitly as a prepared user with KGRAF_TEST_EXPECT_UPG"]
    fn the_running_users_private_group_is_as_expected() {
        let expect = std::env::var("KGRAF_TEST_EXPECT_UPG").expect("set the expectation");
        let got = refresh_private_group();
        match expect.as_str() {
            "none" => assert_eq!(got, None, "a shared group was taken as private"),
            "some" => assert!(got.is_some(), "a private group was not recognized"),
            other => panic!("KGRAF_TEST_EXPECT_UPG={other}"),
        }
    }
}

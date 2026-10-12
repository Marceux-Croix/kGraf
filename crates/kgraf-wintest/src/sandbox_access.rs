//! Whether a sandbox user can read and run the test binaries of a run.
//!
//! The re-run (e) of the sandbox user phase starts each sandbox test binary
//! as the owner, and the binary starts itself again as a sandbox user to
//! probe. If the sandbox users cannot read that binary, every probe fails
//! before it checks anything. The owner's profile admits only the owner,
//! SYSTEM and administrators, so a bundle unzipped there cannot work.
//!
//! The phase checks this before setup, when the sandbox group does not exist
//! yet. So the check reads what every local user carries: a grant of read
//! and execute on each binary to Everyone, Authenticated Users or Users, and
//! no deny entry on them that takes it away. A grant to the sandbox group
//! alone cannot exist before setup made the group. Folder traversal is not
//! checked: every user has the bypass-traverse right on a default Windows.
//!
//! Platform-neutral, so the judgment is tested on every host; the Windows
//! calls that read the lists are in `platform.rs`.

use kgraf_sandbox::sandbox_user::acl::AceView;

/// The SIDs in the token of every local user, with their names for a
/// message: Everyone, Authenticated Users, Users.
pub const EVERY_LOCAL_USER: [(&str, &str); 3] = [
    ("S-1-1-0", "Everyone"),
    ("S-1-5-11", "Authenticated Users"),
    ("S-1-5-32-545", "Users"),
];

/// Read, execute and synchronize on a file: `FILE_GENERIC_READ` and
/// `FILE_EXECUTE` (`0x1200a9`).
pub const READ_EXECUTE: u32 = 0x0012_00a9;

/// The generic rights as a file maps them.
fn mapped(mask: u32) -> u32 {
    let mut m = mask & 0x0fff_ffff;
    if mask & 0x8000_0000 != 0 {
        m |= 0x0012_0089; // GENERIC_READ
    }
    if mask & 0x2000_0000 != 0 {
        m |= 0x0012_00a0; // GENERIC_EXECUTE
    }
    if mask & 0x1000_0000 != 0 {
        m |= 0x001f_01ff; // GENERIC_ALL
    }
    m
}

/// The rights `dacl` grants a token that holds `sids`, read in order as
/// Windows reads it: an entry denies or grants only the rights an earlier
/// entry has not decided. `None` is a null list.
pub fn granted(dacl: Option<&[AceView]>, sids: &[&str]) -> u32 {
    let Some(entries) = dacl else {
        return u32::MAX;
    };
    let (mut allowed, mut denied) = (0u32, 0u32);
    for e in entries {
        if !sids.iter().any(|s| s.eq_ignore_ascii_case(&e.sid)) {
            continue;
        }
        let m = mapped(e.mask);
        if e.deny {
            denied |= m & !allowed;
        } else {
            allowed |= m & !denied;
        }
    }
    allowed
}

/// A Windows path as compared here: no verbatim prefix, backslashes, lower
/// case, no trailing separator.
fn plain(p: &str) -> String {
    let p = p.replace('/', "\\");
    let p = p.strip_prefix(r"\\?\").unwrap_or(&p);
    p.trim_end_matches('\\').to_lowercase()
}

/// Whether `path` is `folder` or inside it, on Windows path rules: case does
/// not count, and `C:\Users\alicetest` is not inside `C:\Users\alice`.
pub fn inside(path: &str, folder: &str) -> bool {
    let (p, f) = (plain(path), plain(folder));
    !f.is_empty() && (p == f || p.starts_with(&format!("{f}\\")))
}

/// One test binary and its access list as read; `Err` when it could not be
/// read.
pub struct Binary {
    pub path: String,
    pub dacl: Result<Option<Vec<AceView>>, String>,
}

/// `Ok` when the sandbox group (named `group` in the message) can read and
/// run every binary in `tests_dir`, or why not, by name.
pub fn judge(
    tests_dir: &str,
    profile: Option<&str>,
    group: &str,
    binaries: &[Binary],
) -> Result<(), String> {
    let cannot = format!(
        "the sandbox group {group} cannot read the test binaries in {tests_dir}, so every \
         probe of the re-run would fail before it checks anything"
    );
    let fix = "Move the unzipped bundle to a folder outside the profile that every local user \
               may read and run (C:\\kgraf-wintest, for example) and start the runner there";
    if let Some(profile) = profile
        && inside(tests_dir, profile)
    {
        return Err(format!(
            "{cannot}: the folder is inside the owner's profile {profile}, which admits only \
             the owner, SYSTEM and administrators. {fix}"
        ));
    }
    if binaries.is_empty() {
        return Err(format!("{cannot}: the folder has no test binary"));
    }
    let sids: Vec<&str> = EVERY_LOCAL_USER.iter().map(|(s, _)| *s).collect();
    let names: Vec<&str> = EVERY_LOCAL_USER.iter().map(|(_, n)| *n).collect();
    for b in binaries {
        match &b.dacl {
            Err(e) => {
                return Err(format!(
                    "{cannot}: the access list of {} could not be read: {e}",
                    b.path
                ));
            }
            Ok(dacl) => {
                if granted(dacl.as_deref(), &sids) & READ_EXECUTE != READ_EXECUTE {
                    return Err(format!(
                        "{cannot}: {} grants read and execute to none of {}. {fix}",
                        b.path,
                        names.join(", ")
                    ));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ace(deny: bool, sid: &str, mask: u32) -> AceView {
        AceView {
            deny,
            sid: sid.into(),
            mask,
        }
    }

    fn bin(dacl: Vec<AceView>) -> Binary {
        Binary {
            path: r"C:\kgraf-wintest\tests\escape_windows.exe".into(),
            dacl: Ok(Some(dacl)),
        }
    }

    const GROUP: &str = "kgraf-sandbox-0123abcd";
    const TESTS: &str = r"C:\kgraf-wintest\tests";
    const PROFILE: &str = r"C:\Users\runneradmin";

    #[test]
    fn a_folder_in_the_profile_is_refused_by_name() {
        let ok = vec![bin(vec![ace(false, "S-1-5-32-545", READ_EXECUTE)])];
        let tests = r"\\?\C:\USERS\RunnerAdmin\Downloads\bundle\tests";
        let err = judge(tests, Some(PROFILE), GROUP, &ok).unwrap_err();
        assert!(err.contains(GROUP), "{err}");
        assert!(err.contains("inside the owner's profile"), "{err}");
        assert!(err.contains(PROFILE), "{err}");
        // A sibling whose name starts like the profile is not inside it.
        assert!(!inside(r"C:\Users\runneradmin2\tests", PROFILE));
        assert!(judge(TESTS, Some(PROFILE), GROUP, &ok).is_ok());
        assert!(judge(TESTS, None, GROUP, &ok).is_ok());
    }

    #[test]
    fn read_and_execute_must_reach_every_local_user() {
        // The list the workflow sets on its copy of the bundle.
        let workflow_copy = vec![
            ace(false, "S-1-5-18", 0x001f_01ff),
            ace(false, "S-1-5-32-544", 0x001f_01ff),
            ace(false, "S-1-5-32-545", READ_EXECUTE),
        ];
        assert!(judge(TESTS, Some(PROFILE), GROUP, &[bin(workflow_copy)]).is_ok());
        // Generic rights count as their file rights.
        let generic = vec![ace(false, "S-1-1-0", 0xa000_0000)];
        assert!(judge(TESTS, None, GROUP, &[bin(generic)]).is_ok());
        // A profile-like list: no local user may read.
        let owner_only = vec![
            ace(false, "S-1-5-18", 0x001f_01ff),
            ace(false, "S-1-5-32-544", 0x001f_01ff),
            ace(false, "S-1-5-21-1-2-3-500", 0x001f_01ff),
        ];
        let err = judge(TESTS, None, GROUP, &[bin(owner_only)]).unwrap_err();
        assert!(
            err.contains(GROUP) && err.contains("escape_windows.exe"),
            "{err}"
        );
        // Read without execute is not enough.
        let read_only = vec![ace(false, "S-1-5-32-545", 0x0012_0089)];
        assert!(judge(TESTS, None, GROUP, &[bin(read_only)]).is_err());
    }

    #[test]
    fn a_deny_before_the_grant_takes_it_away_and_one_after_does_not() {
        let deny_first = vec![
            ace(true, "S-1-1-0", 0x20),
            ace(false, "S-1-5-32-545", READ_EXECUTE),
        ];
        assert!(judge(TESTS, None, GROUP, &[bin(deny_first)]).is_err());
        let deny_after = vec![
            ace(false, "S-1-5-32-545", READ_EXECUTE),
            ace(true, "S-1-1-0", 0x20),
        ];
        assert!(judge(TESTS, None, GROUP, &[bin(deny_after)]).is_ok());
        // A null list grants everything; an empty one nothing.
        let null = Binary {
            path: "x".into(),
            dacl: Ok(None),
        };
        assert!(judge(TESTS, None, GROUP, &[null]).is_ok());
        assert!(judge(TESTS, None, GROUP, &[bin(vec![])]).is_err());
    }

    #[test]
    fn no_binary_and_an_unreadable_list_are_refused() {
        assert!(judge(TESTS, None, GROUP, &[]).is_err());
        let unread = Binary {
            path: "x".into(),
            dacl: Err("access denied".into()),
        };
        let err = judge(TESTS, None, GROUP, &[unread]).unwrap_err();
        assert!(err.contains("could not be read"), "{err}");
    }
}

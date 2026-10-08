//! Judgments on access lists the elevated setup step makes before it trusts a
//! folder, and the folders it closes to the sandbox users. Platform-neutral,
//! so the rules are tested on every host; the Windows calls that read and
//! write the lists are in `windows::secure`.
//!
//! **Why the folder is checked.** The undo record lives under ProgramData,
//! where every local user may create a folder and, as that folder's creator,
//! holds full control of it. A sandboxed command could create kGraf's folder
//! first and put a link in it, and the administrator step would then write a
//! file of that command's choice. So the step trusts a folder only when its
//! owner is an administrator principal and no other principal may write it.

use std::path::{Path, PathBuf};

/// `NT AUTHORITY\SYSTEM`.
pub const SYSTEM_SID: &str = "S-1-5-18";
/// `BUILTIN\Administrators`.
pub const ADMINISTRATORS_SID: &str = "S-1-5-32-544";
/// `NT SERVICE\TrustedInstaller`, the owner of most system folders.
pub const TRUSTED_INSTALLER_SID: &str =
    "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464";
/// `CREATOR OWNER`: an inherit-only stand-in that becomes the SID of whoever
/// creates a child. Only trusted principals can create a child of a folder
/// that passes [`untrusted_writer`], so the entry grants nothing to anyone
/// else. No token carries this SID itself.
pub const CREATOR_OWNER_SID: &str = "S-1-3-0";
/// `OWNER RIGHTS`: what the owner may do. The owner is checked separately.
pub const OWNER_RIGHTS_SID: &str = "S-1-3-4";

/// Every access right that changes a folder, its entries, or its security:
/// `FILE_WRITE_DATA` (add a file), `FILE_APPEND_DATA` (add a folder),
/// `FILE_WRITE_EA`, `FILE_DELETE_CHILD`, `FILE_WRITE_ATTRIBUTES`, `DELETE`,
/// `WRITE_DAC`, `WRITE_OWNER`, `GENERIC_WRITE` and `GENERIC_ALL`.
pub const WRITE_RIGHTS: u32 = 0x0000_0002
    | 0x0000_0004
    | 0x0000_0010
    | 0x0000_0040
    | 0x0000_0100
    | 0x0001_0000
    | 0x0004_0000
    | 0x0008_0000
    | 0x1000_0000
    | 0x4000_0000;

/// One entry of an access list, as far as the judgment needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AceView {
    /// A deny entry. Every other entry type counts as a grant, so an entry
    /// type this code does not know can only make a folder less trusted.
    pub deny: bool,
    pub sid: String,
    pub mask: u32,
}

/// Whether `sid` may own a folder the administrator step writes in.
/// `elevated_user` is the SID the step itself runs as: an administrator, and
/// the default owner of what it creates where policy says "object creator".
pub fn is_trusted_owner(sid: &str, elevated_user: &str) -> bool {
    sid == SYSTEM_SID
        || sid == ADMINISTRATORS_SID
        || sid == TRUSTED_INSTALLER_SID
        || sid.eq_ignore_ascii_case(elevated_user)
}

/// The first principal other than an administrator principal that `dacl`
/// lets change the folder, or `None` when there is none. `dacl` is `None`
/// for a null access list, which grants everyone everything.
pub fn untrusted_writer(dacl: Option<&[AceView]>, elevated_user: &str) -> Option<String> {
    let Some(entries) = dacl else {
        return Some("everyone (the folder has no access list)".to_owned());
    };
    entries
        .iter()
        .filter(|e| !e.deny && e.mask & WRITE_RIGHTS != 0)
        .find(|e| {
            !(is_trusted_owner(&e.sid, elevated_user)
                || e.sid == CREATOR_OWNER_SID
                || e.sid == OWNER_RIGHTS_SID)
        })
        .map(|e| e.sid.clone())
}

/// Why a folder the administrator step would write in is not trusted, or
/// `None` when it is.
pub fn distrust(
    owner_sid: &str,
    dacl: Option<&[AceView]>,
    is_reparse_point: bool,
    elevated_user: &str,
) -> Option<String> {
    if is_reparse_point {
        return Some("it is a link (junction or symbolic link)".to_owned());
    }
    if !is_trusted_owner(owner_sid, elevated_user) {
        return Some(format!(
            "its owner is {owner_sid}, not SYSTEM or Administrators"
        ));
    }
    untrusted_writer(dacl, elevated_user).map(|sid| format!("{sid} may change it"))
}

/// The access list of a folder the administrator step creates, in SDDL:
/// owner Administrators, protected (nothing inherited from ProgramData,
/// where every user may create entries), full control for SYSTEM and
/// Administrators, read and list for Users (`kgraf sandbox status` reads the
/// record without administrator rights). `0x1200a9` is read, list, traverse
/// and synchronize.
pub const TRUSTED_FOLDER_SDDL: &str =
    "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1200a9;;;BU)";

// ---- folders closed to the sandbox users ------------------------------------

/// What a deny entry on a closed folder takes from the sandbox group, on that
/// folder alone: creating a file or a folder in it, and changing it.
/// `FILE_WRITE_DATA`, `FILE_APPEND_DATA`, `FILE_WRITE_EA`,
/// `FILE_DELETE_CHILD`, `FILE_WRITE_ATTRIBUTES`, `DELETE`, `WRITE_DAC`,
/// `WRITE_OWNER`. Reading stays.
pub const CLOSED_FOLDER_DENY: u32 = 0x0000_0002
    | 0x0000_0004
    | 0x0000_0010
    | 0x0000_0040
    | 0x0000_0100
    | 0x0001_0000
    | 0x0004_0000
    | 0x0008_0000;

/// What the machine offers for [`closed_folders`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Locations {
    /// The roots of the fixed local drives whose file system keeps access
    /// lists (`C:\`, `D:\`).
    pub fixed_drive_roots: Vec<PathBuf>,
    /// `%ProgramData%`, from the known-folder call.
    pub program_data: Option<PathBuf>,
    /// `%PUBLIC%` (`C:\Users\Public`), from the known-folder call.
    pub public: Option<PathBuf>,
    /// The folders directly in [`Self::public`] (Documents, Downloads, ...).
    pub public_children: Vec<PathBuf>,
    /// `%SystemRoot%` (`C:\Windows`).
    pub windows: Option<PathBuf>,
}

/// The folders under the Windows folder where every user may create files by
/// default and that no program of a sandbox user needs: its temporary folder
/// (each command gets its own `TMP` and `TEMP`), the legacy task folder and
/// the tracing folder. Other folders of that kind belong to TrustedInstaller,
/// whose access lists an administrator cannot change without taking
/// ownership, so they stay open; the backend's gaps text says so.
pub const WINDOWS_CHILDREN: [&str; 3] = ["Temp", "Tasks", "tracing"];

/// The folders where setup denies the sandbox group the right to create or
/// change entries, each on that folder alone (decision 0097, amendment of
/// 2026-10-08): the root of each fixed drive, ProgramData, Public and the
/// folders directly in it, and [`WINDOWS_CHILDREN`].
///
/// On the folder alone, never inherited: an inherited entry on a drive root
/// would rewrite the access list of every file on the drive, and an
/// administrator cannot write the lists of the system's own files. So folders
/// that already exist below these and grant every user write stay writable
/// (many do below ProgramData), and the gaps text says so.
pub fn closed_folders(loc: &Locations) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut push = |p: &Path| {
        let p = p.to_path_buf();
        if !out
            .iter()
            .any(|o| o.as_os_str().eq_ignore_ascii_case(p.as_os_str()))
        {
            out.push(p);
        }
    };
    loc.fixed_drive_roots.iter().for_each(|p| push(p));
    if let Some(p) = &loc.program_data {
        push(p);
    }
    if let Some(p) = &loc.public {
        push(p);
        loc.public_children.iter().for_each(|c| push(c));
    }
    if let Some(w) = &loc.windows {
        for child in WINDOWS_CHILDREN {
            push(&w.join(child));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: &str = "S-1-5-21-1-2-3-500";

    fn allow(sid: &str, mask: u32) -> AceView {
        AceView {
            deny: false,
            sid: sid.into(),
            mask,
        }
    }

    /// The access list of `C:\ProgramData` on a default install, as `icacls`
    /// shows it: Users may create files and folders.
    fn program_data_default() -> Vec<AceView> {
        vec![
            allow(SYSTEM_SID, 0x001f_01ff),
            allow(ADMINISTRATORS_SID, 0x001f_01ff),
            allow(CREATOR_OWNER_SID, 0x1000_0000),
            allow("S-1-5-32-545", 0x0012_00a9),
            // (CI)(WD,AD,WEA,WA)
            allow("S-1-5-32-545", 0x0000_0116),
        ]
    }

    #[test]
    fn a_folder_that_lets_users_create_entries_is_not_trusted() {
        assert_eq!(
            untrusted_writer(Some(&program_data_default()), ME).as_deref(),
            Some("S-1-5-32-545")
        );
        for sid in ["S-1-1-0", "S-1-5-11", "S-1-5-4", "S-1-5-21-9-9-9-1001"] {
            for right in [0x2, 0x4, 0x10, 0x40, 0x100, 0x1_0000, 0x4_0000, 0x8_0000] {
                let dacl = vec![allow(SYSTEM_SID, 0x1f01ff), allow(sid, right)];
                assert_eq!(
                    untrusted_writer(Some(&dacl), ME).as_deref(),
                    Some(sid),
                    "{sid} with {right:#x}"
                );
            }
            for generic in [0x1000_0000u32, 0x4000_0000] {
                let dacl = vec![allow(sid, generic)];
                assert!(untrusted_writer(Some(&dacl), ME).is_some(), "{generic:#x}");
            }
        }
        assert!(
            untrusted_writer(None, ME).is_some(),
            "a null list admits all"
        );
    }

    #[test]
    fn administrator_principals_and_reads_leave_a_folder_trusted() {
        let dacl = vec![
            allow(SYSTEM_SID, 0x001f_01ff),
            allow(ADMINISTRATORS_SID, 0x001f_01ff),
            allow(TRUSTED_INSTALLER_SID, 0x001f_01ff),
            allow(ME, 0x001f_01ff),
            allow(CREATOR_OWNER_SID, 0x1000_0000),
            allow(OWNER_RIGHTS_SID, 0x4_0000),
            allow("S-1-5-32-545", 0x0012_00a9),
            AceView {
                deny: true,
                sid: "S-1-1-0".into(),
                mask: 0x001f_01ff,
            },
        ];
        assert_eq!(untrusted_writer(Some(&dacl), ME), None);
        assert_eq!(distrust(ADMINISTRATORS_SID, Some(&dacl), false, ME), None);
        assert_eq!(distrust(ME, Some(&dacl), false, ME), None);
    }

    #[test]
    fn an_owner_or_a_link_outside_the_administrators_is_not_trusted() {
        let dacl = vec![allow(SYSTEM_SID, 0x001f_01ff)];
        let why = distrust("S-1-5-21-9-9-9-1001", Some(&dacl), false, ME).unwrap();
        assert!(why.contains("owner"), "{why}");
        let why = distrust(SYSTEM_SID, Some(&dacl), true, ME).unwrap();
        assert!(why.contains("link"), "{why}");
        let why = distrust(SYSTEM_SID, Some(&program_data_default()), false, ME).unwrap();
        assert!(why.contains("S-1-5-32-545"), "{why}");
    }

    #[test]
    fn the_created_folder_admits_only_administrators_to_write() {
        assert!(TRUSTED_FOLDER_SDDL.starts_with("O:BAD:P"), "protected");
        for ace in TRUSTED_FOLDER_SDDL.split('(').skip(1) {
            let ace = ace.trim_end_matches(')');
            let parts: Vec<&str> = ace.split(';').collect();
            let (rights, who) = (parts[2], parts[5]);
            match who {
                "SY" | "BA" => assert_eq!(rights, "FA"),
                "BU" => {
                    let mask = u32::from_str_radix(rights.trim_start_matches("0x"), 16).unwrap();
                    assert_eq!(mask & WRITE_RIGHTS, 0, "Users read only: {mask:#x}");
                }
                other => panic!("unexpected principal {other}"),
            }
        }
    }

    #[test]
    fn the_closed_folders_are_each_named_once_and_never_inherited() {
        let loc = Locations {
            fixed_drive_roots: vec![PathBuf::from(r"C:\"), PathBuf::from(r"D:\")],
            program_data: Some(PathBuf::from(r"C:\ProgramData")),
            public: Some(PathBuf::from(r"C:\Users\Public")),
            public_children: vec![
                PathBuf::from(r"C:\Users\Public\Documents"),
                PathBuf::from(r"C:\Users\Public\Downloads"),
            ],
            windows: Some(PathBuf::from(r"C:\Windows")),
        };
        let mut twice = loc.clone();
        twice.fixed_drive_roots.push(PathBuf::from(r"c:\"));
        let folders = closed_folders(&twice);
        assert_eq!(folders, closed_folders(&loc), "one entry per folder");
        assert_eq!(folders.len(), 2 + 1 + 1 + 2 + WINDOWS_CHILDREN.len());
        assert!(folders.contains(&PathBuf::from(r"C:\Windows").join("Temp")));
        // The deny takes every change and no read.
        assert_eq!(CLOSED_FOLDER_DENY & 0x0012_00a9, 0);
        assert_eq!(CLOSED_FOLDER_DENY & !WRITE_RIGHTS, 0);
        assert_eq!(closed_folders(&Locations::default()), Vec::<PathBuf>::new());
    }
}

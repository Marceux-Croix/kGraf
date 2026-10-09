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

// ---- the write grant on a tree ----------------------------------------------

/// `READ_CONTROL`: read an object's access list.
pub const READ_CONTROL_RIGHT: u32 = 0x0002_0000;
/// `WRITE_DAC`: rewrite an object's access list.
pub const WRITE_DAC_RIGHT: u32 = 0x0004_0000;

/// Where an entry of a tree grant applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// The object the entry is written on, and nothing below it. The root of
    /// the grant, or one file of the tree.
    Folder,
    /// Everything below that object, by inheritance, and not the object
    /// itself. Anything created there later gets it too.
    Below,
    /// The object the entry is written on and everything below it. What one
    /// existing folder of the tree carries ([`child_grant`]): the entry is
    /// effective on that folder and is inherited by what is made in it.
    HereAndBelow,
}

/// Who an entry of a tree grant is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Who {
    /// The sandbox user the grant is written for.
    SandboxUser,
    /// `OWNER RIGHTS` ([`OWNER_RIGHTS_SID`]): whoever owns the object the
    /// entry is on, which for a file a sandbox user created is that user.
    OwnerRights,
    /// kGraf's own user, which owns the tree.
    Owner,
}

/// One entry of a write grant on a tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TreeAce {
    pub who: Who,
    pub mask: u32,
    pub scope: Scope,
}

/// The entries a write grant on a worktree writes: `folder_mask` for the
/// sandbox user on the folder, `below_mask` for it on everything below, and
/// the two entries that keep the owner of a file in the tree from giving
/// itself more than the list allows.
///
/// **Why the owner entries.** A standard user owns every file it creates,
/// and an owner may read and rewrite the access list of what it owns
/// whatever that list says. A slot user that moved to another folder would
/// therefore still own the files it created in the old one, could rewrite
/// their lists from its new folder, and could write and delete them again,
/// although a revoke took its entries away. That breaks the rule that a
/// user never keeps write access to a folder another user may now work in.
/// An `OWNER RIGHTS` entry replaces an owner's implicit rights with exactly
/// what that entry allows, which here is reading the list.
///
/// The same entry takes kGraf's own implicit right to rewrite a list, since
/// kGraf owns the tree, and kGraf must still write these lists. So kGraf's
/// user gets those two rights by name, on the folder and below it, and
/// nothing else: it keeps exactly what ownership gave it, and no principal
/// gains a right it did not have.
///
/// The alternative, resetting the owner of every object on each revoke, was
/// rejected: an owner is not inherited, so it would mean a walk of the whole
/// tree, object by object, on the slot-move path, where a build folder holds
/// tens of thousands of files.
pub fn tree_grant(folder_mask: u32, below_mask: u32) -> Vec<TreeAce> {
    let owner_mask = READ_CONTROL_RIGHT | WRITE_DAC_RIGHT;
    vec![
        TreeAce {
            who: Who::SandboxUser,
            mask: folder_mask,
            scope: Scope::Folder,
        },
        TreeAce {
            who: Who::SandboxUser,
            mask: below_mask,
            scope: Scope::Below,
        },
        TreeAce {
            who: Who::OwnerRights,
            mask: READ_CONTROL_RIGHT,
            scope: Scope::Folder,
        },
        TreeAce {
            who: Who::OwnerRights,
            mask: READ_CONTROL_RIGHT,
            scope: Scope::Below,
        },
        TreeAce {
            who: Who::Owner,
            mask: owner_mask,
            scope: Scope::Folder,
        },
        TreeAce {
            who: Who::Owner,
            mask: owner_mask,
            scope: Scope::Below,
        },
    ]
}

/// Why the folder `base` cannot hold the undo record, judged from its text
/// alone, or `None` when the text is sound.
///
/// The record's folders are made under this one, so its shape is part of what
/// keeps the record where only administrators may write it. The known-folder
/// call gives `C:\ProgramData`; a name with a colon in it names an alternate
/// data stream, not a folder, and a relative part could leave the drive.
pub fn record_base_shape(base: &str) -> Option<String> {
    let bytes = base.as_bytes();
    if bytes.len() < 3
        || !bytes[0].is_ascii_alphabetic()
        || bytes[1] != b':'
        || (bytes[2] != b'\\' && bytes[2] != b'/')
    {
        return Some(format!("{base} is not a path on a local drive"));
    }
    for part in base[3..].split(['\\', '/']).filter(|p| !p.is_empty()) {
        if part.contains(':') {
            return Some(format!(
                "{base} names an alternate data stream in {part}, not a folder"
            ));
        }
        if part == "." || part == ".." {
            return Some(format!("{base} holds the relative part {part}"));
        }
    }
    Some(base)
        .filter(|b| b.contains("\\\\") || b.contains("//"))
        .map(|b| format!("{b} holds an empty part"))
}

/// The entries of a tree grant ([`tree_grant`]) as they are written on one
/// existing child of the tree, a folder where `is_dir`, else a file.
///
/// **Why this exists.** Windows propagates an inheritable entry to the
/// children that already exist by itself, but it walks the tree with rules
/// the sandbox cannot steer: it may enter a junction, and it writes the one
/// security descriptor that every hard link of a file shares, which can lie
/// outside the tree. That propagation is therefore suppressed and the pass is
/// made here, child by child. The entries below are derived from the same
/// list the root carries, so what the root restricts the child restricts:
/// without that, the `OWNER RIGHTS` entry and kGraf's compensating entry
/// would reach only the files created after the grant, and every file already
/// in the tree would keep the sandbox user as its owner, with an owner's
/// implicit right to rewrite its own list.
///
/// Each entry that applies below the root is written on the child, with its
/// own trustee: effective on a file, effective and inheritable on a folder,
/// so what is made in that folder later still gets it from Windows. An entry
/// that applies to the root alone is not a child's.
pub fn child_grant(aces: &[TreeAce], is_dir: bool) -> Vec<TreeAce> {
    aces.iter()
        .filter(|ace| ace.scope == Scope::Below)
        .map(|ace| TreeAce {
            scope: if is_dir {
                Scope::HereAndBelow
            } else {
                Scope::Folder
            },
            ..*ace
        })
        .collect()
}

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

/// The flags of a deny entry that setup writes: none, so the entry is on the
/// folder alone ([`CLOSED_FOLDER_DENY`] and `NO_INHERITANCE`).
pub const CLOSED_FOLDER_FLAGS: u8 = 0;

/// The entry flags that make an access entry inherited (`INHERITED_ACE`,
/// `0x10`) or inheritable (`OBJECT_INHERIT_ACE`, `CONTAINER_INHERIT_ACE`,
/// `NO_PROPAGATE_INHERIT_ACE`, `INHERIT_ONLY_ACE`, the low four bits). An
/// entry setup writes or takes off again has none of them: nothing it does
/// may reach a child.
pub const ACE_INHERITANCE_FLAGS: u8 = 0x1F;

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

// ---- the runner's own access lists ---------------------------------------------

/// `ACCESS_ALLOWED_ACE_TYPE`: the one allow entry whose layout (header, mask,
/// SID) the check below reads.
pub const ACE_ALLOWED: u8 = 0x00;
/// The deny entry types (`ACCESS_DENIED_ACE_TYPE`, `..._OBJECT_...`,
/// `..._CALLBACK_...`, `..._CALLBACK_OBJECT_...`). A deny entry grants
/// nothing, so it can never break the check.
pub const ACE_DENY_TYPES: [u8; 4] = [0x01, 0x06, 0x0A, 0x0C];
/// The allow entry types other than [`ACE_ALLOWED`]: compound, object,
/// callback and callback-object. Each grants access, and each puts its SID
/// at another offset (object entries after their flags and GUIDs), so
/// reading one as a plain entry would check the wrong bytes.
pub const ACE_OTHER_ALLOW_TYPES: [u8; 4] = [0x04, 0x05, 0x09, 0x0B];

/// The bytes of `S-1-5-18` (SYSTEM) as a binary SID.
const SYSTEM_SID_BYTES: [u8; 12] = [1, 1, 0, 0, 0, 0, 0, 5, 18, 0, 0, 0];

/// What a runner access list grants that it must not, or why it cannot be
/// read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SelfAclProblem {
    /// The list's bytes do not hold the entries its header says.
    Malformed(&'static str),
    /// An entry of a type this check does not read. An allow entry other than
    /// the plain one, or an unknown type, could grant anything.
    Unchecked { index: usize, ace_type: u8 },
    /// An allow entry for a principal other than SYSTEM holds some of the
    /// forbidden rights (`mask` is that part).
    Grants { index: usize, mask: u32 },
}

impl std::fmt::Display for SelfAclProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(why) => write!(f, "the access list is malformed: {why}"),
            Self::Unchecked { index, ace_type } => write!(
                f,
                "entry {index} is of type 0x{ace_type:02x}, which this check cannot read; \
                 only plain allow entries and deny entries are accepted"
            ),
            Self::Grants { index, mask } => write!(
                f,
                "entry {index}, for a principal other than SYSTEM, still allows access \
                 0x{mask:08x}"
            ),
        }
    }
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes([*b.get(at)?, *b.get(at + 1)?]))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// Check a binary access list (`ACL` and its entries, as Windows returns it)
/// written on one of the runner's own objects: every entry that allows
/// access to anyone but SYSTEM must stay clear of `forbidden`. Deny entries
/// are skipped. Any other entry type fails the check (fail loud): the lists
/// kGraf writes hold plain allow entries only, so another type means the list
/// is not the one written.
///
/// Platform-neutral, so it is tested on every host with lists built by hand;
/// `windows::runner` passes the bytes `GetSecurityInfo` returned.
pub fn check_self_acl(acl: &[u8], forbidden: u32) -> Result<(), SelfAclProblem> {
    // ACL header: revision u8, Sbz1 u8, AclSize u16, AceCount u16, Sbz2 u16.
    let size = u16_at(acl, 2).ok_or(SelfAclProblem::Malformed("no header"))? as usize;
    let count = u16_at(acl, 4).ok_or(SelfAclProblem::Malformed("no header"))? as usize;
    if size < 8 || size > acl.len() {
        return Err(SelfAclProblem::Malformed("its size is outside its bytes"));
    }
    let acl = &acl[..size];
    let mut at = 8;
    for index in 0..count {
        // ACE header: type u8, flags u8, size u16.
        let (Some(&ace_type), Some(ace_size)) = (acl.get(at), u16_at(acl, at + 2)) else {
            return Err(SelfAclProblem::Malformed("an entry runs past the list"));
        };
        let ace_size = ace_size as usize;
        if ace_size < 4 || at + ace_size > acl.len() {
            return Err(SelfAclProblem::Malformed("an entry runs past the list"));
        }
        let ace = &acl[at..at + ace_size];
        at += ace_size;
        if ACE_DENY_TYPES.contains(&ace_type) {
            continue;
        }
        if ace_type != ACE_ALLOWED {
            return Err(SelfAclProblem::Unchecked { index, ace_type });
        }
        // Plain allow entry: header (4), mask u32, then the SID: revision u8,
        // sub-authority count u8, authority (6), 4 bytes per sub-authority.
        let mask = u32_at(ace, 4).ok_or(SelfAclProblem::Malformed("an allow entry is short"))?;
        let subs = *ace
            .get(9)
            .ok_or(SelfAclProblem::Malformed("an allow entry is short"))?;
        let sid = ace
            .get(8..16 + 4 * subs as usize)
            .ok_or(SelfAclProblem::Malformed("a SID runs past its entry"))?;
        if sid != SYSTEM_SID_BYTES && mask & forbidden != 0 {
            return Err(SelfAclProblem::Grants {
                index,
                mask: mask & forbidden,
            });
        }
    }
    Ok(())
}

/// The folder of [`closed_folders`] that `root` is, or `None` when `root` is
/// none of them.
///
/// A writable root that is one of these can never be written by a sandbox
/// user: a deny entry is evaluated before any grant, so the grant reads back
/// as present and every write still fails, with nothing naming the cause.
/// Compared without case, as Windows compares a path.
pub fn closed_folder_of(root: &Path, loc: &Locations) -> Option<PathBuf> {
    closed_folders(loc)
        .into_iter()
        .find(|p| p.as_os_str().eq_ignore_ascii_case(root.as_os_str()))
}

/// `SE_DACL_AUTO_INHERIT_REQ`: asks the system to treat the list as one that
/// takes part in automatic inheritance.
pub const SE_DACL_AUTO_INHERIT_REQ: u16 = 0x0100;
/// `SE_DACL_AUTO_INHERITED`: the list is set up for automatic inheritance.
pub const SE_DACL_AUTO_INHERITED: u16 = 0x0400;
/// `SE_DACL_PROTECTED`: the list takes nothing from its parent.
pub const SE_DACL_PROTECTED: u16 = 0x1000;

/// The control bits a lone write of a folder's list sets
/// (`windows::secure::write_dacl`), from the bits the folder had: which bits
/// it sets (the first value, for `SetSecurityDescriptorControl`'s bits of
/// interest) and to what (the second).
///
/// The protected and auto-inherited bits are carried as they were. A folder
/// that was auto-inherited also gets `SE_DACL_AUTO_INHERIT_REQ`: on Windows
/// Server 2025 a list written with `SE_DACL_AUTO_INHERITED` alone through
/// `SetKernelObjectSecurity` kept its entries and its protected bit but lost
/// the auto-inherited bit (`0x1400` became `0x1000`, `0x0400` became `0`),
/// which the read-back refused, and the roll-back, written the same way,
/// could not put back either. The request bit is how a caller that is not
/// propagating asks for the bit to be kept; that it keeps it on this path is
/// not documented and not yet measured, so the read-back still decides. A
/// folder that was not auto-inherited gets neither bit, as before, which
/// Windows kept.
pub fn lone_write_control(before: u16) -> (u16, u16) {
    let interest = SE_DACL_PROTECTED | SE_DACL_AUTO_INHERITED | SE_DACL_AUTO_INHERIT_REQ;
    let carried = before & (SE_DACL_PROTECTED | SE_DACL_AUTO_INHERITED);
    let request = if before & SE_DACL_AUTO_INHERITED != 0 {
        SE_DACL_AUTO_INHERIT_REQ
    } else {
        0
    };
    (interest, carried | request)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two cases Windows Server 2025 showed: an auto-inherited list (with
    /// or without protection) asks for the bit to be kept; a list without it
    /// is written as before. No other bit is ever set.
    #[test]
    fn a_lone_write_asks_to_keep_the_auto_inherited_bit() {
        let all = SE_DACL_PROTECTED | SE_DACL_AUTO_INHERITED | SE_DACL_AUTO_INHERIT_REQ;
        assert_eq!(lone_write_control(0x1400), (all, 0x1500));
        assert_eq!(lone_write_control(0x0400), (all, 0x0500));
        assert_eq!(lone_write_control(0x1000), (all, 0x1000));
        assert_eq!(lone_write_control(0x0000), (all, 0x0000));
        // Other control bits of the folder (here SE_DACL_PRESENT and
        // SE_SELF_RELATIVE) are not written.
        assert_eq!(lone_write_control(0x8404), (all, 0x0500));
    }

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

    /// The entries of a tree grant: an owner of a file in the tree may read
    /// its access list and change nothing, so a sandbox user that owns the
    /// files it created cannot give itself rights a revoke took away, and
    /// kGraf keeps by name what ownership gave it.
    #[test]
    fn a_tree_grant_leaves_an_owner_no_right_to_change_the_list() {
        const FOLDER: u32 = 0x0012_01bf;
        const BELOW: u32 = FOLDER | 0x0001_0000;
        let aces = tree_grant(FOLDER, BELOW);
        let of =
            |who: Who| -> Vec<TreeAce> { aces.iter().copied().filter(|a| a.who == who).collect() };
        // Every entry covers the folder and everything below it, so a file
        // created later carries it too.
        for who in [Who::SandboxUser, Who::OwnerRights, Who::Owner] {
            let mine = of(who);
            assert_eq!(mine.len(), 2, "{who:?}");
            assert_eq!(mine[0].scope, Scope::Folder);
            assert_eq!(mine[1].scope, Scope::Below);
        }
        // The sandbox user's own masks are untouched.
        assert_eq!(of(Who::SandboxUser)[0].mask, FOLDER);
        assert_eq!(of(Who::SandboxUser)[1].mask, BELOW);
        // An owner may read the list and change nothing.
        for ace in of(Who::OwnerRights) {
            assert_eq!(ace.mask, READ_CONTROL_RIGHT);
            assert_eq!(ace.mask & WRITE_RIGHTS, 0, "an owner may change it");
        }
        assert_eq!(OWNER_RIGHTS_SID, "S-1-3-4");
        // kGraf keeps, by name, the two rights ownership gave it, and no
        // more: without them the entry above would lock it out of the lists
        // it must write.
        for ace in of(Who::Owner) {
            assert_eq!(ace.mask, READ_CONTROL_RIGHT | WRITE_DAC_RIGHT);
            assert_eq!(ace.mask & WRITE_RIGHTS, WRITE_DAC_RIGHT);
        }
        assert_eq!(aces.len(), 6);
    }

    /// A writable root that is one of the closed folders must be named, so
    /// the backend refuses it instead of granting write that the deny entry
    /// outranks.
    #[test]
    fn a_writable_root_that_is_a_closed_folder_is_named() {
        let loc = Locations {
            fixed_drive_roots: vec![PathBuf::from(r"C:\")],
            program_data: Some(PathBuf::from(r"C:\ProgramData")),
            public: Some(PathBuf::from(r"C:\Users\Public")),
            public_children: vec![PathBuf::from(r"C:\Users\Public").join("Documents")],
            windows: Some(PathBuf::from(r"C:\Windows")),
        };
        // Built with `join`, so the test reads the same separator on every
        // host, as the folder list does.
        let closed = [
            PathBuf::from(r"C:\"),
            PathBuf::from(r"C:\ProgramData"),
            PathBuf::from(r"c:\programdata"),
            PathBuf::from(r"C:\Users\Public"),
            PathBuf::from(r"C:\Users\Public").join("Documents"),
            PathBuf::from(r"C:\Windows").join("Temp"),
        ];
        for closed in closed {
            assert!(
                closed_folder_of(&closed, &loc).is_some(),
                "{}",
                closed.display()
            );
        }
        for open in [
            PathBuf::from(r"C:\ProgramData").join("kGraf"),
            PathBuf::from(r"C:\Users").join("someone"),
            PathBuf::from(r"D:\"),
            PathBuf::from(r"C:\Windows"),
        ] {
            assert_eq!(closed_folder_of(&open, &loc), None, "{}", open.display());
        }
    }

    #[test]
    fn the_record_folders_base_must_be_a_plain_local_path() {
        assert_eq!(record_base_shape(r"C:\ProgramData"), None);
        assert_eq!(record_base_shape(r"D:\"), None);
        assert_eq!(record_base_shape(r"C:/ProgramData"), None);
        for bad in [
            r"\\server\share",
            r"ProgramData",
            r"C:ProgramData",
            r"C:\ProgramData:stream",
            r"C:\ProgramData\..\Users",
            r"C:\ProgramData\.",
            r"C:\\ProgramData",
            "",
            "C:",
        ] {
            assert!(record_base_shape(bad).is_some(), "{bad:?} must be refused");
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

    // ---- check_self_acl, on lists built by hand ----

    /// `S-1-5-21-1-2-3-1001`, a local user, as a binary SID.
    const USER_SID: [u8; 28] = [
        1, 5, 0, 0, 0, 0, 0, 5, 21, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0, 0xe9, 3, 0, 0,
    ];
    const SYNCHRONIZE: u32 = 0x0010_0000;
    const PROCESS_VM_WRITE: u32 = 0x0020;
    const FORBIDDEN: u32 = PROCESS_VM_WRITE | 0x0001;

    /// An entry of `ace_type` with `body` (everything after the header).
    fn entry(ace_type: u8, body: &[u8]) -> Vec<u8> {
        let size = (4 + body.len()) as u16;
        let mut out = vec![ace_type, 0];
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(body);
        out
    }

    /// A plain allow or deny entry: mask, then SID.
    fn plain(ace_type: u8, mask: u32, sid: &[u8]) -> Vec<u8> {
        let mut body = mask.to_le_bytes().to_vec();
        body.extend_from_slice(sid);
        entry(ace_type, &body)
    }

    /// An allowed-object entry: mask, flags (object type present), the
    /// object type GUID, then the SID, 20 bytes later than in a plain entry.
    fn object(mask: u32, sid: &[u8]) -> Vec<u8> {
        let mut body = mask.to_le_bytes().to_vec();
        body.extend_from_slice(&1u32.to_le_bytes());
        body.extend_from_slice(&[0xAB; 16]);
        body.extend_from_slice(sid);
        entry(0x05, &body)
    }

    fn list(entries: &[Vec<u8>]) -> Vec<u8> {
        let body: Vec<u8> = entries.concat();
        let size = (8 + body.len()) as u16;
        let mut out = vec![2, 0];
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(&body);
        out
    }

    #[test]
    fn a_runner_list_that_lets_the_user_only_wait_passes() {
        let acl = list(&[
            plain(ACE_ALLOWED, 0x001f_ffff, &SYSTEM_SID_BYTES),
            plain(ACE_ALLOWED, SYNCHRONIZE, &USER_SID),
            // A deny entry never grants, whatever its mask.
            plain(0x01, u32::MAX, &USER_SID),
        ]);
        assert_eq!(check_self_acl(&acl, FORBIDDEN), Ok(()));
    }

    #[test]
    fn a_plain_allow_entry_with_a_forbidden_right_fails() {
        let acl = list(&[
            plain(ACE_ALLOWED, 0x001f_ffff, &SYSTEM_SID_BYTES),
            plain(ACE_ALLOWED, SYNCHRONIZE | PROCESS_VM_WRITE, &USER_SID),
        ]);
        assert_eq!(
            check_self_acl(&acl, FORBIDDEN),
            Err(SelfAclProblem::Grants {
                index: 1,
                mask: PROCESS_VM_WRITE
            })
        );
    }

    /// The defect: object and callback allow entries were skipped, so one
    /// that let the user write the runner's memory passed.
    #[test]
    fn an_allow_entry_of_any_other_type_fails_whatever_it_grants() {
        for (what, bad) in [
            ("object", object(PROCESS_VM_WRITE, &USER_SID)),
            ("object, harmless mask", object(SYNCHRONIZE, &USER_SID)),
            ("callback", plain(0x09, PROCESS_VM_WRITE, &USER_SID)),
            ("callback object", plain(0x0B, PROCESS_VM_WRITE, &USER_SID)),
            ("compound", plain(0x04, PROCESS_VM_WRITE, &USER_SID)),
            ("unknown type", plain(0x7F, 0, &USER_SID)),
        ] {
            let acl = list(&[plain(ACE_ALLOWED, SYNCHRONIZE, &USER_SID), bad.clone()]);
            assert_eq!(
                check_self_acl(&acl, FORBIDDEN),
                Err(SelfAclProblem::Unchecked {
                    index: 1,
                    ace_type: bad[0]
                }),
                "{what}"
            );
        }
        for t in ACE_OTHER_ALLOW_TYPES {
            let acl = list(&[plain(t, 0, &USER_SID)]);
            assert!(check_self_acl(&acl, FORBIDDEN).is_err(), "type {t}");
        }
    }

    #[test]
    fn a_list_whose_bytes_do_not_match_its_header_fails() {
        let good = list(&[plain(ACE_ALLOWED, SYNCHRONIZE, &USER_SID)]);
        assert_eq!(check_self_acl(&good, FORBIDDEN), Ok(()));
        assert!(matches!(
            check_self_acl(&good[..good.len() - 1], FORBIDDEN),
            Err(SelfAclProblem::Malformed(_))
        ));
        assert!(matches!(
            check_self_acl(&good[..3], FORBIDDEN),
            Err(SelfAclProblem::Malformed(_))
        ));
        // Two entries announced, one present.
        let mut short = good.clone();
        short[4] = 2;
        assert!(matches!(
            check_self_acl(&short, FORBIDDEN),
            Err(SelfAclProblem::Malformed(_))
        ));
        // A SID that claims more sub-authorities than its entry holds.
        let mut sid = USER_SID;
        sid[1] = 9;
        let bad = list(&[plain(ACE_ALLOWED, SYNCHRONIZE, &sid)]);
        assert!(matches!(
            check_self_acl(&bad, FORBIDDEN),
            Err(SelfAclProblem::Malformed(_))
        ));
        // An empty list grants nothing.
        assert_eq!(check_self_acl(&list(&[]), FORBIDDEN), Ok(()));
    }
}

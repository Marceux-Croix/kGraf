//! Which program files kGraf's launcher service may be installed from: the
//! rule that no principal other than an administrator principal may replace
//! the service's own image or the runner it starts (decision 0097).
//!
//! **Why the rule exists.** The launcher service runs as LocalSystem, and the
//! owner's own unelevated account may start it, because every sandboxed
//! command triggers a start. Windows runs whatever bytes the service's image
//! path holds at the moment of the start. So whoever can replace that file,
//! or rename a folder above it and put another file in its place, runs code
//! as LocalSystem. That principal must be an administrator already, or the
//! sandbox hands out the machine.
//!
//! This is not a far-fetched case. A build of kGraf runs its executable out
//! of a build folder inside a working tree, and a sandbox user is granted
//! write access over a whole working tree by inheritance, so the sandbox
//! could come to own the very file it later starts as LocalSystem. The check
//! therefore runs before the service is installed, and it refuses the
//! install rather than warn about it.
//!
//! Everything here is the rule alone, so it is reviewable and tested on
//! every host. The Windows calls that read an owner and an access list are
//! in `windows::secure`, and the caller that walks the two paths is in
//! `windows::setup`.

use super::acl::{
    ADMINISTRATORS_SID, AceView, CREATOR_OWNER_SID, OWNER_RIGHTS_SID, SYSTEM_SID,
    TRUSTED_INSTALLER_SID, record_base_shape,
};

// ---- the rights that disqualify an entry --------------------------------------
//
// The values are the file-system access rights of `winnt.h`, repeated here by
// name and value so the policy can be read and tested on a host that is not
// Windows. A Windows-only test below holds each equal to its windows-sys
// binding, so the two cannot drift.

/// `FILE_WRITE_DATA` (`0x0002`) on a file: overwrite its bytes. On a folder
/// the same bit is `FILE_ADD_FILE`, which makes a new name and changes no
/// file that is already there.
pub const FILE_WRITE_DATA: u32 = 0x0000_0002;
/// `FILE_APPEND_DATA` (`0x0004`) on a file: add bytes to its end. On a folder
/// the same bit is `FILE_ADD_SUBDIRECTORY`.
pub const FILE_APPEND_DATA: u32 = 0x0000_0004;
/// `FILE_DELETE_CHILD` (`0x0040`) on a folder: delete a name in it, whatever
/// that child's own access list says.
pub const FILE_DELETE_CHILD: u32 = 0x0000_0040;
/// `DELETE` (`0x0001_0000`): delete the object, and rename it, which is how a
/// folder above the image is moved aside so another folder can take its name.
pub const DELETE_RIGHT: u32 = 0x0001_0000;
/// `WRITE_DAC` (`0x0004_0000`): rewrite the object's access list, and so take
/// every other right.
pub const WRITE_DAC_RIGHT: u32 = 0x0004_0000;
/// `WRITE_OWNER` (`0x0008_0000`): take ownership, and so rewrite the access
/// list whatever that list says.
pub const WRITE_OWNER_RIGHT: u32 = 0x0008_0000;
/// `GENERIC_ALL` (`0x1000_0000`): every right of the object's generic
/// mapping, which for a file or a folder is full control.
pub const GENERIC_ALL_RIGHT: u32 = 0x1000_0000;
/// `GENERIC_WRITE` (`0x4000_0000`): on a file, overwrite its bytes. On a
/// folder its mapping holds no delete and no delete-child, so it makes new
/// names only.
pub const GENERIC_WRITE_RIGHT: u32 = 0x4000_0000;

/// Each right by the name Windows gives it, for the message that says which
/// entry refused an install.
pub const RIGHT_NAMES: [(&str, u32); 8] = [
    ("FILE_WRITE_DATA", FILE_WRITE_DATA),
    ("FILE_APPEND_DATA", FILE_APPEND_DATA),
    ("FILE_DELETE_CHILD", FILE_DELETE_CHILD),
    ("DELETE", DELETE_RIGHT),
    ("WRITE_DAC", WRITE_DAC_RIGHT),
    ("WRITE_OWNER", WRITE_OWNER_RIGHT),
    ("GENERIC_ALL", GENERIC_ALL_RIGHT),
    ("GENERIC_WRITE", GENERIC_WRITE_RIGHT),
];

/// What one object on the path to the service's program is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The service's own image, or the runner it starts.
    File,
    /// A folder above one of those files.
    Folder,
    /// The root of the volume, which has no folder above it.
    VolumeRoot,
}

/// The rights that disqualify an allow entry on an object of this kind: the
/// rights that let the principal named by the entry put other bytes behind
/// the path the service starts.
///
/// **Why the kind matters.** The specific rights `0x02` and `0x04` are two
/// different things on the two kinds. On a file they are "overwrite" and
/// "append", which replace the program. On a folder they are `FILE_ADD_FILE`
/// and `FILE_ADD_SUBDIRECTORY`, which make a **new** name and cannot touch a
/// name that is already there; the child that is on the path has its own
/// entry in the walk, and that entry is judged on its own. A rule that
/// disqualified them on a folder would refuse every Windows machine, because
/// the default access list of a volume root lets any authenticated user
/// create a folder in it.
///
/// A folder is disqualified by the rights that reach a child or the folder
/// itself: delete a child, delete or rename the folder, rewrite its list,
/// take it over, or full control.
///
/// A volume root is judged by less again: no principal can delete or rename
/// a volume root, there is no folder above it to do so from, and the entries
/// of a root that reach the folders below it are inherited by those folders,
/// where this same walk reads them with their effect. What is left on a root
/// is the right to reach into an existing child.
pub fn disqualifying_rights(kind: Kind) -> u32 {
    let reaches_a_child =
        FILE_DELETE_CHILD | WRITE_DAC_RIGHT | WRITE_OWNER_RIGHT | GENERIC_ALL_RIGHT;
    match kind {
        Kind::File => {
            FILE_WRITE_DATA
                | FILE_APPEND_DATA
                | DELETE_RIGHT
                | WRITE_DAC_RIGHT
                | WRITE_OWNER_RIGHT
                | GENERIC_ALL_RIGHT
                | GENERIC_WRITE_RIGHT
        }
        Kind::Folder => reaches_a_child | DELETE_RIGHT,
        Kind::VolumeRoot => reaches_a_child,
    }
}

/// The rights of `mask` that [`disqualifying_rights`] names, by name, for a
/// refusal message. The mask itself is given as well, so a right this list
/// does not name is still visible.
pub fn rights_text(mask: u32) -> String {
    let named: Vec<&str> = RIGHT_NAMES
        .iter()
        .filter(|(_, bit)| mask & bit != 0)
        .map(|(name, _)| *name)
        .collect();
    if named.is_empty() {
        format!("{mask:#010x}")
    } else {
        format!("{} ({mask:#010x})", named.join(" | "))
    }
}

/// Whether a principal may hold a write right on the path the launcher
/// service starts: `NT AUTHORITY\SYSTEM`, `BUILTIN\Administrators` and
/// `NT SERVICE\TrustedInstaller`, which installs and services the system's
/// own programs.
///
/// A user account is never trusted here, not even the owner's own, although
/// the owner is an administrator and the setup runs elevated. The owner's
/// everyday process does not run elevated, and an entry for that account is
/// a right that process holds: it could replace the service's image with no
/// prompt, and so could anything that runs in it. The whole point of the
/// check is that an administrator principal, which means an elevated token,
/// is the only writer.
pub fn is_trusted_writer(sid: &str) -> bool {
    sid.eq_ignore_ascii_case(SYSTEM_SID)
        || sid.eq_ignore_ascii_case(ADMINISTRATORS_SID)
        || sid.eq_ignore_ascii_case(TRUSTED_INSTALLER_SID)
}

/// Whether the entry stands for whoever owns the object, rather than for a
/// principal of its own: `CREATOR OWNER`, which becomes the SID of whoever
/// creates a child, and `OWNER RIGHTS`, which says what the owner may do.
///
/// Such an entry is read as the owner's, and the owner of every object on the
/// path is checked by [`refusal`] before any entry is read. So the entry
/// grants only what an administrator principal already has. No token carries
/// either SID itself.
pub fn stands_for_the_owner(sid: &str) -> bool {
    sid.eq_ignore_ascii_case(CREATOR_OWNER_SID) || sid.eq_ignore_ascii_case(OWNER_RIGHTS_SID)
}

/// One object on the path to the program the launcher service starts, as the
/// rule reads it. The Windows side fills this in with one open of the object
/// that follows no link.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Object {
    /// The path as it is named, for the refusal message.
    pub path: String,
    pub kind: Kind,
    /// The owner's SID as text, or the reason it could not be read. An owner
    /// may rewrite an access list whatever that list says, so a reason that
    /// is not a SID refuses the object.
    pub owner: String,
    /// Whether the object is a reparse point (a junction or a symbolic link).
    pub link: bool,
    /// The object's access list. `None` is a list that is not present, which
    /// grants every principal every right.
    pub dacl: Option<Vec<AceView>>,
}

/// Whether `text` is a SID and not a reason an owner could not be read.
fn is_sid(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.starts_with("s-1-") && lower[4..].chars().all(|c| c.is_ascii_digit() || c == '-')
}

/// Why the launcher service may not be installed from a path that holds this
/// object, or `None` when the object is sound.
///
/// The order is the order of the ways an object is taken over: a link first,
/// because what a link points at is not what was checked; then the owner,
/// who may rewrite a list whatever it says; then the absence of a list,
/// which grants everything to everyone; then the first allow entry that
/// grants a disqualifying right to a principal that is not an administrator
/// principal.
///
/// A deny entry is not credited against a later allow entry. Windows
/// evaluates a list in order and per principal, so a deny can take a right
/// back, but a group membership this code cannot see decides whether it does.
/// The check fails closed instead.
pub fn refusal(o: &Object) -> Option<String> {
    if o.link {
        return Some(format!(
            "{} is a link (a junction or a symbolic link), and what a link points at can be \
             changed after this check",
            o.path
        ));
    }
    if !is_sid(&o.owner) {
        return Some(format!(
            "the owner of {} could not be read: {}",
            o.path, o.owner
        ));
    }
    if !is_trusted_writer(&o.owner) {
        return Some(format!(
            "{} is owned by {}, which is not SYSTEM, the Administrators group or \
             TrustedInstaller, and an owner may rewrite an access list whatever that list says",
            o.path, o.owner
        ));
    }
    let Some(entries) = &o.dacl else {
        return Some(format!(
            "{} has no access list, which grants every principal every right",
            o.path
        ));
    };
    let disqualifying = disqualifying_rights(o.kind);
    entries
        .iter()
        .filter(|e| !e.deny && e.mask & disqualifying != 0)
        .find(|e| !(is_trusted_writer(&e.sid) || stands_for_the_owner(&e.sid)))
        .map(|e| {
            format!(
                "{} grants {} to {}, which is not SYSTEM, the Administrators group or \
                 TrustedInstaller",
                o.path,
                rights_text(e.mask & disqualifying),
                e.sid
            )
        })
}

/// Why the launcher service may not be installed from this path, or `None`
/// when every object on it is sound. `chain` is the volume root first and the
/// file itself last, so the first refusal names the outermost object that
/// has to be put right.
pub fn first_refusal(chain: &[Object]) -> Option<String> {
    chain.iter().find_map(refusal)
}

/// Why the path's text alone rules it out, or `None` when the text is sound.
/// A path that is not on a local drive, holds a relative part or names an
/// alternate data stream cannot be judged by an access list at all: a
/// network path's list is the server's, and a relative part resolves against
/// a working folder the service does not have.
pub fn shape_refusal(path: &str) -> Option<String> {
    record_base_shape(path)
}

/// What to do about a refusal, in the same words wherever one is reported.
pub const ADVICE: &str = "Install kGraf where only administrators may write, such as a folder \
                          under Program Files, and run the setup again.";

/// The whole message of a refused install: what failed, then what to do.
pub fn refusal_message(why: &str) -> String {
    format!(
        "the launcher service runs as LocalSystem and the owner's own account may start it, so \
         kGraf refuses to install it from a path another principal can write: {why}. {ADVICE}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allow(sid: &str, mask: u32) -> AceView {
        AceView {
            deny: false,
            sid: sid.to_owned(),
            mask,
        }
    }

    fn file(path: &str, dacl: Vec<AceView>) -> Object {
        Object {
            path: path.to_owned(),
            kind: Kind::File,
            owner: ADMINISTRATORS_SID.to_owned(),
            link: false,
            dacl: Some(dacl),
        }
    }

    /// A user account, as a machine's own local users have.
    const A_LOCAL_USER: &str = "S-1-5-21-2342-9981-1077-1004";

    /// The Users group, which holds every local user of the machine, the
    /// sandbox users with them.
    const USERS_GROUP: &str = "S-1-5-32-545";
    /// Authenticated Users, which the access list of a volume root admits.
    const AUTHENTICATED_USERS: &str = "S-1-5-11";
    /// Read, list and run, as a bundle folder gives the Users group: read
    /// data, read the attributes and the extended attributes, run, list a
    /// folder, read the access list and wait on the handle. No right here
    /// changes a byte.
    const FILE_GENERIC_READ_AND_EXECUTE: u32 = 0x0012_00a9;
    /// `FILE_ADD_SUBDIRECTORY` on a folder, which is `FILE_APPEND_DATA` on a
    /// file: make a new name in the folder.
    const FILE_ADD_SUBDIRECTORY: u32 = FILE_APPEND_DATA;

    /// The three principals that may write the path, and nothing else: no
    /// user account, no Users group, no Authenticated Users.
    #[test]
    fn only_system_administrators_and_trusted_installer_may_write() {
        assert!(is_trusted_writer(SYSTEM_SID));
        assert!(is_trusted_writer(ADMINISTRATORS_SID));
        assert!(is_trusted_writer(TRUSTED_INSTALLER_SID));
        // The text form of a SID is case insensitive.
        assert!(is_trusted_writer(&SYSTEM_SID.to_ascii_lowercase()));
        for sid in [
            A_LOCAL_USER,
            "S-1-5-32-545", // Users
            "S-1-5-11",     // Authenticated Users
            "S-1-1-0",      // Everyone
            "S-1-5-32-547", // Power Users
            CREATOR_OWNER_SID,
            OWNER_RIGHTS_SID,
        ] {
            assert!(
                !is_trusted_writer(sid),
                "{sid} must not be a trusted writer"
            );
        }
        // The two stand-ins are read as the owner, which is checked on its
        // own, and are never principals of their own.
        assert!(stands_for_the_owner(CREATOR_OWNER_SID));
        assert!(stands_for_the_owner(OWNER_RIGHTS_SID));
        assert!(!stands_for_the_owner(A_LOCAL_USER));
        assert!(!stands_for_the_owner(ADMINISTRATORS_SID));
    }

    /// The shape of a hosted test machine, read from what such a machine
    /// has: the program in a folder of its own below the volume root, with an
    /// access list of full control for SYSTEM and the Administrators group
    /// and read and execute for the Users group, and a volume root whose own
    /// list lets any authenticated user make a name in it.
    ///
    /// Two things this pins. The root passes, because making a new name in a
    /// folder reaches no name that is already there. The folder passes only
    /// when its owner is a trusted principal: a folder a job's own account
    /// created is owned by that account until the owner is set, and an owner
    /// may rewrite an access list whatever that list says, so the program
    /// could be replaced and the launcher service would start it as the local
    /// system account.
    #[test]
    fn a_program_folder_of_a_hosted_machine_passes_only_when_its_owner_is_trusted() {
        // `C:\`: full control for SYSTEM and the Administrators group, an
        // entry for whoever creates a child, and, for any authenticated
        // user, the right to make a folder in the root and the rights that
        // reach only the children it makes.
        let volume_root = Object {
            path: r"C:\".to_owned(),
            kind: Kind::VolumeRoot,
            owner: SYSTEM_SID.to_owned(),
            link: false,
            dacl: Some(vec![
                allow(SYSTEM_SID, GENERIC_ALL_RIGHT),
                allow(ADMINISTRATORS_SID, GENERIC_ALL_RIGHT),
                allow(CREATOR_OWNER_SID, GENERIC_ALL_RIGHT),
                // Authenticated Users: make a folder in the root, and the
                // modify rights that Windows marks inherit only. Neither
                // reaches a name that is already there.
                allow(
                    AUTHENTICATED_USERS,
                    FILE_ADD_SUBDIRECTORY | FILE_APPEND_DATA | DELETE_RIGHT,
                ),
            ]),
        };
        assert_eq!(refusal(&volume_root), None, "the volume root must pass");

        let bundle_list = vec![
            allow(SYSTEM_SID, GENERIC_ALL_RIGHT),
            allow(ADMINISTRATORS_SID, GENERIC_ALL_RIGHT),
            // The Users group, which holds every local user and so every
            // sandbox user: read and execute, and nothing more.
            allow(USERS_GROUP, FILE_GENERIC_READ_AND_EXECUTE),
        ];
        let program = |owner: &str| Object {
            path: r"C:\kgraf-wintest\kgraf.exe".to_owned(),
            kind: Kind::File,
            owner: owner.to_owned(),
            link: false,
            dacl: Some(bundle_list.clone()),
        };
        let folder = |owner: &str| Object {
            path: r"C:\kgraf-wintest".to_owned(),
            kind: Kind::Folder,
            owner: owner.to_owned(),
            link: false,
            dacl: Some(bundle_list.clone()),
        };

        // Owned by the account that made the folder: refused, and the
        // message names that account and the path.
        let as_made = [
            volume_root.clone(),
            folder(A_LOCAL_USER),
            program(A_LOCAL_USER),
        ];
        let why = first_refusal(&as_made).expect("a folder owned by a user must be refused");
        assert!(why.contains(r"C:\kgraf-wintest"), "{why}");
        assert!(why.contains(A_LOCAL_USER), "{why}");
        assert!(why.contains("an owner may rewrite an access list"), "{why}");

        // Owner set to the Administrators group, for the folder and for the
        // program in it: the whole path passes.
        let owned_by_administrators = [
            volume_root,
            folder(ADMINISTRATORS_SID),
            program(ADMINISTRATORS_SID),
        ];
        assert_eq!(
            first_refusal(&owned_by_administrators),
            None,
            "the path must pass once the owner is the Administrators group"
        );
    }

    /// Every right that replaces the bytes of the program disqualifies an
    /// entry on the file, and the read rights do not.
    #[test]
    fn the_rights_that_replace_a_program_disqualify_an_entry_on_it() {
        let m = disqualifying_rights(Kind::File);
        for (name, bit) in RIGHT_NAMES {
            // `FILE_DELETE_CHILD` is a folder's right alone: the bit names no
            // access on a file.
            if bit == FILE_DELETE_CHILD {
                continue;
            }
            assert_eq!(m & bit, bit, "{name} must disqualify an entry on a file");
        }
        // Every right this file names is disqualifying on one kind or the
        // other, so none is listed and then ignored.
        for (name, bit) in RIGHT_NAMES {
            let on_any = [Kind::File, Kind::Folder, Kind::VolumeRoot]
                .into_iter()
                .any(|k| disqualifying_rights(k) & bit != 0);
            assert!(on_any, "{name} disqualifies nothing");
        }
        // Reading, listing, running and reading a list are not write rights.
        for (name, bit) in [
            ("FILE_READ_DATA", 0x0000_0001),
            ("FILE_READ_EA", 0x0000_0008),
            ("FILE_EXECUTE", 0x0000_0020),
            ("FILE_READ_ATTRIBUTES", 0x0000_0080),
            ("READ_CONTROL", 0x0002_0000),
            ("SYNCHRONIZE", 0x0010_0000),
            ("GENERIC_READ", 0x8000_0000u32),
            ("GENERIC_EXECUTE", 0x2000_0000),
        ] {
            assert_eq!(m & bit, 0, "{name} must not disqualify an entry");
        }
        // A read-and-run entry for the machine's users is what a program
        // folder carries, and it is sound.
        assert_eq!(
            refusal(&file(
                r"C:\Program Files\kGraf\kgraf.exe",
                vec![
                    allow(SYSTEM_SID, 0x001f_01ff),
                    allow(ADMINISTRATORS_SID, 0x001f_01ff),
                    allow("S-1-5-32-545", 0x0012_00a9),
                ],
            )),
            None
        );
    }

    /// Making a new name in a folder is not the same as changing the name
    /// that is already there. The default access list of a volume root grants
    /// every authenticated user the right to create a folder in it, so a rule
    /// that read that as write access would refuse every machine.
    #[test]
    fn creating_a_name_in_a_folder_is_not_write_access_to_the_path() {
        let folder = disqualifying_rights(Kind::Folder);
        let root = disqualifying_rights(Kind::VolumeRoot);
        for (name, bit) in [
            ("FILE_ADD_FILE", FILE_WRITE_DATA),
            ("FILE_ADD_SUBDIRECTORY", FILE_APPEND_DATA),
            ("GENERIC_WRITE", GENERIC_WRITE_RIGHT),
        ] {
            assert_eq!(folder & bit, 0, "{name} on a folder makes a new name only");
            assert_eq!(root & bit, 0, "{name} on a root makes a new name only");
        }
        // What does reach the child on the path, or the folder itself.
        for (name, bit) in [
            ("FILE_DELETE_CHILD", FILE_DELETE_CHILD),
            ("WRITE_DAC", WRITE_DAC_RIGHT),
            ("WRITE_OWNER", WRITE_OWNER_RIGHT),
            ("GENERIC_ALL", GENERIC_ALL_RIGHT),
        ] {
            assert_eq!(folder & bit, bit, "{name} must disqualify a folder entry");
            assert_eq!(root & bit, bit, "{name} must disqualify a root entry");
        }
        // A folder can be renamed out of the way by a principal that may
        // delete it; a volume root cannot, and has no folder above it.
        assert_eq!(folder & DELETE_RIGHT, DELETE_RIGHT);
        assert_eq!(root & DELETE_RIGHT, 0);
    }

    /// The path a build runs from: a sandbox user holds an inherited write
    /// entry over the whole tree, so the install is refused and the message
    /// names the folder and the entry.
    #[test]
    fn a_tree_a_sandbox_user_may_write_refuses_the_install() {
        let chain = vec![
            Object {
                path: r"C:\".to_owned(),
                kind: Kind::VolumeRoot,
                owner: SYSTEM_SID.to_owned(),
                link: false,
                dacl: Some(vec![
                    allow(SYSTEM_SID, 0x001f_01ff),
                    allow(ADMINISTRATORS_SID, 0x001f_01ff),
                    // Authenticated Users may create a folder in the root.
                    allow("S-1-5-11", FILE_APPEND_DATA),
                ]),
            },
            Object {
                path: r"C:\work\kgraf\target\debug".to_owned(),
                kind: Kind::Folder,
                owner: ADMINISTRATORS_SID.to_owned(),
                link: false,
                dacl: Some(vec![
                    allow(SYSTEM_SID, 0x001f_01ff),
                    allow(A_LOCAL_USER, 0x001f_01ff),
                ]),
            },
            file(
                r"C:\work\kgraf\target\debug\kgraf.exe",
                vec![allow(SYSTEM_SID, 0x001f_01ff)],
            ),
        ];
        let why = first_refusal(&chain).expect("the tree is writable, so it must be refused");
        assert!(why.contains(r"C:\work\kgraf\target\debug"), "{why}");
        assert!(why.contains(A_LOCAL_USER), "{why}");
        assert!(why.contains("FILE_DELETE_CHILD"), "{why}");
        // The outermost object that is sound does not appear.
        assert!(!why.contains(r"C:\ "), "{why}");
        // The whole message says what to do.
        let message = refusal_message(&why);
        assert!(message.contains("LocalSystem"), "{message}");
        assert!(
            message.contains("Install kGraf where only administrators may write"),
            "{message}"
        );
    }

    /// The first object that fails is the one named, and the walk is the
    /// volume root first, so a message names the outermost path to put right.
    #[test]
    fn the_first_failing_path_is_the_one_named() {
        let bad = |path: &str, kind: Kind| Object {
            path: path.to_owned(),
            kind,
            owner: ADMINISTRATORS_SID.to_owned(),
            link: false,
            dacl: Some(vec![allow(A_LOCAL_USER, GENERIC_ALL_RIGHT)]),
        };
        let chain = vec![
            bad(r"C:\apps", Kind::Folder),
            bad(r"C:\apps\kgraf", Kind::Folder),
            bad(r"C:\apps\kgraf\kgraf.exe", Kind::File),
        ];
        let why = first_refusal(&chain).expect("every object is writable");
        assert!(why.starts_with(r"C:\apps grants"), "{why}");
        assert_eq!(first_refusal(&[]), None);
    }

    /// An owner may rewrite an access list whatever that list says, so an
    /// owner that is not an administrator principal refuses the object even
    /// when its list admits no one else. An owner that could not be read
    /// refuses it as well: the check fails closed.
    #[test]
    fn an_owner_that_is_not_an_administrator_refuses_the_object() {
        let mut o = file(
            r"C:\apps\kgraf\kgraf.exe",
            vec![allow(SYSTEM_SID, 0x001f_01ff)],
        );
        o.owner = A_LOCAL_USER.to_owned();
        let why = refusal(&o).expect("an owner that is a user account must be refused");
        assert!(why.contains("is owned by"), "{why}");
        assert!(why.contains(A_LOCAL_USER), "{why}");
        for sid in [SYSTEM_SID, ADMINISTRATORS_SID, TRUSTED_INSTALLER_SID] {
            o.owner = sid.to_owned();
            assert_eq!(refusal(&o), None, "{sid} owns the system's own programs");
        }
        o.owner = "its owner could not be read: Access is denied".to_owned();
        let why = refusal(&o).expect("an owner that could not be read must be refused");
        assert!(why.contains("could not be read"), "{why}");
    }

    /// A list that is not present grants everything to everyone, and a link
    /// is not what was checked. Both refuse.
    #[test]
    fn an_absent_list_and_a_link_refuse_the_object() {
        let mut o = file(r"C:\apps\kgraf\kgraf.exe", Vec::new());
        o.dacl = None;
        let why = refusal(&o).expect("an absent list must be refused");
        assert!(why.contains("no access list"), "{why}");
        let mut o = file(
            r"C:\apps\kgraf\kgraf.exe",
            vec![allow(SYSTEM_SID, 0x001f_01ff)],
        );
        o.link = true;
        let why = refusal(&o).expect("a link must be refused");
        assert!(why.contains("is a link"), "{why}");
    }

    /// A deny entry is not credited against a later allow entry, and an
    /// entry of a type this code cannot read counts as a grant of everything
    /// (`windows::secure` reports it with every bit set).
    #[test]
    fn a_deny_entry_does_not_excuse_a_later_allow_entry() {
        let o = file(
            r"C:\apps\kgraf\kgraf.exe",
            vec![
                AceView {
                    deny: true,
                    sid: A_LOCAL_USER.to_owned(),
                    mask: u32::MAX,
                },
                allow(A_LOCAL_USER, FILE_WRITE_DATA),
            ],
        );
        let why = refusal(&o).expect("the allow entry must still refuse");
        assert!(why.contains("FILE_WRITE_DATA"), "{why}");
        let unknown = file(
            r"C:\apps\kgraf\kgraf.exe",
            vec![allow("an access entry of type 9", u32::MAX)],
        );
        assert!(
            refusal(&unknown).is_some(),
            "an unreadable entry must refuse"
        );
    }

    /// A `CREATOR OWNER` or `OWNER RIGHTS` entry is the owner's, and the
    /// owner is checked on its own, so such an entry does not refuse a sound
    /// object. Program folders carry both.
    #[test]
    fn an_owner_stand_in_entry_does_not_refuse_a_sound_object() {
        assert_eq!(
            refusal(&Object {
                path: r"C:\Program Files\kGraf".to_owned(),
                kind: Kind::Folder,
                owner: SYSTEM_SID.to_owned(),
                link: false,
                dacl: Some(vec![
                    allow(TRUSTED_INSTALLER_SID, GENERIC_ALL_RIGHT),
                    allow(CREATOR_OWNER_SID, GENERIC_ALL_RIGHT),
                    allow(OWNER_RIGHTS_SID, WRITE_DAC_RIGHT),
                    allow("S-1-5-32-545", 0x0012_00a9),
                ]),
            }),
            None
        );
    }

    /// A path whose text cannot be judged by an access list is ruled out by
    /// its text: a network path, a relative part, an alternate data stream.
    #[test]
    fn a_path_an_access_list_cannot_judge_is_ruled_out_by_its_text() {
        assert_eq!(shape_refusal(r"C:\Program Files\kGraf\kgraf.exe"), None);
        for bad in [
            r"\\server\share\kgraf.exe",
            r"C:\apps\..\kgraf.exe",
            r"C:\apps\kgraf.exe:stream",
            r"kgraf.exe",
        ] {
            assert!(shape_refusal(bad).is_some(), "{bad} must be ruled out");
        }
    }

    /// The names in a refusal are the rights that refused it, so a reader can
    /// see which entry to take off.
    #[test]
    fn the_rights_text_names_each_right_it_holds() {
        let text = rights_text(FILE_WRITE_DATA | DELETE_RIGHT);
        assert!(text.contains("FILE_WRITE_DATA"), "{text}");
        assert!(text.contains("DELETE"), "{text}");
        assert!(text.contains("0x00010002"), "{text}");
        // A mask with no right this list names is still shown.
        assert_eq!(rights_text(0x0000_0001), "0x00000001");
    }

    /// The values repeated here must be the ones windows-sys binds, or the
    /// rule reviewed on a host is not the rule applied on Windows.
    #[cfg(windows)]
    #[test]
    fn the_repeated_values_match_the_windows_sys_bindings() {
        use windows_sys::Win32::Storage::FileSystem as fs;
        assert_eq!(FILE_WRITE_DATA, fs::FILE_WRITE_DATA);
        assert_eq!(FILE_APPEND_DATA, fs::FILE_APPEND_DATA);
        assert_eq!(FILE_DELETE_CHILD, fs::FILE_DELETE_CHILD);
        assert_eq!(DELETE_RIGHT, fs::DELETE);
        assert_eq!(WRITE_DAC_RIGHT, fs::WRITE_DAC);
        assert_eq!(WRITE_OWNER_RIGHT, fs::WRITE_OWNER);
        assert_eq!(
            GENERIC_ALL_RIGHT,
            windows_sys::Win32::Foundation::GENERIC_ALL
        );
        assert_eq!(
            GENERIC_WRITE_RIGHT,
            windows_sys::Win32::Foundation::GENERIC_WRITE
        );
    }
}

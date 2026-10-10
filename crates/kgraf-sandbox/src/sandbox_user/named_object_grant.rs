//! The fallback that lets a sandboxed command make a named kernel object in
//! the owner's own Windows session: which rights the sandbox group is given,
//! which rights it must never be given, where the object directory is, and
//! the one text kGraf says about the grant (decision 0097).
//!
//! **Why a grant is needed at all.** Every Win32 call for a named mutex,
//! semaphore, event or section resolves the name below one object directory
//! that `kernelbase.dll` opens once per process, with explicit rights, before
//! it looks up any name. In the owner's session that directory is
//! `\Sessions\<n>\BaseNamedObjects`. Windows admits the session's logon SID
//! there, and a sandboxed command holds that SID deny-only, so every such
//! call is refused: cargo cannot make its jobserver semaphore and rustc
//! cannot make the mutex it takes for every diagnostic it prints.
//!
//! **This is the fallback, not the fix.** The fix is kGraf's launcher
//! service, which starts the command in session 0, where an unprefixed name
//! resolves below the machine-wide `\BaseNamedObjects` and no session logon
//! SID is in the way ([`super::session0`]). This grant is what a machine gets
//! where that service cannot be set up or does not prove itself. It is a
//! weaker boundary, so it is recorded, it is explained in the same words
//! everywhere ([`EXPLANATION`]), and `kgraf sandbox remove` takes it off.
//!
//! Everything here is platform-neutral, so the rights and the words are
//! tested on every host. The Windows calls that read and write the
//! directory's access list are in `windows::named_object_dir`.

use super::acl::{READ_CONTROL_RIGHT, WRITE_DAC_RIGHT};

// ---- the rights of an object directory ---------------------------------------
//
// The four values below are the access rights of an object-directory object.
// They are defined in the Windows Driver Kit header `ntifs.h`, and
// windows-sys binds them as `Wdk::System::SystemServices::DIRECTORY_*`. They
// are repeated here, by name and value, so the mask kGraf writes can be read
// and tested on a host that is not Windows; a Windows-only test below holds
// each equal to its windows-sys binding, so the two cannot drift.

/// `DIRECTORY_QUERY` (`0x0001`): list the names in the directory. Part of the
/// set `kernelbase.dll` opens the session directory with before it looks up
/// any name.
pub const DIRECTORY_QUERY: u32 = 0x0000_0001;
/// `DIRECTORY_TRAVERSE` (`0x0002`): look a name up in the directory. Without
/// it no named object can be opened, whatever that object's own access list
/// says.
pub const DIRECTORY_TRAVERSE: u32 = 0x0000_0002;
/// `DIRECTORY_CREATE_OBJECT` (`0x0004`): make an object in the directory.
/// cargo's jobserver semaphore and rustc's `rustc_errors` mutex need it.
pub const DIRECTORY_CREATE_OBJECT: u32 = 0x0000_0004;
/// `DIRECTORY_CREATE_SUBDIRECTORY` (`0x0008`): make a subdirectory in the
/// directory. `kernelbase.dll` asks for it in the same open, so a grant
/// without it leaves that open refused.
pub const DIRECTORY_CREATE_SUBDIRECTORY: u32 = 0x0000_0008;

/// `DELETE` (`0x0001_0000`), a standard right: delete the object.
pub const DELETE_RIGHT: u32 = 0x0001_0000;
/// `WRITE_OWNER` (`0x0008_0000`), a standard right: take ownership of the
/// object. An owner may rewrite an access list whatever that list says.
pub const WRITE_OWNER_RIGHT: u32 = 0x0008_0000;
/// `GENERIC_ALL` (`0x1000_0000`): every right the object's generic mapping
/// gives, which for an object directory is all four rights above and every
/// standard right.
pub const GENERIC_ALL_RIGHT: u32 = 0x1000_0000;

/// The mask kGraf adds for the sandbox group, and nothing more: the four
/// object-directory rights a command needs to look a name up and make one,
/// plus `READ_CONTROL` so a command can read the list it was admitted to and
/// a measurement can be taken from inside the sandbox.
///
/// It carries no `WRITE_DAC`, no `WRITE_OWNER`, no `DELETE` and no
/// `GENERIC_ALL` ([`FORBIDDEN_RIGHTS`]): a sandboxed command may put a name in
/// this directory, and may not change who else is admitted to it, take it over
/// or remove it.
pub const GRANT_MASK: u32 = DIRECTORY_QUERY
    | DIRECTORY_TRAVERSE
    | DIRECTORY_CREATE_OBJECT
    | DIRECTORY_CREATE_SUBDIRECTORY
    | READ_CONTROL_RIGHT;

/// The rights the grant must never carry. Each would let a sandboxed command
/// change the directory itself, not only put a name in it.
pub const FORBIDDEN_RIGHTS: u32 =
    WRITE_DAC_RIGHT | WRITE_OWNER_RIGHT | DELETE_RIGHT | GENERIC_ALL_RIGHT;

/// Each part of [`GRANT_MASK`] with the name Windows gives it, for a
/// measurement line and for a review of what was written.
pub const MASK_PARTS: [(&str, u32); 5] = [
    ("DIRECTORY_QUERY", DIRECTORY_QUERY),
    ("DIRECTORY_TRAVERSE", DIRECTORY_TRAVERSE),
    ("DIRECTORY_CREATE_OBJECT", DIRECTORY_CREATE_OBJECT),
    (
        "DIRECTORY_CREATE_SUBDIRECTORY",
        DIRECTORY_CREATE_SUBDIRECTORY,
    ),
    ("READ_CONTROL", READ_CONTROL_RIGHT),
];

/// [`GRANT_MASK`] in words: the mask, then each right by name. For the line a
/// setup step or a status report prints beside [`EXPLANATION`].
pub fn mask_text() -> String {
    let names: Vec<&str> = MASK_PARTS.iter().map(|(name, _)| *name).collect();
    format!("{GRANT_MASK:#010x} ({})", names.join(" | "))
}

// ---- where the directory is ---------------------------------------------------

/// The object directory an unprefixed named object of a process in `session`
/// resolves below: `\Sessions\<n>\BaseNamedObjects`, and for session 0 the
/// machine-wide `\BaseNamedObjects`.
///
/// Session 0 has no `\Sessions\0` directory of this kind. Microsoft documents
/// the two namespaces ("Kernel Object Namespaces": a service application uses
/// the global namespace by default, and each session has a namespace of its
/// own), and the `\Sessions\<n>` form of the per-session name is **inferred**
/// from the object namespace as `winobj` and the ReactOS and Wine object
/// managers lay it out; Microsoft does not give the path in text. A process in
/// session 0 therefore names the same directory on both launch paths, which is
/// why the first thing a Windows run reports is its own session
/// (`windows::named_object_dir::session_of_current_process`).
///
/// This function lives in the platform-neutral file, not beside the Windows
/// calls, because the name is a string and nothing else: putting it here lets
/// every host test it.
pub fn directory_path(session: u32) -> String {
    if session == 0 {
        r"\BaseNamedObjects".to_owned()
    } else {
        format!(r"\Sessions\{session}\BaseNamedObjects")
    }
}

// ---- the one text about the grant ---------------------------------------------

/// What kGraf says about this grant, in one place, so the end of
/// `kgraf sandbox setup`, `kgraf sandbox status`, `kgraf sandbox selfcheck`
/// and `kgraf sandbox remove` say the same thing. It names what the grant
/// allows and the risk it carries.
///
/// The one-line form a status line carries is
/// `super::session0::Active::line`; this is the longer explanation a setup end
/// or a status detail prints under it.
pub const EXPLANATION: &str = "\
The sandbox group is admitted to the object directory of the owner's Windows session \
with four rights: look a name up in it, list it, make an object in it and make a \
subdirectory in it, and with the right to read the directory's own access list. \
Without them a sandboxed command can make or open no named mutex, semaphore, event or \
section, so cargo and rustc cannot run as a sandbox user. The grant allows nothing \
else: it cannot rewrite the directory's access list, take its ownership or delete it. \
The risk: a sandboxed command can then put a named object in the directory the owner's \
own programs read, so a program of the owner can be made to open an object a sandboxed \
command made instead of its own. The same two rights that let it make an object and a \
subdirectory let it make an object symbolic link, so it can also redirect a name the \
owner's programs open to an object of its own choice, and not only occupy that name. \
This is the fallback for a machine where kGraf's \
launcher service, which starts a sandboxed command in session 0 and needs no grant, \
cannot be set up or does not prove itself. Windows makes the session's object directory \
again at every logon, so the grant is gone at the owner's next sign-out whether or not \
`kgraf sandbox remove` ran; remove takes it off as well.";

#[cfg(test)]
mod tests {
    use super::*;

    /// The mask is exactly the four object-directory rights and
    /// `READ_CONTROL`, and carries none of the rights that would let a
    /// sandboxed command change the directory itself.
    #[test]
    fn the_grant_mask_is_the_four_directory_rights_and_read_control() {
        assert_eq!(GRANT_MASK, 0x0002_000f, "{GRANT_MASK:#010x}");
        assert_eq!(
            GRANT_MASK,
            DIRECTORY_QUERY
                | DIRECTORY_TRAVERSE
                | DIRECTORY_CREATE_OBJECT
                | DIRECTORY_CREATE_SUBDIRECTORY
                | READ_CONTROL_RIGHT
        );
        // Each part is in, and the mask holds nothing else.
        let mut rest = GRANT_MASK;
        for (name, bit) in MASK_PARTS {
            assert_eq!(GRANT_MASK & bit, bit, "{name} is missing");
            rest &= !bit;
        }
        assert_eq!(rest, 0, "the mask holds {rest:#010x} beyond its parts");
    }

    /// The rights that would make the grant a way to change the directory,
    /// each named, each absent.
    #[test]
    fn the_grant_mask_cannot_change_the_directory() {
        assert_eq!(GRANT_MASK & WRITE_DAC_RIGHT, 0, "WRITE_DAC");
        assert_eq!(GRANT_MASK & WRITE_OWNER_RIGHT, 0, "WRITE_OWNER");
        assert_eq!(GRANT_MASK & DELETE_RIGHT, 0, "DELETE");
        assert_eq!(GRANT_MASK & GENERIC_ALL_RIGHT, 0, "GENERIC_ALL");
        assert_eq!(GRANT_MASK & FORBIDDEN_RIGHTS, 0);
        // The documented values, so a typo in one of them is caught here and
        // not on Windows.
        assert_eq!(
            [
                DIRECTORY_QUERY,
                DIRECTORY_TRAVERSE,
                DIRECTORY_CREATE_OBJECT,
                DIRECTORY_CREATE_SUBDIRECTORY
            ],
            [1, 2, 4, 8]
        );
        assert_eq!(READ_CONTROL_RIGHT, 0x0002_0000);
        assert_eq!(WRITE_DAC_RIGHT, 0x0004_0000);
        assert_eq!(DELETE_RIGHT, 0x0001_0000);
        assert_eq!(WRITE_OWNER_RIGHT, 0x0008_0000);
        assert_eq!(GENERIC_ALL_RIGHT, 0x1000_0000);
    }

    /// The values repeated here must be the ones windows-sys binds, or the
    /// mask written on Windows is not the mask this test reviewed.
    #[cfg(windows)]
    #[test]
    fn the_repeated_values_match_the_windows_sys_bindings() {
        use windows_sys::Wdk::System::SystemServices as wdk;
        assert_eq!(DIRECTORY_QUERY, wdk::DIRECTORY_QUERY);
        assert_eq!(DIRECTORY_TRAVERSE, wdk::DIRECTORY_TRAVERSE);
        assert_eq!(DIRECTORY_CREATE_OBJECT, wdk::DIRECTORY_CREATE_OBJECT);
        assert_eq!(
            DIRECTORY_CREATE_SUBDIRECTORY,
            wdk::DIRECTORY_CREATE_SUBDIRECTORY
        );
        assert_eq!(
            READ_CONTROL_RIGHT,
            windows_sys::Win32::Storage::FileSystem::READ_CONTROL
        );
    }

    #[test]
    fn the_mask_text_names_every_right() {
        let text = mask_text();
        assert!(text.contains("0x0002000f"), "{text}");
        for (name, _) in MASK_PARTS {
            assert!(text.contains(name), "{name} missing from {text}");
        }
    }

    /// Session 0 names the machine-wide directory, every other session its
    /// own. A grant on one is no grant on the other.
    #[test]
    fn the_directory_path_is_the_machine_wide_one_in_session_zero() {
        assert_eq!(directory_path(0), r"\BaseNamedObjects");
        assert_eq!(directory_path(1), r"\Sessions\1\BaseNamedObjects");
        assert_eq!(directory_path(2), r"\Sessions\2\BaseNamedObjects");
        assert_eq!(
            directory_path(4_294_967_295),
            r"\Sessions\4294967295\BaseNamedObjects"
        );
        assert_ne!(directory_path(0), directory_path(1));
    }

    /// The explanation must say what the grant allows and the risk it
    /// carries, in the words every surface repeats. A fallback that is
    /// weaker than the path it replaces is always said (fail loud).
    #[test]
    fn the_explanation_says_what_it_allows_and_what_it_risks() {
        for needle in [
            // what it allows
            "look a name up",
            "make an object in it",
            "read the directory's own access list",
            // what it does not allow
            "cannot rewrite the directory's access list",
            // the risk, in the same words as the one-line form
            "The risk",
            "the directory the owner's own programs read",
            // the risk is a redirect, not only an occupied name: with
            // create-object and create-subdirectory the command can make an
            // object symbolic link as well
            "object symbolic link",
            "redirect a name the owner's programs open",
            "not only occupy that name",
            // why it is only the fallback, and that it is removable
            "launcher service",
            "kgraf sandbox remove",
            "at every logon",
        ] {
            assert!(
                EXPLANATION.contains(needle),
                "the grant explanation does not say {needle:?}"
            );
        }
        assert!(!EXPLANATION.contains('\n'), "one paragraph, no line breaks");
    }

    /// The explanation and the one-line form of the active path must not
    /// disagree about the risk: both name the directory the owner's own
    /// programs read. The line is taken through `select`, the one place that
    /// decides which path is active, so this test needs no field of
    /// `Active`.
    #[test]
    fn the_explanation_and_the_status_line_name_the_same_risk() {
        use super::super::session0::{Attempt, select};
        let line = select(&Attempt {
            service_failed: Some("the service could not be set up".into()),
            granted: true,
            ..Attempt::default()
        })
        .line();
        let risk = "named object in the directory the owner's own programs read";
        assert!(line.contains(risk), "{line}");
        assert!(EXPLANATION.contains(risk), "{EXPLANATION}");
    }
}

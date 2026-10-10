//! A jobserver for the cargo and rustc a sandbox user runs (decision 0097).
//!
//! **Why.** cargo and rustc share build parallelism through a jobserver. With
//! none in the environment each makes its own: on Windows the `jobserver`
//! crate creates a named semaphore with no prefix, `CreateSemaphoreA(...,
//! "__rust_jobserver_semaphore_<n>")`, which puts it in the session's
//! `BaseNamedObjects` directory. A sandbox user's command is refused there:
//! the session's directory admits the session's logon SID, and the runner
//! makes that SID deny-only in the command's token (it would otherwise admit
//! the command to the owner's own objects). A hosted Windows self-test showed
//! it on every cargo build the sandbox user ran: rustc stopped at its start
//! with "failed to create jobserver: Os { code: 5, kind: PermissionDenied }"
//! (rustc_data_structures `jobserver.rs`) and the status 0xC0000409 of its
//! abort, and cargo reported "failed to run `rustc` to learn about
//! target-specific information".
//!
//! **What.** kGraf's own process, which the session admits, creates the
//! semaphore for each command, with an access list that admits SYSTEM and
//! the owner fully and the command's sandbox user to wait on it and release
//! it (`SYNCHRONIZE | SEMAPHORE_MODIFY_STATE`, what the `jobserver` crate's
//! `OpenSemaphoreA` asks for), and names it in `CARGO_MAKEFLAGS` the way
//! cargo names its own to the processes it starts. cargo, rustc and the `cc`
//! crate read that variable first (`jobserver::Client::from_env`), open the
//! semaphore by name, and create none. The semaphore lives as long as the
//! command: the started child holds it.
//!
//! **Measured: this is not enough.** The open was expected to need no right
//! on the session's directory beyond traversal, which every token's
//! `SeChangeNotifyPrivilege` gives. A hosted Windows self-test refused it
//! ("cannot open path or name kgraf-jobserver-... Access is denied"):
//! `kernelbase.dll` opens the session's directory itself, with explicit
//! rights, before it looks up any name, and the directory refuses the
//! command. rustc also makes a named mutex of its own to print any
//! diagnostic, so a jobserver alone cannot make a build run. The cause, and
//! the note a failed command's output gets, are in
//! [`super::named_objects`]. The semaphore is still made: it costs one
//! handle, and where the directory admits the command it saves cargo from
//! making its own.
//!
//! The command can do nothing with the semaphore but take and give back its
//! tokens, and no other program is admitted to it. The self-test measures
//! the open, a named object of the command's own in the session and in the
//! global namespace, and the session's directory
//! (`the_sandbox_user_opens_its_jobserver_and_named_objects_are_measured` in
//! `tests/escape_windows.rs`).
//!
//! Platform-neutral: the name, the variable's value, the access list and the
//! token count are tested on every host. The Windows call is in
//! `windows::jobserver`.

/// The variable cargo and rustc read first for a jobserver to join.
pub const JOBSERVER_VAR: &str = "CARGO_MAKEFLAGS";

/// `SYNCHRONIZE | SEMAPHORE_MODIFY_STATE`: what the `jobserver` crate opens
/// a named semaphore with (`OpenSemaphoreA`), and all the sandbox user is
/// admitted to.
pub const USER_SEMAPHORE_ACCESS: u32 = 0x0010_0000 | 0x0002;

/// The semaphore's name for `nonce`: `kgraf-jobserver-` and sixteen hex
/// digits. No `Global\` or `Local\` prefix: the session's own namespace,
/// where the command, started in the owner's session, looks it up.
pub fn semaphore_name(nonce: [u8; 8]) -> String {
    let hex: String = nonce.iter().map(|b| format!("{b:02x}")).collect();
    format!("kgraf-jobserver-{hex}")
}

/// The value of [`JOBSERVER_VAR`] that names the semaphore `name`, in the
/// form cargo itself writes for the processes it starts (`-j`, then the
/// name under both of the flags the `jobserver` crate reads).
pub fn makeflags(name: &str) -> String {
    format!("-j --jobserver-fds={name} --jobserver-auth={name}")
}

/// The semaphore's access list in SDDL: SYSTEM and the owner fully, the
/// sandbox user `user_sid` to [`USER_SEMAPHORE_ACCESS`], nothing inherited
/// and nobody else. Both SIDs come from Windows' own SID-to-string
/// conversion, so neither holds SDDL syntax characters.
pub fn semaphore_sddl(owner_sid: &str, user_sid: &str) -> String {
    format!("D:P(A;;GA;;;SY)(A;;GA;;;{owner_sid})(A;;0x{USER_SEMAPHORE_ACCESS:08x};;;{user_sid})")
}

/// How many tokens the semaphore holds: the machine's parallelism, at
/// least one. cargo's own jobserver holds as many as it runs jobs.
pub fn tokens(parallelism: usize) -> i32 {
    i32::try_from(parallelism.max(1)).unwrap_or(i32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_name_has_no_namespace_prefix_and_carries_the_nonce() {
        let name = semaphore_name([0, 1, 2, 0xab, 0xcd, 0xef, 0x10, 0xff]);
        assert_eq!(name, "kgraf-jobserver-000102abcdef10ff");
        assert!(!name.contains('\\'), "no Global\\ or Local\\ prefix");
        assert_ne!(name, semaphore_name([0; 8]));
    }

    /// The value names the semaphore the way the `jobserver` crate finds it:
    /// it looks for `--jobserver-fds=` or `--jobserver-auth=` and takes the
    /// rest of that word as the name.
    #[test]
    fn makeflags_names_the_semaphore_under_both_flags() {
        let v = makeflags("kgraf-jobserver-00ff");
        assert_eq!(
            v,
            "-j --jobserver-fds=kgraf-jobserver-00ff --jobserver-auth=kgraf-jobserver-00ff"
        );
        for flag in ["--jobserver-auth=", "--jobserver-fds="] {
            let name = v
                .split_whitespace()
                .find_map(|w| w.strip_prefix(flag))
                .expect("the flag");
            assert_eq!(name, "kgraf-jobserver-00ff");
        }
        assert_eq!(JOBSERVER_VAR, "CARGO_MAKEFLAGS");
    }

    /// The sandbox user may wait on the semaphore and release it, and
    /// nothing more: no write of its access list, no delete, no other user.
    #[test]
    fn the_access_list_admits_the_user_to_wait_and_release_only() {
        let sddl = semaphore_sddl("S-1-5-21-1-2-3-1001", "S-1-5-21-1-2-3-1010");
        assert_eq!(
            sddl,
            "D:P(A;;GA;;;SY)(A;;GA;;;S-1-5-21-1-2-3-1001)(A;;0x00100002;;;S-1-5-21-1-2-3-1010)"
        );
        // SYNCHRONIZE and SEMAPHORE_MODIFY_STATE, not WRITE_DAC (0x40000),
        // WRITE_OWNER (0x80000) or DELETE (0x10000).
        assert_eq!(USER_SEMAPHORE_ACCESS & (0x4_0000 | 0x8_0000 | 0x1_0000), 0);
        assert_eq!(sddl.matches("(A;;").count(), 3);
        assert!(sddl.starts_with("D:P"), "protected: nothing inherited");
    }

    #[test]
    fn tokens_are_the_parallelism_and_never_zero() {
        assert_eq!(tokens(0), 1);
        assert_eq!(tokens(1), 1);
        assert_eq!(tokens(16), 16);
        assert_eq!(tokens(usize::MAX), i32::MAX);
    }
}

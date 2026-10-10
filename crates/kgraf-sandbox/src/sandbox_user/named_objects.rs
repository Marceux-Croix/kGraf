//! Named kernel objects and the Windows sandbox (decision 0097): why cargo
//! and rustc stop at start in a sandboxed command, and the note kGraf adds
//! to such a command's output so the failure says why.
//!
//! **What fails.** A program makes or opens a named mutex, semaphore, event
//! or section through the Win32 calls (`CreateMutexA`, `OpenSemaphoreA`,
//! ...). Those calls resolve every name, with or without a `Global\` or
//! `Local\` prefix, relative to a handle that `kernelbase.dll` opens once per
//! process on the session's object directory
//! (`\Sessions\<n>\BaseNamedObjects`), with explicit access rights; `Global`
//! and `Local` are symbolic links inside that directory. Microsoft does not
//! document this; it is how ReactOS and Wine implement the calls after
//! Windows, and it fits every measurement so far. An explicit open of
//! the directory is an access check on the directory itself, which the
//! traverse privilege (`SeChangeNotifyPrivilege`) does not bypass. Windows
//! admits the session's logon SID to that directory. The command of a
//! sandbox user runs with that SID deny-only, because objects the owner's
//! processes make with their default access list grant it (see
//! `windows::runner`). So the command can neither make nor open a named
//! object through those calls: a hosted Windows self-test measured "a named
//! semaphore in the session ... exit 5, denied Access is denied", and the
//! open of a semaphore kGraf made for the command and whose own access list
//! admits it was refused the same way.
//!
//! **Who needs one.** cargo and rustc share build parallelism through a
//! jobserver, which on Windows is a named semaphore (the `jobserver` crate:
//! `OpenSemaphoreA` on the name in `CARGO_MAKEFLAGS`, `CreateSemaphoreA`
//! when there is none). cargo makes one for any job count, `-j1` included,
//! so a smaller job count does not avoid it. rustc also makes the named mutex
//! `rustc_errors` (no prefix) to print any warning or error
//! (`compiler/rustc_errors/src/lock.rs`, taken in `emit_to_destination`), so
//! a jobserver in any namespace cannot save a build that prints a
//! diagnostic: rustc then aborts with status 0xC0000409.
//!
//! **What kGraf does.** It cannot make the session's directory admit the
//! command without giving the command the logon SID, which would admit it to
//! the owner's processes, or without changing the access list of a system
//! object of every session. So it says so: [`note_for`] reads a failed
//! command's output, and where the output shows one of these failures, the
//! caller adds [`NOTE`] to it. The self-test measures each part of the cause
//! (`the_sandbox_user_opens_its_jobserver_and_named_objects_are_measured` in
//! `tests/escape_windows.rs`), so a later fix can be judged on evidence.
//!
//! Platform-neutral: the markers are matched on every host. They are texts
//! Windows builds of rustc, cargo and the `jobserver` crate print.

/// Which failure a command's output shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cause {
    /// rustc could not make its `rustc_errors` mutex to print a diagnostic.
    DiagnosticLock,
    /// cargo or rustc could not make a jobserver semaphore.
    JobserverCreate,
    /// cargo or rustc could not open the jobserver `CARGO_MAKEFLAGS` names.
    JobserverOpen,
}

impl Cause {
    /// What the output showed, in words.
    pub fn seen(self) -> &'static str {
        match self {
            Cause::DiagnosticLock => "rustc could not make its named mutex to print a diagnostic",
            Cause::JobserverCreate => "cargo or rustc could not make its jobserver semaphore",
            Cause::JobserverOpen => {
                "cargo or rustc could not open the jobserver semaphore CARGO_MAKEFLAGS names"
            }
        }
    }
}

/// rustc's panic when `CreateMutexA` fails (`rustc_errors/src/lock.rs`).
const DIAGNOSTIC_LOCK_MARKER: &str = "failed to create global mutex named";
/// rustc's panic and cargo's error when `jobserver::Client::new` fails.
const JOBSERVER_CREATE_MARKER: &str = "failed to create jobserver";
/// The warning cargo and rustc print when the jobserver in the environment
/// cannot be opened.
const JOBSERVER_OPEN_MARKER: &str = "failed to connect to jobserver from environment variable";
/// How Windows words ERROR_ACCESS_DENIED in an `io::Error`, in its
/// `Display` (`Access is denied. (os error 5)`) and in its `Debug`
/// (`Os { code: 5, kind: PermissionDenied, ... }`).
const ACCESS_DENIED_MARKERS: [&str; 2] = ["Access is denied", "kind: PermissionDenied"];

/// The failure `output` shows, if any. A jobserver failure counts only with
/// Windows' access-denied text beside it: on other hosts and for other
/// errors the same words mean something else. The diagnostic lock is made
/// only by Windows builds of rustc, so its marker stands alone. Where more
/// than one shows, the diagnostic lock wins: it is the one no jobserver
/// avoids.
pub fn cause(output: &str) -> Option<Cause> {
    if output.contains(DIAGNOSTIC_LOCK_MARKER) {
        return Some(Cause::DiagnosticLock);
    }
    let denied = ACCESS_DENIED_MARKERS.iter().any(|m| output.contains(m));
    if !denied {
        return None;
    }
    if output.contains(JOBSERVER_CREATE_MARKER) {
        Some(Cause::JobserverCreate)
    } else if output.contains(JOBSERVER_OPEN_MARKER) {
        Some(Cause::JobserverOpen)
    } else {
        None
    }
}

/// The note added to a failed command's output whose failure is one of
/// [`Cause`]. It names the cause and says that the program itself did not
/// fail. No prefix: each caller adds its own (`[sandbox] `).
pub const NOTE: &str = "the program stopped because it could not make or open a named kernel \
     object (a mutex or a semaphore) in the Windows session. cargo and rustc need one: cargo \
     for its jobserver, rustc for its jobserver and for the lock it takes to print any warning \
     or error. A sandboxed command cannot open the session's object directory: Windows admits \
     the session's logon SID there, the Windows sandbox user's command holds that SID \
     deny-only so that it cannot open the owner's own processes and objects, and the token \
     sandbox's restricted token is refused there too (decision 0097). The program did not \
     fail on its own; it cannot run in the Windows sandbox on this machine. A smaller job \
     count (-j1) does not help: cargo makes a jobserver for any job count.";

/// The note for a failed command's `output`, with what it showed, or `None`
/// when the output shows no named-object failure.
pub fn note_for(output: &str) -> Option<String> {
    cause(output).map(|c| format!("{}: {NOTE}", c.seen()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// rustc's abort in a hosted Windows self-test, as cargo relayed it.
    const RUSTC_LOCK: &str = "error: failed to run `rustc` to learn about target-specific \
         information\n\nCaused by:\n  process didn't exit successfully: `rustc.exe - \
         --crate-name ___ --print=file-names` (exit code: 0xc0000409, \
         STATUS_STACK_BUFFER_OVERRUN)\n  --- stderr\n\n  thread 'main' (8272) panicked at \
         compiler\\rustc_errors\\src\\lock.rs:54:29:\n  failed to create global mutex named \
         `rustc_errors`\n";

    /// cargo's warning for a jobserver it could not open, in the same run.
    const CARGO_OPEN: &str = "warning: failed to connect to jobserver from environment \
         variable `CARGO_MAKEFLAGS=\"-j --jobserver-fds=kgraf-jobserver-c6be7c4d14239042 \
         --jobserver-auth=kgraf-jobserver-c6be7c4d14239042\"`: cannot open path or name \
         kgraf-jobserver-c6be7c4d14239042 from the jobserver environment variable value: \
         Access is denied. (os error 5)\n";

    /// rustc's panic for a jobserver it could not make, in an earlier run.
    const RUSTC_CREATE: &str = "thread 'main' panicked at \
         compiler\\rustc_data_structures\\src\\jobserver.rs:43:41:\nfailed to create \
         jobserver: Os { code: 5, kind: PermissionDenied, message: \"Access is denied.\" }\n";

    #[test]
    fn each_windows_failure_text_names_its_cause() {
        assert_eq!(cause(RUSTC_LOCK), Some(Cause::DiagnosticLock));
        assert_eq!(cause(CARGO_OPEN), Some(Cause::JobserverOpen));
        assert_eq!(cause(RUSTC_CREATE), Some(Cause::JobserverCreate));
    }

    /// The run's whole output carried the open warning and the lock abort;
    /// the lock is named, because no jobserver avoids it.
    #[test]
    fn the_diagnostic_lock_wins_over_a_jobserver_failure() {
        let both = format!("{CARGO_OPEN}{RUSTC_LOCK}");
        assert_eq!(cause(&both), Some(Cause::DiagnosticLock));
    }

    #[test]
    fn other_failures_get_no_note() {
        for text in [
            "",
            "error[E0308]: mismatched types",
            // An access denial with no jobserver or lock in sight.
            "error: could not write to C:\\x: Access is denied. (os error 5)",
            // A jobserver failure for another reason, or on another host.
            "failed to create jobserver: No such file or directory (os error 2)",
            "warning: failed to connect to jobserver from environment variable \
             `MAKEFLAGS=\"-j --jobserver-auth=3,4\"`: cannot open file descriptor 3",
        ] {
            assert_eq!(cause(text), None, "{text}");
            assert_eq!(note_for(text), None, "{text}");
        }
    }

    #[test]
    fn the_note_says_what_was_seen_and_why_it_cannot_run() {
        let note = note_for(RUSTC_LOCK).expect("a note");
        assert!(note.starts_with(Cause::DiagnosticLock.seen()), "{note}");
        for part in ["logon SID", "deny-only", "did not fail on its own", "-j1"] {
            assert!(note.contains(part), "{part}: {note}");
        }
    }
}

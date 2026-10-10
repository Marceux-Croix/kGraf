//! The probe that proves whether a sandboxed command can make the named
//! kernel objects cargo and rustc need (decision 0097).
//!
//! **Why a probe at all.** Whether a named object works is a property of the
//! machine, not of what setup meant to do. The launcher service can be
//! installed and still leave a command unable to make a name, and the
//! fallback grant can be written and still not be enough. So kGraf measures
//! it: one tiny program runs as a sandbox user, through the launch path
//! being tried, and makes the two objects a build needs. Its exit status and
//! output become a [`super::session0::Proof`], and only a proof makes
//! [`super::session0::Active::cargo_runs`] true.
//!
//! **What it makes.** A named semaphore, which is what cargo's jobserver is
//! on Windows, and a named mutex with no namespace prefix, which is what
//! rustc takes to print any warning or error. Both names are unprefixed,
//! because that is the form cargo and rustc use, and an unprefixed name is
//! the one that resolves below the session's own object directory and is
//! refused there ([`super::named_objects`]). The mutex is the case that
//! decides a build: a jobserver in any namespace cannot save a compile that
//! prints a diagnostic.
//!
//! The names carry a random nonce and never rustc's own `rustc_errors`: a
//! probe must not collide with a real rustc that runs at the same time, and
//! must not leave a name a later rustc then opens instead of making.
//!
//! **How it is reached.** As a hidden subcommand of the kGraf executable
//! ([`SUBCOMMAND`]), the way the launcher service is
//! ([`super::session0::SERVICE_SUBCOMMAND`]). Two forms:
//!
//! - without [`LAUNCH_FLAG`], the sandbox user's own side: it makes the
//!   objects, prints one line per object and exits 0, or prints the Win32
//!   error and the object it could not make and exits [`FAILED_EXIT`];
//! - with [`LAUNCH_FLAG`], the owner's side: it starts the first form as a
//!   sandbox user through the recorded launch path, and relays what that
//!   printed and the status it exited with. It is its own process because
//!   the launch path is read once per process: setup writes the record and
//!   then must measure the path it just wrote.
//!
//! On a platform that is not Windows the probe prints that there is nothing
//! to prove and exits 0: named kernel objects of this kind are a Windows
//! idea, and no other backend meets the problem.
//!
//! Everything here but [`Object::make`]'s Windows call is platform-neutral,
//! so the subcommand's name, the names of the objects and every line the
//! probe prints are tested on every host.

/// The probe's subcommand of the kGraf executable. Hidden: it is a
/// measurement kGraf takes of itself, not a feature of a run.
pub const SUBCOMMAND: &str = "sandbox-named-object-probe";

/// The flag that asks for the owner's side: start the probe as a sandbox
/// user through the recorded launch path and relay its result.
pub const LAUNCH_FLAG: &str = "--launch";

/// The status the probe exits with when it made both objects.
pub const MADE_EXIT: i32 = 0;

/// The status the probe exits with when an object was refused or any other
/// failure stopped it.
pub const FAILED_EXIT: i32 = 1;

/// The status the owner's side exits with when the probe could not be
/// started or waited for at all.
///
/// It is a status of its own, and not simply a non-zero one, because the
/// reason such a launch failed can itself carry Windows' access-denied text
/// (a logon right, a file the sandbox user cannot read). Read as output,
/// that text would say a named object was refused, and a refused named
/// object is what moves the machine to the weaker fallback. A launch that
/// never ran says nothing about named objects, so it gets its own channel.
pub const LAUNCH_FAILED_EXIT: i32 = 70;

/// How long the owner's side waits for the probe. The probe makes two
/// objects and exits, so a wait this long means it is not coming; a
/// measurement must never hold setup for the rest of the day.
pub const WAIT: std::time::Duration = std::time::Duration::from_secs(120);

/// What a run of the probe through a launch path proves, from the exit
/// status and the output of the owner's side ([`LAUNCH_FLAG`]).
///
/// Every reader of a relayed probe uses this, so setup, `selfcheck` and the
/// Windows self-test cannot disagree about what a run proved. The one rule
/// it adds over [`super::session0::Proof::of`] is [`LAUNCH_FAILED_EXIT`]: a
/// launch that never ran says nothing about named objects, whatever its text
/// says, and must never move the machine to the weaker fallback.
pub fn proof_of_relay(code: Option<i32>, output: &str) -> super::session0::Proof {
    if code == Some(LAUNCH_FAILED_EXIT) {
        return super::session0::Proof::CouldNotRun(output.trim().to_owned());
    }
    super::session0::Proof::of(code, output)
}

/// What the probe makes, in the order it makes them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Object {
    /// cargo's jobserver is a named semaphore on Windows.
    Semaphore,
    /// rustc takes a named mutex with no namespace prefix to print any
    /// warning or error. This is the case that decides whether a build runs.
    Mutex,
}

impl Object {
    /// Both objects, in the order the probe makes them.
    pub const ALL: [Object; 2] = [Object::Semaphore, Object::Mutex];

    /// A single word for a name and a line.
    pub fn key(self) -> &'static str {
        match self {
            Object::Semaphore => "semaphore",
            Object::Mutex => "mutex",
        }
    }

    /// What the object is, in words, for the line the probe prints.
    pub fn words(self) -> &'static str {
        match self {
            Object::Semaphore => "named semaphore",
            Object::Mutex => "unprefixed named mutex",
        }
    }

    /// The object's name for `nonce`: `kgraf-probe-<kind>-` and sixteen hex
    /// digits, with no `Global\` or `Local\` prefix.
    ///
    /// Unprefixed, because that is the form cargo and rustc use and the one
    /// that resolves below the object directory of the session the command
    /// runs in. A prefixed name would measure another directory and prove
    /// nothing about a build.
    pub fn name(self, nonce: [u8; 8]) -> String {
        let hex: String = nonce.iter().map(|b| format!("{b:02x}")).collect();
        format!("kgraf-probe-{}-{hex}", self.key())
    }
}

/// A fresh nonce for one probe's names, from the system random source.
pub fn nonce() -> Result<[u8; 8], getrandom::Error> {
    let mut out = [0u8; 8];
    getrandom::fill(&mut out)?;
    Ok(out)
}

/// The line for an object the probe made.
pub fn made_line(object: Object, name: &str) -> String {
    format!("made the {} {name}", object.words())
}

/// The line for an object the probe could not make. `why` is the Win32
/// error, in the words Windows gives it and with its code, so the text a
/// caller classifies carries Windows' own access-denied wording
/// ([`super::named_objects::denied`]).
pub fn failed_line(object: Object, name: &str, why: &str) -> String {
    format!("could not make the {} {name}: {why}", object.words())
}

/// What the probe prints where named kernel objects of this kind do not
/// exist, which is every platform but Windows.
pub fn nothing_to_prove_line() -> String {
    format!(
        "nothing to prove on {}: a named kernel object of this kind is a Windows idea, and no \
         other sandbox backend needs one",
        std::env::consts::OS
    )
}

/// What one run of the probe did: the lines it prints, and the status it
/// exits with.
///
/// The lines are returned instead of printed so a host test reads them. The
/// caller prints them and exits, which keeps every decision about the text
/// in this file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    /// One line per object made, in order.
    pub made: Vec<String>,
    /// The object that was refused, with the Win32 error. `None` when every
    /// object was made.
    pub failed: Option<String>,
    pub code: i32,
}

impl Outcome {
    /// Every object was made and nothing was refused.
    pub fn passed(&self) -> bool {
        self.code == MADE_EXIT && self.failed.is_none()
    }
}

/// Make both objects and say what happened. On a platform that is not
/// Windows it proves nothing and says so.
///
/// The objects live until this call returns: a name is free again once its
/// object is closed, and the second object must be made while the first is
/// still held, as cargo's and rustc's are.
pub fn run() -> Outcome {
    #[cfg(not(windows))]
    {
        Outcome {
            made: vec![nothing_to_prove_line()],
            failed: None,
            code: MADE_EXIT,
        }
    }
    #[cfg(windows)]
    {
        let nonce = match nonce() {
            Ok(n) => n,
            Err(e) => {
                return Outcome {
                    made: Vec::new(),
                    // Not a refused object: the names were never made, so
                    // this must not read as Windows denying one.
                    failed: Some(format!("the probe could not make a random name: {e}")),
                    code: FAILED_EXIT,
                };
            }
        };
        let mut made = Vec::new();
        // Held until the end of the call, so the second object is made while
        // the first still holds its name.
        let mut held = Vec::new();
        for object in Object::ALL {
            let name = object.name(nonce);
            match super::windows::probe::make(object, &name) {
                Ok(handle) => {
                    held.push(handle);
                    made.push(made_line(object, &name));
                }
                Err(why) => {
                    return Outcome {
                        made,
                        failed: Some(failed_line(object, &name, &why)),
                        code: FAILED_EXIT,
                    };
                }
            }
        }
        Outcome {
            made,
            failed: None,
            code: MADE_EXIT,
        }
    }
}

/// Names the sandbox users of a tagged setup, as it does for any other
/// sandboxed command (`windows::state`). The owner's side passes it to the
/// probe it starts, so a tagged setup proves its own users and never the
/// owner's.
///
/// Repeated here because the probe's launcher is outside this crate and that
/// constant is internal. The Windows test below holds the two equal.
pub const TAG_ENV: &str = "KGRAF_SANDBOX_TAG";

#[cfg(test)]
mod tests {
    use super::*;

    /// The subcommand is one word, so it can be a single argument of the
    /// executable, and it is not the launcher service's.
    #[test]
    fn the_subcommand_is_one_word_of_its_own() {
        assert_eq!(SUBCOMMAND, "sandbox-named-object-probe");
        assert!(!SUBCOMMAND.contains(' '), "{SUBCOMMAND}");
        assert_ne!(SUBCOMMAND, super::super::session0::SERVICE_SUBCOMMAND);
        assert_eq!(LAUNCH_FLAG, "--launch");
    }

    /// A launch that never ran is told apart from a probe that ran and
    /// failed, by a status of its own.
    #[test]
    fn each_exit_status_is_its_own() {
        assert_eq!(MADE_EXIT, 0);
        assert_ne!(FAILED_EXIT, MADE_EXIT);
        assert_ne!(LAUNCH_FAILED_EXIT, FAILED_EXIT);
        assert_ne!(LAUNCH_FAILED_EXIT, MADE_EXIT);
    }

    /// A launch that never ran is never read as a refused named object, even
    /// when the reason it failed carries Windows' access-denied text. Only a
    /// refusal moves the machine to the weaker fallback, so this rule is the
    /// one every reader of a relayed probe must share.
    #[test]
    fn a_launch_that_never_ran_is_not_a_refused_object() {
        use super::super::session0::Proof;
        let denied = "start the probe as a sandbox user: Access is denied. (os error 5)";
        assert_eq!(
            proof_of_relay(Some(LAUNCH_FAILED_EXIT), denied),
            Proof::CouldNotRun(denied.to_owned())
        );
        assert_eq!(
            proof_of_relay(Some(FAILED_EXIT), denied),
            Proof::Refused(denied.to_owned())
        );
        assert_eq!(proof_of_relay(Some(MADE_EXIT), "made both"), Proof::Made);
    }

    /// The names carry the nonce, carry no namespace prefix, and are not
    /// rustc's own, which a real rustc would then open instead of making.
    #[test]
    fn the_names_are_random_unprefixed_and_not_rustcs_own() {
        let nonce = [0, 1, 2, 0xab, 0xcd, 0xef, 0x10, 0xff];
        assert_eq!(
            Object::Semaphore.name(nonce),
            "kgraf-probe-semaphore-000102abcdef10ff"
        );
        assert_eq!(
            Object::Mutex.name(nonce),
            "kgraf-probe-mutex-000102abcdef10ff"
        );
        for object in Object::ALL {
            let name = object.name(nonce);
            assert!(!name.contains('\\'), "{name}: no Global\\ or Local\\");
            assert!(!name.contains("rustc_errors"), "{name}");
            assert_ne!(name, object.name([0; 8]));
        }
        // Two objects, two names: one name for both would make the second
        // call open the first object and prove nothing.
        assert_ne!(Object::Semaphore.name(nonce), Object::Mutex.name(nonce));
    }

    /// Two runs ask the system random source for different names.
    #[test]
    fn a_fresh_nonce_is_random() {
        let first = nonce().expect("a nonce");
        assert_ne!(first, nonce().expect("a nonce"));
        assert_ne!(first, [0u8; 8]);
    }

    /// Each line names the object it is about, and the mutex line says the
    /// name carried no prefix: that is the case a build depends on.
    #[test]
    fn each_line_names_its_object_and_its_name() {
        let made = made_line(Object::Semaphore, "kgraf-probe-semaphore-00ff");
        assert_eq!(
            made, "made the named semaphore kgraf-probe-semaphore-00ff",
            "{made}"
        );
        let mutex = made_line(Object::Mutex, "kgraf-probe-mutex-00ff");
        assert!(mutex.contains("unprefixed named mutex"), "{mutex}");

        let failed = failed_line(
            Object::Mutex,
            "kgraf-probe-mutex-00ff",
            "Access is denied. (os error 5) (Windows error 5)",
        );
        assert_eq!(
            failed,
            "could not make the unprefixed named mutex kgraf-probe-mutex-00ff: Access is \
             denied. (os error 5) (Windows error 5)"
        );
        // The classification of a result reads Windows' own words out of
        // this line, so the line must carry them unchanged.
        assert!(super::super::named_objects::denied(&failed), "{failed}");
    }

    /// On a host that is not Windows the probe proves nothing and says so,
    /// and still exits 0: there is no named-object problem to measure.
    #[cfg(not(windows))]
    #[test]
    fn a_host_that_is_not_windows_proves_nothing_and_passes() {
        let outcome = run();
        assert!(outcome.passed(), "{outcome:?}");
        assert_eq!(outcome.code, MADE_EXIT);
        assert_eq!(outcome.failed, None);
        assert_eq!(outcome.made.len(), 1);
        assert!(outcome.made[0].contains("nothing to prove"), "{outcome:?}");
        assert!(outcome.made[0].contains(std::env::consts::OS));
        // Nothing to prove is not a refusal: a caller that read these lines
        // as one would move the machine to a weaker boundary for no reason.
        assert!(!super::super::named_objects::denied(&outcome.made[0]));
    }

    /// On Windows the probe makes both objects as the owner's own process,
    /// which the session's object directory admits. A failure here is a
    /// defect of the probe, not a measurement of the sandbox.
    #[cfg(windows)]
    #[test]
    fn the_owners_own_process_makes_both_objects() {
        let outcome = run();
        assert!(outcome.passed(), "{outcome:?}");
        assert_eq!(outcome.made.len(), Object::ALL.len(), "{outcome:?}");
    }

    /// The tag's variable is the one every other sandboxed command reads.
    #[cfg(windows)]
    #[test]
    fn the_tag_variable_is_the_one_a_command_reads() {
        assert_eq!(TAG_ENV, super::super::windows::state::TAG_ENV);
    }
}

//! Where a sandboxed command's named kernel objects live, and the launcher
//! that puts a command where it can make them (decision 0097).
//!
//! **The problem.** A command that runs as a sandbox user in the owner's own
//! Windows session can neither make nor open a named mutex, semaphore, event
//! or section. Every Win32 call for those names resolves it below the
//! session's object directory (`\Sessions\<n>\BaseNamedObjects`), which
//! `kernelbase.dll` opens with explicit rights before it looks up any name.
//! Windows admits the session's logon SID there, and the command holds that
//! SID deny-only, because objects the owner's own processes make admit it.
//! cargo needs a named semaphore for its jobserver and rustc needs the
//! unprefixed named mutex `rustc_errors` to print any warning or error, so
//! neither runs. [`super::named_objects`] holds the measurement.
//!
//! **The launcher.** A process in session 0, the session Windows gives its
//! services, resolves an unprefixed name below the machine-wide
//! `\BaseNamedObjects` instead, which carries no session logon SID. So kGraf
//! installs a service at setup, in session 0, that does one thing: it starts
//! kGraf's own runner as a slot user. The command then runs in session 0, the
//! runner builds its token exactly as before, and the sandbox user's rights
//! are unchanged: the launcher widens nothing and runs nothing but the runner.
//! [`LaunchPath::Session0`].
//!
//! **The fallback.** Where the service cannot be installed or does not prove
//! itself, setup instead grants the sandbox group the smallest rights that let
//! a command look up and make a name in the owner's session directory
//! ([`LaunchPath::OwnerSession`]). That is a weaker boundary: a command can
//! then put a named object in the directory the owner's own programs read.
//! Setup, status, selfcheck and the Windows self-test say which path is
//! active, why, and what the fallback costs. Where neither works, commands
//! run as before and a failed cargo or rustc keeps the note of
//! [`super::named_objects`].
//!
//! Everything in this file is platform-neutral, so the protocol, the names,
//! the selection and the messages are tested on every host. The Windows calls
//! live in `windows::service` (install and remove), `windows::service_main`
//! (the service itself) and `windows::launch` (the owner's side).

use std::fmt;
use std::io::Read;

use zeroize::{Zeroize, Zeroizing};

/// Which launcher starts a sandboxed command, and so which object namespace
/// the command's named objects live in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LaunchPath {
    /// kGraf's service starts the runner in session 0. An unprefixed name
    /// the command makes lands in the machine-wide `\BaseNamedObjects`, so
    /// cargo and rustc run.
    Session0,
    /// The owner's own session, through `CreateProcessWithLogonW`. Named
    /// objects work only where the session's object directory admits the
    /// command, which needs the grant of [`LaunchPath::OwnerSession`]'s
    /// setup step.
    OwnerSession,
}

impl LaunchPath {
    /// A single word for a record, a message or a pipe field.
    pub fn key(self) -> &'static str {
        match self {
            LaunchPath::Session0 => "session-0",
            LaunchPath::OwnerSession => "owner-session",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "session-0" => Some(LaunchPath::Session0),
            "owner-session" => Some(LaunchPath::OwnerSession),
            _ => None,
        }
    }

    /// Whether a name the command makes with no prefix is expected to work.
    pub fn named_objects_work(self, granted: bool) -> bool {
        match self {
            LaunchPath::Session0 => true,
            LaunchPath::OwnerSession => granted,
        }
    }
}

/// Whether the named objects a command needs are expected to work, and why,
/// as setup, status, selfcheck and the self-test report say it. Built from
/// what is on the machine, never from what setup meant to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Active {
    pub path: LaunchPath,
    /// For [`LaunchPath::OwnerSession`]: whether the session directory grant
    /// is on the machine.
    pub granted: bool,
    /// What the probe found, if one ran.
    pub proved: Option<Proof>,
    /// Why this path is the active one, in words. Empty for the service path
    /// that was installed and proved.
    pub because: String,
}

/// What a probe found. A sandbox user runs one command through the path being
/// tried, and that command makes a named semaphore and an unprefixed mutex.
///
/// The three outcomes are not the same thing, and kGraf must not treat them
/// so: a refused named object is what the fallback exists for, while a probe
/// that could not run at all says nothing about named objects and must not
/// move the machine to a weaker boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Proof {
    /// The command made both objects.
    Made,
    /// The command ran and Windows refused a named object.
    Refused(String),
    /// The probe could not run, for a reason that is not a refused named
    /// object (the program could not start, or the launcher could not be
    /// asked).
    CouldNotRun(String),
}

impl Proof {
    /// What a probe's exit status and output mean. A refusal is read from the
    /// markers of [`super::named_objects`], which are the texts Windows and
    /// the probe print for an access denial; anything else that failed could
    /// not run.
    pub fn of(code: Option<i32>, output: &str) -> Self {
        if code == Some(0) {
            return Proof::Made;
        }
        let text = one_line(output);
        if super::named_objects::denied(output) {
            Proof::Refused(text)
        } else {
            Proof::CouldNotRun(text)
        }
    }
}

impl Active {
    /// The service is installed and the probe passed: nothing to warn about.
    pub fn session0_proved() -> Self {
        Self {
            path: LaunchPath::Session0,
            granted: false,
            proved: Some(Proof::Made),
            because: String::new(),
        }
    }

    /// Whether cargo and rustc are expected to run as a sandbox user: only
    /// where a probe made the objects on this machine. A path that promises
    /// them and was not proved counts as not working, so a failed cargo still
    /// gets the note that says why (fail loud).
    pub fn cargo_runs(&self) -> bool {
        self.proved == Some(Proof::Made)
    }

    /// One line naming the active path, for `kgraf sandbox status`,
    /// `selfcheck`, the end of setup and the self-test report. It says the
    /// path, whether a probe proved it, and what the fallback costs. Always
    /// said: a sandbox that quietly gives less than it promised is a defect
    /// (fail loud).
    pub fn line(&self) -> String {
        let mut out = String::from("named objects: ");
        match self.path {
            LaunchPath::Session0 => out.push_str(
                "sandboxed commands start in session 0 through kGraf's launcher service, \
                 so cargo and rustc can make the named objects they need. What this path \
                 costs: a command can make a name in the machine-wide object directory, \
                 which the services of this machine also use, so it can take a name a \
                 service has not made yet. An object a command makes admits that command's \
                 user alone, and two commands of one sandbox user can reach each other's \
                 objects there",
            ),
            LaunchPath::OwnerSession if self.granted => out.push_str(
                "sandboxed commands start in the owner's session, which admits the sandbox \
                 group to its object directory, so cargo and rustc can make the named objects \
                 they need. Risk of this fallback: a sandboxed command can put a named object \
                 in the directory the owner's own programs read",
            ),
            LaunchPath::OwnerSession => out.push_str(
                "sandboxed commands start in the owner's session and cannot make or open a \
                 named object there, so cargo and rustc cannot run as a sandbox user",
            ),
        }
        if !self.because.is_empty() {
            out.push_str(". Why this path: ");
            out.push_str(&self.because);
        }
        match &self.proved {
            Some(Proof::Made) => out.push_str(
                ". Proved: a sandbox user made a named semaphore and an unprefixed mutex \
                 through this path",
            ),
            Some(Proof::Refused(why)) => {
                out.push_str(". The proof failed, a named object was refused: ");
                out.push_str(why);
            }
            Some(Proof::CouldNotRun(why)) => {
                out.push_str(
                    ". Not proved on this machine: the probe could not run, which says \
                     nothing about named objects. ",
                );
                out.push_str(why);
            }
            None => out.push_str(". Not proved on this machine: no probe has run"),
        }
        out
    }
}

impl Active {
    /// The record setup writes beside the credential store, so every later
    /// command and every `status` reads the path that was proved and not the
    /// one a version of kGraf would pick again. One `key: value` line per
    /// field, so a damaged file loses a field and not the whole record.
    pub fn encode(&self) -> String {
        let mut out = format!("path: {}\ngranted: {}\n", self.path.key(), self.granted);
        match &self.proved {
            Some(Proof::Made) => out.push_str("proved: yes\n"),
            Some(Proof::Refused(why)) => {
                out.push_str(&format!("proved: refused {}\n", one_line(why)));
            }
            Some(Proof::CouldNotRun(why)) => {
                out.push_str(&format!("proved: could-not-run {}\n", one_line(why)));
            }
            None => out.push_str("proved: none\n"),
        }
        if !self.because.is_empty() {
            out.push_str(&format!("because: {}\n", one_line(&self.because)));
        }
        out
    }

    /// The record in `text`. A line that is not understood is skipped, and a
    /// missing or damaged record reads as the owner's session with no grant
    /// and no proof: the launch path every setup before the launcher service
    /// made, which is the one that promises least.
    pub fn decode(text: &str) -> Self {
        let mut out = Self {
            path: LaunchPath::OwnerSession,
            granted: false,
            proved: None,
            because: String::new(),
        };
        for line in text.lines() {
            let Some((key, value)) = line.split_once(": ") else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "path" => {
                    if let Some(p) = LaunchPath::from_key(value) {
                        out.path = p;
                    }
                }
                "granted" => out.granted = value == "true",
                "proved" => {
                    out.proved = match value.split_once(' ') {
                        Some(("refused", why)) => Some(Proof::Refused(why.to_owned())),
                        Some(("could-not-run", why)) => Some(Proof::CouldNotRun(why.to_owned())),
                        _ if value == "yes" => Some(Proof::Made),
                        // A line this version does not read is no proof: the
                        // path then promises nothing, which is the safe way
                        // to read a record a later kGraf wrote.
                        _ => None,
                    };
                }
                "because" => out.because = value.to_owned(),
                _ => {}
            }
        }
        out
    }
}

/// What setup found while it tried the two paths, in the order it tried
/// them. [`select`] turns it into the active path, so the order and the
/// messages are decided in one place and tested on every host.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Attempt {
    /// Why the service could not be installed or started, if that failed.
    pub service_failed: Option<String>,
    /// What the probe through the service path found, if it ran.
    pub service_probe: Option<Proof>,
    /// Why the session directory grant failed, if it was tried and failed.
    pub grant_failed: Option<String>,
    /// Whether the grant is on the machine.
    pub granted: bool,
    /// What the probe through the owner's session found, if it ran.
    pub grant_probe: Option<Proof>,
}

/// The active path from what setup found. The service path wins unless it
/// could not be set up or a probe showed a named object refused through it;
/// the grant is next; where neither holds, commands keep the owner's session
/// with no grant and cargo and rustc stay unable to run.
///
/// A probe that could not run leaves the service path active and unproved: it
/// says nothing about named objects, so it must not move the machine to the
/// weaker boundary of the grant.
pub fn select(a: &Attempt) -> Active {
    let service_ok =
        a.service_failed.is_none() && !matches!(a.service_probe, Some(Proof::Refused(_)));
    if service_ok {
        return Active {
            path: LaunchPath::Session0,
            granted: false,
            proved: a.service_probe.clone(),
            because: String::new(),
        };
    }
    let because = match (&a.service_failed, &a.service_probe) {
        (Some(why), _) => format!("kGraf's launcher service could not be set up: {why}"),
        (None, Some(Proof::Refused(why))) => {
            format!("kGraf's launcher service is installed, but its proof failed: {why}")
        }
        (None, _) => String::new(),
    };
    if a.granted {
        return Active {
            path: LaunchPath::OwnerSession,
            granted: true,
            proved: a.grant_probe.clone(),
            because,
        };
    }
    let because = match &a.grant_failed {
        Some(why) => format!("{because}. The fallback grant also failed: {why}"),
        None => because,
    };
    Active {
        path: LaunchPath::OwnerSession,
        granted: false,
        proved: a.grant_probe.clone(),
        because,
    }
}

// ---- the runner a launch names ----------------------------------------------

/// The launcher service a session-0 launch asks, as this Windows user's names
/// give it (`super::names::OwnerNames`).
#[derive(Clone, Copy, Debug)]
pub struct ExpectedService<'a> {
    pub name: &'a str,
    pub owner_sid: &'a str,
    pub pipe: &'a str,
}

/// The runner the owner's side names when it starts a command on `path`.
///
/// - [`LaunchPath::Session0`]: the runner kGraf's launcher service was
///   installed with, read from the service's image path (`installed_image`,
///   `None` where no service has the name). The service starts that runner
///   and refuses any other, so a copy beside the calling program is never
///   named, wherever that program lives: a test binary, or a second copy of
///   kGraf. A service that is missing, unreadable, or set up for another
///   user or pipe is an error that says to run setup again (fail loud).
/// - [`LaunchPath::OwnerSession`]: the runner beside the calling program
///   (`beside`), which this process starts itself.
///
/// Each source is read only on its own path, so a session-0 launch does not
/// depend on a runner beside the program at all.
pub fn runner_for(
    path: LaunchPath,
    service: ExpectedService<'_>,
    installed_image: impl FnOnce() -> Result<Option<String>, String>,
    beside: impl FnOnce() -> Result<std::path::PathBuf, String>,
) -> Result<std::path::PathBuf, String> {
    if path == LaunchPath::OwnerSession {
        return beside();
    }
    let again = "run `kgraf sandbox setup` again";
    let name = service.name;
    let image = match installed_image() {
        Ok(Some(image)) => image,
        Ok(None) => {
            return Err(format!(
                "setup recorded the session-0 launch path, but kGraf's launcher service {name} \
                 is not installed; {again}"
            ));
        }
        Err(why) => {
            return Err(format!(
                "the runner kGraf's launcher service {name} starts could not be read from the \
                 service's configuration: {why}; {again}"
            ));
        }
    };
    let args = super::service_args::ServiceArgs::from_image_path(&image).map_err(|e| {
        format!(
            "kGraf's launcher service {name} is not configured as setup installs it: {e}; {again}"
        )
    })?;
    if args.owner_sid != service.owner_sid {
        return Err(format!(
            "kGraf's launcher service {name} answers {}, not this Windows user {}; {again}",
            args.owner_sid, service.owner_sid
        ));
    }
    if args.pipe != service.pipe {
        return Err(format!(
            "kGraf's launcher service {name} listens on the pipe {}, not {}; {again}",
            args.pipe, service.pipe
        ));
    }
    Ok(std::path::PathBuf::from(args.runner))
}

// ---- the service protocol ---------------------------------------------------

pub const REQUEST_MAGIC: &[u8; 8] = b"KGRSVCRQ";
pub const REQUEST_VERSION: u8 = 1;

/// The longest text field the service reads, in UTF-16 units: a Windows
/// command line's limit.
pub const MAX_TEXT_UNITS: usize = 32_767;
/// The longest reply line the owner's side reads.
pub const MAX_REPLY: usize = 4096;
/// The longest request the service reads before it has checked anything: the
/// header, four text fields at their limit and the fixed words.
pub const MAX_REQUEST_BYTES: usize = 9 + 4 * (4 + 2 * MAX_TEXT_UNITS) + 8 * 5;

/// The service's own subcommand of the kGraf executable. The service is
/// installed with the executable's recorded path and this word, so the
/// service control manager starts nothing else.
pub const SERVICE_SUBCOMMAND: &str = "sandbox-launcher-service";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ServiceError {
    #[error("the launcher request is not one kGraf wrote")]
    Magic,
    #[error("the launcher request has version {0}, which this launcher does not read")]
    Version(u8),
    #[error("the launcher request is cut short")]
    Truncated,
    #[error("the launcher request's {0} is too long")]
    TooLong(&'static str),
    #[error("the launcher request's {0} is malformed")]
    Field(&'static str),
    #[error("the launcher request's {0} is empty")]
    Empty(&'static str),
    #[error("the launcher request names no {0} handle")]
    NoHandle(&'static str),
    #[error("the launcher request names a runner the service was not installed with")]
    RunnerPath,
    #[error("the launcher request did not come from the owner: it came from {0}")]
    NotOwner(String),
    #[error("the launcher reply is not understood")]
    Reply,
    #[error("reading the launcher pipe failed: {0}")]
    Io(String),
}

/// What the owner's process asks the service to start: kGraf's runner, as one
/// slot user, with the owner's own standard handles and job.
///
/// The password is in the request because the slot passwords are sealed for
/// the owner's account (DPAPI for the current user), which the service, as
/// LocalSystem, cannot open. It travels over a pipe whose access list admits
/// SYSTEM and the owner alone, is never written to disk, and both ends clear
/// it from memory when they are done with it.
#[derive(Clone, PartialEq, Eq)]
pub struct ServiceRequest {
    /// The owner's process, checked against the pipe client's process ID.
    pub owner_pid: u32,
    /// The runner's full path. The service refuses any path but the one it
    /// was installed with.
    pub runner_exe: Vec<u16>,
    /// The runner's command line, which carries only the owner's pipe name
    /// and process ID.
    pub command_line: Vec<u16>,
    pub user_name: Vec<u16>,
    pub user_sid: Vec<u16>,
    pub password: Vec<u16>,
    /// The owner's standard handles and job, as handle values in the owner's
    /// own process. The service duplicates them out of that process and
    /// refuses any whose object is not a pipe, a file or a job.
    pub stdin: u64,
    pub stdout: u64,
    pub stderr: u64,
    pub job: u64,
}

impl fmt::Debug for ServiceRequest {
    /// No password, no command line: this is printed in errors.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServiceRequest")
            .field("owner_pid", &self.owner_pid)
            .field("user_name", &String::from_utf16_lossy(&self.user_name))
            .field("user_sid", &String::from_utf16_lossy(&self.user_sid))
            .finish_non_exhaustive()
    }
}

impl Drop for ServiceRequest {
    fn drop(&mut self) {
        self.password.zeroize();
    }
}

impl ServiceRequest {
    /// Checks every field, so neither end sends or acts on a request the
    /// other would refuse.
    pub fn validate(&self) -> Result<(), ServiceError> {
        text("runner path", &self.runner_exe)?;
        text("command line", &self.command_line)?;
        text("user name", &self.user_name)?;
        text("user SID", &self.user_sid)?;
        text("password", &self.password)?;
        if self.owner_pid == 0 {
            return Err(ServiceError::Field("owner process ID"));
        }
        for (name, h) in [
            ("standard input", self.stdin),
            ("standard output", self.stdout),
            ("standard error", self.stderr),
            ("job", self.job),
        ] {
            // Windows gives no process the handle 0, and -1 is the
            // pseudo-handle of the current process, which would name the
            // service's own.
            if h == 0 || h as i64 == -1 {
                return Err(ServiceError::NoHandle(name));
            }
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<Vec<u8>, ServiceError> {
        self.validate()?;
        // The whole frame at its exact size before anything is written: the
        // password is in it, and a buffer that grows as it is filled gives up
        // buffers that keep a copy of the password, which nothing clears.
        let fields = [
            &self.runner_exe,
            &self.command_line,
            &self.user_name,
            &self.user_sid,
            &self.password,
        ];
        let size = 9 + 4 + 8 * 4 + fields.iter().map(|f| 4 + f.len() * 2).sum::<usize>();
        let mut out = Vec::with_capacity(size);
        out.extend_from_slice(REQUEST_MAGIC);
        out.push(REQUEST_VERSION);
        out.extend_from_slice(&self.owner_pid.to_le_bytes());
        for h in [self.stdin, self.stdout, self.stderr, self.job] {
            out.extend_from_slice(&h.to_le_bytes());
        }
        for field in fields {
            out.extend_from_slice(&(field.len() as u32).to_le_bytes());
            for unit in field {
                out.extend_from_slice(&unit.to_le_bytes());
            }
        }
        debug_assert_eq!(out.len(), size, "the frame's size was computed wrong");
        Ok(out)
    }

    /// Read one request from `src`, which is the service's pipe. Nothing but
    /// the header is read before the magic and the version are checked, and
    /// no field longer than [`MAX_TEXT_UNITS`] is read.
    pub fn decode(src: &mut impl Read) -> Result<Self, ServiceError> {
        let mut header = [0u8; 9];
        read_exact(src, &mut header)?;
        if &header[..8] != REQUEST_MAGIC {
            return Err(ServiceError::Magic);
        }
        if header[8] != REQUEST_VERSION {
            return Err(ServiceError::Version(header[8]));
        }
        let mut four = [0u8; 4];
        read_exact(src, &mut four)?;
        let owner_pid = u32::from_le_bytes(four);
        let mut handles = [0u64; 4];
        for h in &mut handles {
            let mut eight = [0u8; 8];
            read_exact(src, &mut eight)?;
            *h = u64::from_le_bytes(eight);
        }
        let mut fields: Vec<Vec<u16>> = Vec::with_capacity(5);
        for name in [
            "runner path",
            "command line",
            "user name",
            "user SID",
            "password",
        ] {
            read_exact(src, &mut four)?;
            let units = u32::from_le_bytes(four) as usize;
            if units > MAX_TEXT_UNITS {
                return Err(ServiceError::TooLong(name));
            }
            // One field holds the password, so the bytes it is read into are
            // cleared when they are dropped, and the text is built in a
            // buffer of its exact size: a buffer that grows gives up buffers
            // that keep a copy of the text, and nothing clears those.
            let mut bytes = Zeroizing::new(vec![0u8; units * 2]);
            read_exact(src, &mut bytes)?;
            let mut field = Vec::with_capacity(units);
            field.extend(
                bytes
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| u16::from_le_bytes(*c)),
            );
            fields.push(field);
        }
        let mut it = fields.into_iter();
        let mut next = || it.next().unwrap_or_default();
        let request = Self {
            owner_pid,
            runner_exe: next(),
            command_line: next(),
            user_name: next(),
            user_sid: next(),
            password: next(),
            stdin: handles[0],
            stdout: handles[1],
            stderr: handles[2],
            job: handles[3],
        };
        request.validate()?;
        Ok(request)
    }
}

/// A text field: not empty, within the limit and with no NUL inside, which
/// a Windows call would read as the end of the string.
fn text(name: &'static str, units: &[u16]) -> Result<(), ServiceError> {
    if units.is_empty() {
        return Err(ServiceError::Empty(name));
    }
    if units.len() > MAX_TEXT_UNITS {
        return Err(ServiceError::TooLong(name));
    }
    if units.contains(&0) {
        return Err(ServiceError::Field(name));
    }
    Ok(())
}

fn read_exact(src: &mut impl Read, buf: &mut [u8]) -> Result<(), ServiceError> {
    match src.read_exact(buf) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Err(ServiceError::Truncated),
        Err(e) => Err(ServiceError::Io(e.to_string())),
    }
}

/// What the service answers. One line, so the owner's side reads it with no
/// frame: the runner's process ID and whether its access lists were written
/// before it ran, or the step that failed with its Win32 error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServiceReply {
    Started {
        pid: u32,
        /// The runner's process handle, as a handle value in the OWNER's
        /// process: the service duplicates it there before it replies.
        ///
        /// The owner needs it to wait for the command and to read its exit
        /// status, and cannot open the process itself: the runner's own
        /// access list admits the sandbox user and SYSTEM alone, never the
        /// owner (`windows::runner::protect_before_resume`). A handle keeps
        /// the rights it was made with, so one the service duplicated goes on
        /// working after that list is written.
        handle: u64,
        /// `Ok(())` when the runner's process and main thread got their
        /// protected access lists before it ran. The text of an error says
        /// the step and the Win32 error, and the owner's side keeps it on the
        /// started command, as it does on the owner-session path.
        protected: Result<(), String>,
    },
    Failed {
        step: String,
        code: u32,
        text: String,
    },
}

impl fmt::Display for ServiceReply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ServiceReply::Started { pid, protected, .. } => match protected {
                Ok(()) => write!(f, "the launcher started the runner as process {pid}"),
                Err(why) => write!(
                    f,
                    "the launcher started the runner as process {pid}, and its access lists \
                     could not be written before it ran: {why}"
                ),
            },
            ServiceReply::Failed { step, code, text } => {
                write!(
                    f,
                    "the launcher could not {step}: {text} (Windows error {code})"
                )
            }
        }
    }
}

impl ServiceReply {
    /// The line sent over the pipe. One line, no NUL and no newline inside:
    /// every text here is built by the service from its own words and a
    /// Windows error, and a newline in either is replaced by a space.
    pub fn line(&self) -> String {
        match self {
            ServiceReply::Started {
                pid,
                handle,
                protected,
            } => match protected {
                Ok(()) => format!("started {pid} {handle} protected\n"),
                Err(why) => format!("started {pid} {handle} unprotected {}\n", one_line(why)),
            },
            ServiceReply::Failed { step, code, text } => format!(
                "fail {} {code} {}\n",
                one_line(step).replace(' ', "_"),
                one_line(text)
            ),
        }
    }

    pub fn parse(line: &str) -> Result<Self, ServiceError> {
        let mut words = line.trim_end().split(' ');
        match words.next() {
            Some("started") => {
                let pid = words
                    .next()
                    .and_then(|w| w.parse().ok())
                    .ok_or(ServiceError::Reply)?;
                let handle = words
                    .next()
                    .and_then(|w| w.parse().ok())
                    .ok_or(ServiceError::Reply)?;
                if handle == 0 {
                    return Err(ServiceError::Reply);
                }
                let protected = match words.next() {
                    Some("protected") => Ok(()),
                    Some("unprotected") => {
                        Err(words.collect::<Vec<_>>().join(" ").trim().to_owned())
                    }
                    _ => return Err(ServiceError::Reply),
                };
                Ok(ServiceReply::Started {
                    pid,
                    handle,
                    protected,
                })
            }
            Some("fail") => {
                let step = words.next().ok_or(ServiceError::Reply)?.replace('_', " ");
                let code = words
                    .next()
                    .and_then(|w| w.parse().ok())
                    .ok_or(ServiceError::Reply)?;
                Ok(ServiceReply::Failed {
                    step,
                    code,
                    text: words.collect::<Vec<_>>().join(" "),
                })
            }
            _ => Err(ServiceError::Reply),
        }
    }
}

/// `text` with every line break turned into a space, so one reply is one
/// line.
fn one_line(text: &str) -> String {
    text.replace(['\r', '\n'], " ").trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWNER: &str = "S-1-5-21-1-2-3-1001";
    const SERVICE: ExpectedService<'static> = ExpectedService {
        name: "kgraf-launcher-1a2b3c4d",
        owner_sid: OWNER,
        pipe: "kgraf-launcher-1a2b3c4d",
    };
    const INSTALLED_RUNNER: &str = r"C:\Program Files\kGraf\kgraf-sandbox-runner.exe";

    /// The image path setup installs for [`SERVICE`].
    fn installed_image() -> String {
        let args = super::super::service_args::ServiceArgs {
            owner_sid: OWNER.to_owned(),
            pipe: SERVICE.pipe.to_owned(),
            runner: INSTALLED_RUNNER.to_owned(),
        };
        super::super::service_dacl::image_path(
            std::path::Path::new(r"C:\Program Files\kGraf\kgraf.exe"),
            &args.command_line(),
        )
        .expect("image path")
    }

    /// A session-0 launch names the runner the service was installed with,
    /// never the copy beside the calling program: the service starts only
    /// its own, so a test binary in another folder that named its own copy
    /// had every command refused.
    #[test]
    fn a_session0_launch_names_the_installed_runner_and_never_the_one_beside() {
        let runner = runner_for(
            LaunchPath::Session0,
            SERVICE,
            || Ok(Some(installed_image())),
            || panic!("a session-0 launch looked for a runner beside the program"),
        )
        .expect("the installed runner");
        assert_eq!(runner, std::path::PathBuf::from(INSTALLED_RUNNER));
    }

    /// The owner-session path starts the runner itself, so it takes the one
    /// beside the program and never asks the service.
    #[test]
    fn an_owner_session_launch_names_the_runner_beside_the_program() {
        let beside = std::path::PathBuf::from(r"C:\kgraf\tests\kgraf-sandbox-runner.exe");
        let runner = runner_for(
            LaunchPath::OwnerSession,
            SERVICE,
            || panic!("an owner-session launch read the service"),
            || Ok(beside.clone()),
        )
        .expect("the runner beside");
        assert_eq!(runner, beside);
        let missing = runner_for(
            LaunchPath::OwnerSession,
            SERVICE,
            || panic!("an owner-session launch read the service"),
            || Err("kgraf-sandbox-runner.exe is not installed beside kgraf.exe".to_owned()),
        );
        assert!(missing.is_err());
    }

    /// Where the installed runner cannot be known, the launch is refused with
    /// one line that says to run setup again; it never falls back to the
    /// runner beside the program.
    #[test]
    fn a_missing_or_foreign_service_is_refused_with_what_to_run() {
        let session0 = |image: Result<Option<String>, String>| {
            runner_for(
                LaunchPath::Session0,
                SERVICE,
                || image,
                || {
                    Ok(std::path::PathBuf::from(
                        r"C:\elsewhere\kgraf-sandbox-runner.exe",
                    ))
                },
            )
            .expect_err("refused")
        };
        let other_owner = installed_image().replace(OWNER, "S-1-5-21-9-9-9-1002");
        let other_pipe = installed_image().replace(
            &format!("--pipe {}", SERVICE.pipe),
            "--pipe kgraf-launcher-ffffffff",
        );
        for (case, why) in [
            ("not installed", session0(Ok(None))),
            (
                "unreadable",
                session0(Err("Access is denied. (os error 5)".into())),
            ),
            (
                "no subcommand",
                session0(Ok(Some(r#""C:\k\kgraf.exe" --pipe x"#.into()))),
            ),
            ("another owner", session0(Ok(Some(other_owner)))),
            ("another pipe", session0(Ok(Some(other_pipe)))),
        ] {
            assert!(
                why.ends_with("run `kgraf sandbox setup` again"),
                "{case}: {why}"
            );
            assert!(!why.contains('\n'), "{case}: one line: {why}");
            assert!(
                why.contains(SERVICE.name),
                "{case}: names the service: {why}"
            );
            assert!(!why.contains("elsewhere"), "{case}: no fallback: {why}");
        }
    }

    fn request() -> ServiceRequest {
        ServiceRequest {
            owner_pid: 4321,
            runner_exe: "C:\\Program Files\\kGraf\\kgraf-sandbox-runner.exe"
                .encode_utf16()
                .collect(),
            command_line: "runner --pipe kgraf-run-00ff --owner-pid 4321"
                .encode_utf16()
                .collect(),
            user_name: "kgraf-off-0-abcdef".encode_utf16().collect(),
            user_sid: "S-1-5-21-1-2-3-1010".encode_utf16().collect(),
            password: "a very long random password".encode_utf16().collect(),
            stdin: 0x24,
            stdout: 0x28,
            stderr: 0x2c,
            job: 0x30,
        }
    }

    #[test]
    fn a_request_survives_a_round_trip() {
        let sent = request();
        let bytes = sent.encode().expect("encode");
        let read = ServiceRequest::decode(&mut bytes.as_slice()).expect("decode");
        assert_eq!(read, sent);
    }

    #[test]
    fn the_debug_text_carries_no_password_and_no_command_line() {
        let text = format!("{:?}", request());
        assert!(!text.contains("password"), "{text}");
        assert!(!text.contains("--pipe"), "{text}");
        assert!(text.contains("kgraf-off-0-abcdef"), "{text}");
    }

    #[test]
    fn a_request_without_a_handle_is_refused() {
        for (name, set) in [
            (
                "standard input",
                (|r: &mut ServiceRequest| r.stdin = 0) as fn(&mut ServiceRequest),
            ),
            ("standard output", |r| r.stdout = 0),
            ("standard error", |r| r.stderr = 0),
            ("job", |r| r.job = 0),
            // The pseudo-handle of the current process would name the
            // service's own handle, not the owner's.
            ("standard input", |r| r.stdin = u64::MAX),
        ] {
            let mut r = request();
            set(&mut r);
            assert_eq!(r.validate(), Err(ServiceError::NoHandle(name)));
            assert!(r.encode().is_err());
        }
    }

    #[test]
    fn an_empty_or_overlong_field_is_refused() {
        let mut r = request();
        r.user_name = Vec::new();
        assert_eq!(r.validate(), Err(ServiceError::Empty("user name")));
        let mut r = request();
        r.password = vec![b'x' as u16; MAX_TEXT_UNITS + 1];
        assert_eq!(r.validate(), Err(ServiceError::TooLong("password")));
        let mut r = request();
        r.runner_exe = vec![b'c' as u16, 0, b'x' as u16];
        assert_eq!(r.validate(), Err(ServiceError::Field("runner path")));
        let mut r = request();
        r.owner_pid = 0;
        assert_eq!(r.validate(), Err(ServiceError::Field("owner process ID")));
    }

    #[test]
    fn a_foreign_or_cut_short_request_is_refused() {
        let mut other = b"KGRXXXXX".to_vec();
        other.push(REQUEST_VERSION);
        assert_eq!(
            ServiceRequest::decode(&mut other.as_slice()),
            Err(ServiceError::Magic)
        );
        let mut later = REQUEST_MAGIC.to_vec();
        later.push(REQUEST_VERSION + 1);
        assert_eq!(
            ServiceRequest::decode(&mut later.as_slice()),
            Err(ServiceError::Version(REQUEST_VERSION + 1))
        );
        let whole = request().encode().expect("encode");
        for cut in [0, 5, 9, 20, whole.len() - 1] {
            assert_eq!(
                ServiceRequest::decode(&mut &whole[..cut]),
                Err(ServiceError::Truncated),
                "cut at {cut}"
            );
        }
    }

    /// A field length the sender never writes must not make the service read
    /// that much before it refuses.
    #[test]
    fn an_overlong_field_length_is_refused_before_the_field_is_read() {
        let mut bytes = REQUEST_MAGIC.to_vec();
        bytes.push(REQUEST_VERSION);
        bytes.extend_from_slice(&1u32.to_le_bytes());
        for _ in 0..4 {
            bytes.extend_from_slice(&0x24u64.to_le_bytes());
        }
        bytes.extend_from_slice(&(MAX_TEXT_UNITS as u32 + 1).to_le_bytes());
        assert_eq!(
            ServiceRequest::decode(&mut bytes.as_slice()),
            Err(ServiceError::TooLong("runner path"))
        );
    }

    #[test]
    fn a_reply_survives_a_round_trip_on_one_line() {
        for reply in [
            ServiceReply::Started {
                pid: 7,
                handle: 0x2c,
                protected: Ok(()),
            },
            ServiceReply::Started {
                pid: 7,
                handle: u64::from(u32::MAX),
                protected: Err("write the runner's list: Windows error 5".into()),
            },
            ServiceReply::Failed {
                step: "log the sandbox user on".into(),
                code: 1385,
                text: "Logon failure: the user has not been granted the requested logon type"
                    .into(),
            },
        ] {
            let line = reply.line();
            assert_eq!(line.matches('\n').count(), 1, "{line}");
            assert!(line.ends_with('\n'), "{line}");
            assert_eq!(ServiceReply::parse(&line), Ok(reply.clone()), "{line}");
            assert!(line.len() <= MAX_REPLY);
        }
    }

    /// A Windows message with a line break in it must not end the reply
    /// early.
    #[test]
    fn a_reply_holds_a_windows_message_that_carries_line_breaks() {
        let reply = ServiceReply::Failed {
            step: "start the runner".into(),
            code: 5,
            text: "Access is denied.\r\nStep two\n".into(),
        };
        let line = reply.line();
        assert_eq!(line.matches('\n').count(), 1, "{line}");
        let read = ServiceReply::parse(&line).expect("parse");
        assert_eq!(
            read,
            ServiceReply::Failed {
                step: "start the runner".into(),
                code: 5,
                text: "Access is denied.  Step two".into(),
            }
        );
    }

    #[test]
    fn a_reply_that_is_not_understood_is_refused() {
        for line in [
            "",
            "started\n",
            "started x 4 protected\n",
            "started 7 protected\n",
            // A handle value of zero names no object.
            "started 7 0 protected\n",
            "fail step\n",
            "ok\n",
        ] {
            assert_eq!(
                ServiceReply::parse(line),
                Err(ServiceError::Reply),
                "{line}"
            );
        }
    }

    /// Every path kGraf can record survives the file, and the probe's
    /// result with it.
    #[test]
    fn the_recorded_path_survives_a_round_trip() {
        for active in [
            Active::session0_proved(),
            Active {
                path: LaunchPath::OwnerSession,
                granted: true,
                proved: Some(Proof::Refused("exit 5, denied\nsecond line".into())),
                because: "the service could not be set up".into(),
            },
            Active {
                path: LaunchPath::OwnerSession,
                granted: false,
                proved: None,
                because: String::new(),
            },
        ] {
            let text = active.encode();
            let read = Active::decode(&text);
            // The record holds one line per field, so a reason that carried
            // a line break comes back as one line.
            let want = Active {
                proved: active.proved.clone().map(|p| match p {
                    Proof::Refused(why) => Proof::Refused(one_line(&why)),
                    Proof::CouldNotRun(why) => Proof::CouldNotRun(one_line(&why)),
                    Proof::Made => Proof::Made,
                }),
                ..active.clone()
            };
            assert_eq!(read, want, "{text}");
        }
    }

    /// A missing or damaged record must read as the path that promises the
    /// least, never as one that says cargo works.
    #[test]
    fn a_damaged_record_reads_as_the_owner_session_with_no_grant() {
        for text in ["", "path: nonsense\n", "granted: yes\n", "noise\n\n\n"] {
            let read = Active::decode(text);
            assert_eq!(read.path, LaunchPath::OwnerSession, "{text:?}");
            assert!(!read.granted, "{text:?}");
            assert_eq!(read.proved, None, "{text:?}");
            assert!(!read.cargo_runs(), "{text:?}");
        }
    }

    #[test]
    fn the_launch_path_keys_round_trip() {
        for path in [LaunchPath::Session0, LaunchPath::OwnerSession] {
            assert_eq!(LaunchPath::from_key(path.key()), Some(path));
        }
        assert_eq!(LaunchPath::from_key("session0"), None);
        assert!(LaunchPath::Session0.named_objects_work(false));
        assert!(!LaunchPath::OwnerSession.named_objects_work(false));
        assert!(LaunchPath::OwnerSession.named_objects_work(true));
    }

    /// The service path is taken when it is installed and its probe did not
    /// fail, the grant when the service failed, and the bare owner session
    /// when both failed. Every fallback says why.
    #[test]
    fn selection_takes_the_service_then_the_grant_then_neither() {
        let proved = select(&Attempt {
            service_probe: Some(Proof::Made),
            ..Attempt::default()
        });
        assert_eq!(proved, Active::session0_proved());
        assert!(proved.cargo_runs());
        assert!(proved.because.is_empty());

        let fell_back = select(&Attempt {
            service_failed: Some("the service could not be created: Windows error 5".into()),
            granted: true,
            grant_probe: Some(Proof::Made),
            ..Attempt::default()
        });
        assert_eq!(fell_back.path, LaunchPath::OwnerSession);
        assert!(fell_back.granted);
        assert!(fell_back.cargo_runs());
        assert!(
            fell_back.because.contains("Windows error 5"),
            "{fell_back:?}"
        );

        let probe_failed = select(&Attempt {
            service_probe: Some(Proof::Refused("exit 5, denied".into())),
            granted: true,
            grant_probe: Some(Proof::Made),
            ..Attempt::default()
        });
        assert_eq!(probe_failed.path, LaunchPath::OwnerSession);
        assert!(probe_failed.because.contains("its proof failed"));

        let neither = select(&Attempt {
            service_failed: Some("no administrator rights".into()),
            grant_failed: Some("the directory refused the write".into()),
            ..Attempt::default()
        });
        assert_eq!(neither.path, LaunchPath::OwnerSession);
        assert!(!neither.granted);
        assert!(!neither.cargo_runs());
        assert!(
            neither.because.contains("no administrator rights"),
            "{neither:?}"
        );
        assert!(
            neither.because.contains("the directory refused the write"),
            "{neither:?}"
        );
    }

    /// A probe that ran and failed decides, whatever the path promises: a
    /// command must never be told cargo works when a measurement says it
    /// does not.
    #[test]
    fn a_failed_probe_means_cargo_does_not_run() {
        let active = Active {
            path: LaunchPath::Session0,
            granted: false,
            proved: Some(Proof::Refused("exit 5, denied".into())),
            because: String::new(),
        };
        assert!(!active.cargo_runs());
        assert!(
            active
                .line()
                .contains("The proof failed, a named object was refused: exit 5, denied")
        );
        // A probe that could not run says nothing about named objects, so
        // the path stays unproved and cargo is not promised either.
        let could_not = Active {
            proved: Some(Proof::CouldNotRun("the probe binary did not start".into())),
            ..active.clone()
        };
        assert!(!could_not.cargo_runs());
        assert!(could_not.line().contains("the probe could not run"));
        // A path nothing proved promises nothing, so a failed cargo still
        // gets the note that says why.
        let unproved = Active {
            proved: None,
            ..active
        };
        assert!(!unproved.cargo_runs());
    }

    /// The exit status and the output decide which of the three outcomes a
    /// probe had, and a failure that is not an access denial is never read
    /// as a refused named object.
    #[test]
    fn a_probes_outcome_follows_its_status_and_its_output() {
        assert_eq!(Proof::of(Some(0), "made both"), Proof::Made);
        assert_eq!(
            Proof::of(Some(1), "CreateMutexW: Access is denied. (os error 5)"),
            Proof::Refused("CreateMutexW: Access is denied. (os error 5)".into())
        );
        assert_eq!(
            Proof::of(Some(1), "the program could not start\nsecond line"),
            Proof::CouldNotRun("the program could not start second line".into())
        );
        // A command Windows ended with no status at all could not run.
        assert_eq!(
            Proof::of(None, "ended by a signal"),
            Proof::CouldNotRun("ended by a signal".into())
        );
    }

    /// Every path says what it is, and the fallback with the grant says what
    /// it costs.
    #[test]
    fn each_line_names_the_path_and_the_fallback_names_its_risk() {
        let proved = Active::session0_proved().line();
        assert!(proved.contains("session 0"), "{proved}");
        assert!(proved.contains("Proved:"), "{proved}");
        // The path that is taken says what it costs, as the fallback does: a
        // boundary this one does not keep must not be left unsaid.
        assert!(proved.contains("What this path costs"), "{proved}");
        assert!(proved.contains("machine-wide object directory"), "{proved}");

        let granted = Active {
            path: LaunchPath::OwnerSession,
            granted: true,
            proved: Some(Proof::Made),
            because: "the service could not be set up".into(),
        }
        .line();
        assert!(granted.contains("Risk of this fallback"), "{granted}");
        assert!(
            granted.contains("the owner's own programs read"),
            "{granted}"
        );
        assert!(granted.contains("Why this path:"), "{granted}");

        let neither = Active {
            path: LaunchPath::OwnerSession,
            granted: false,
            proved: None,
            because: String::new(),
        }
        .line();
        assert!(
            neither.contains("cannot run as a sandbox user"),
            "{neither}"
        );
        assert!(neither.contains("no probe has run"), "{neither}");
    }
}

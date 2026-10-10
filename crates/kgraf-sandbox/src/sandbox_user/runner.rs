//! What the owner's process and the runner say to each other (decision 0097).
//!
//! A command that runs as the sandbox user starts in two stages.
//! `CreateProcessWithLogonW`, the only call a standard user can use to start a
//! process as another user, starts the runner as the sandbox user; the runner
//! then starts the real command with a token whose logon SID is deny-only.
//! That call gives its child the caller's logon SID, which would admit the
//! command to objects in the owner's session, and limits the command line to
//! 1,024 characters. So the runner's own command line carries only the pipe's
//! name and the owner's process ID, and the command comes over the pipe:
//!
//! ```text
//!   owner                                   runner (sandbox user)
//!     create the command's window station
//!       and desktop (once per sandbox user)
//!     create pipe (first instance, DACL:
//!       owner and sandbox user)
//!     CreateProcessWithLogonW(runner --pipe <name> --owner-pid <pid>)
//!                                           connect; server PID == owner PID?
//!     client PID == runner PID?
//!     --- request (binary, below) --------->
//!                                           start the command on the
//!                                             named desktop
//!     <-------------- "started <pid>\n" ---  or "fail <step> <code> <text>\n"
//!                                           wait; exit with the command's code
//! ```
//!
//! The request is one binary frame, little-endian:
//!
//! ```text
//! "KGRUNREQ" | version u8 = 2
//! 5 fields, each: length u32 (UTF-16 units) | UTF-16LE units
//!   application   full path of the program, no NUL
//!   command line  argv[0] and the arguments, quoted, no NUL
//!   directory     the working directory, no NUL
//!   environment   a CreateProcess environment block: NAME=value\0 ...\0
//!   desktop       <window station>\<desktop> for the command, no NUL
//! ```
//!
//! **The command's desktop comes from the owner.** The owner's process makes
//! the window station and desktop the command runs on, with access lists that
//! admit SYSTEM, the owner and that sandbox user, and names them here. The
//! runner only uses them: whether a local user started through the secondary
//! logon service may create a window station in the session is not
//! documented, and a refusal there would stop every command.
//!
//! Everything here is platform-neutral, so the format is tested on every
//! host; the Windows calls are in `windows::runner` and `windows::launch`.

use std::fmt;
use std::io::Read;

use super::handshake::{is_nonce, new_nonce};

pub const REQUEST_MAGIC: &[u8; 8] = b"KGRUNREQ";
pub const REQUEST_VERSION: u8 = 2;

/// The longest path, command line or working directory a Windows process
/// call accepts, in UTF-16 units (`CreateProcessW`'s command line limit).
pub const MAX_TEXT_UNITS: usize = 32_767;

/// The longest `<window station>\<desktop>` name accepted, in UTF-16 units.
/// Windows limits each part to `MAX_PATH`.
pub const MAX_DESKTOP_UNITS: usize = 2 * 260 + 1;

/// The largest environment block accepted, in UTF-16 units. Windows sets no
/// fixed limit; this bounds what the runner reads before it has checked
/// anything.
pub const MAX_ENV_UNITS: usize = 4 * 1024 * 1024;

/// The longest reply line the owner accepts.
pub const MAX_REPLY: usize = 4096;

/// The runner's executable name, without the platform suffix.
pub const RUNNER_NAME: &str = "kgraf-sandbox-runner";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RunnerError {
    #[error("the runner request is not one kGraf wrote")]
    Magic,
    #[error("the runner request has version {0}, which this runner does not read")]
    Version(u8),
    #[error("the runner request is cut short")]
    Truncated,
    #[error("the runner request's {0} is too long")]
    TooLong(&'static str),
    #[error("the runner request's {0} is malformed")]
    Field(&'static str),
    #[error("the runner's arguments are malformed: {0}")]
    Args(String),
    #[error("the runner's reply is not understood")]
    Reply,
    #[error("reading the runner pipe failed: {0}")]
    Io(String),
}

/// What the runner starts. Each field is UTF-16, as the Windows calls take it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub application: Vec<u16>,
    pub command_line: Vec<u16>,
    pub directory: Vec<u16>,
    pub environment: Vec<u16>,
    /// `<window station>\<desktop>`, which the owner's process made for this
    /// sandbox user; filled in by `windows::launch::start`.
    pub desktop: Vec<u16>,
}

impl Request {
    /// Checks every field, so neither end sends or acts on a request the
    /// other would refuse.
    pub fn validate(&self) -> Result<(), RunnerError> {
        text("application", &self.application)?;
        text("command line", &self.command_line)?;
        text("directory", &self.directory)?;
        env_block(&self.environment)?;
        desktop_name(&self.desktop)
    }

    pub fn encode(&self) -> Result<Vec<u8>, RunnerError> {
        self.validate()?;
        let units = self.application.len()
            + self.command_line.len()
            + self.directory.len()
            + self.environment.len()
            + self.desktop.len();
        let mut out = Vec::with_capacity(9 + 20 + units * 2);
        out.extend_from_slice(REQUEST_MAGIC);
        out.push(REQUEST_VERSION);
        for field in [
            &self.application,
            &self.command_line,
            &self.directory,
            &self.environment,
            &self.desktop,
        ] {
            // Validated above: each length fits in u32.
            out.extend_from_slice(&(field.len() as u32).to_le_bytes());
            for unit in field {
                out.extend_from_slice(&unit.to_le_bytes());
            }
        }
        Ok(out)
    }

    /// Reads exactly one request from `r`. Each length is checked before
    /// anything of that size is read.
    pub fn read_from(r: &mut impl Read) -> Result<Self, RunnerError> {
        let mut magic = [0u8; 8];
        read_exact(r, &mut magic)?;
        if &magic != REQUEST_MAGIC {
            return Err(RunnerError::Magic);
        }
        let mut version = [0u8; 1];
        read_exact(r, &mut version)?;
        if version[0] != REQUEST_VERSION {
            return Err(RunnerError::Version(version[0]));
        }
        let request = Request {
            application: read_field(r, "application", MAX_TEXT_UNITS)?,
            command_line: read_field(r, "command line", MAX_TEXT_UNITS)?,
            directory: read_field(r, "directory", MAX_TEXT_UNITS)?,
            environment: read_field(r, "environment", MAX_ENV_UNITS)?,
            desktop: read_field(r, "desktop", MAX_DESKTOP_UNITS)?,
        };
        request.validate()?;
        Ok(request)
    }
}

fn read_exact(r: &mut impl Read, buf: &mut [u8]) -> Result<(), RunnerError> {
    r.read_exact(buf).map_err(|e| match e.kind() {
        std::io::ErrorKind::UnexpectedEof => RunnerError::Truncated,
        _ => RunnerError::Io(e.to_string()),
    })
}

fn read_field(r: &mut impl Read, what: &'static str, max: usize) -> Result<Vec<u16>, RunnerError> {
    let mut len = [0u8; 4];
    read_exact(r, &mut len)?;
    let n = u32::from_le_bytes(len) as usize;
    if n > max {
        return Err(RunnerError::TooLong(what));
    }
    let mut bytes = vec![0u8; n * 2];
    read_exact(r, &mut bytes)?;
    Ok(bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| u16::from_le_bytes(*b))
        .collect())
}

/// A path or command line: not empty, at most [`MAX_TEXT_UNITS`], no NUL.
fn text(what: &'static str, units: &[u16]) -> Result<(), RunnerError> {
    if units.len() > MAX_TEXT_UNITS {
        return Err(RunnerError::TooLong(what));
    }
    if units.is_empty() || units.contains(&0) {
        return Err(RunnerError::Field(what));
    }
    Ok(())
}

/// `<window station>\<desktop>`: both parts present, one backslash between
/// them, no NUL, at most [`MAX_DESKTOP_UNITS`].
fn desktop_name(units: &[u16]) -> Result<(), RunnerError> {
    const WHAT: &str = "desktop";
    if units.len() > MAX_DESKTOP_UNITS {
        return Err(RunnerError::TooLong(WHAT));
    }
    // Empty: the window station Windows made for the runner's own logon
    // session, which is what the session-0 path uses. The owner's process
    // cannot make a window station in another session, and Windows makes one
    // for a noninteractive logon session and names it from that session's ID.
    // The command then runs on the runner's station, which is the same
    // sandbox user in the same logon session.
    if units.is_empty() {
        return Ok(());
    }
    let slash = u16::from(b'\\');
    let mut parts = units.split(|&u| u == slash);
    match (parts.next(), parts.next(), parts.next()) {
        (Some(station), Some(desktop), None)
            if !station.is_empty() && !desktop.is_empty() && !units.contains(&0) =>
        {
            Ok(())
        }
        _ => Err(RunnerError::Field(WHAT)),
    }
}

/// A `CreateProcess` environment block: each entry `NAME=value` and a NUL,
/// then one more NUL; an empty block is two NULs. An entry may start with `=`
/// (cmd.exe's per-drive directories, `=C:=C:\x`), so only the empty entry is
/// refused: it would end the block early and hide what follows.
fn env_block(units: &[u16]) -> Result<(), RunnerError> {
    const WHAT: &str = "environment";
    if units.len() > MAX_ENV_UNITS {
        return Err(RunnerError::TooLong(WHAT));
    }
    if units == [0, 0] {
        return Ok(());
    }
    let body = units
        .strip_suffix(&[0, 0])
        .ok_or(RunnerError::Field(WHAT))?;
    // The name ends at the first `=` after its first character, so an entry
    // with no `=` there has no name; an empty entry has none either.
    let named = |entry: &[u16]| {
        entry
            .get(1..)
            .is_some_and(|rest| rest.contains(&u16::from(b'=')))
    };
    if body.split(|&u| u == 0).all(named) {
        Ok(())
    } else {
        Err(RunnerError::Field(WHAT))
    }
}

/// The runner's own command line arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunnerArgs {
    /// The pipe's full name, `\\.\pipe\kgraf-sandbox-run-<nonce>`.
    pub pipe: String,
    /// The process that created the pipe and started the runner.
    pub owner_pid: u32,
    /// What the runner does with the logon SID in its own token.
    pub logon_sid: LogonSidPolicy,
}

/// What the runner does with the logon SID in its own token before it starts
/// the command. The two launch paths need opposite answers, so the owner's
/// side says which, and the runner checks that the token it built matches
/// what was asked (fail loud either way).
///
/// - [`LogonSidPolicy::Deny`], for a command in the owner's own session:
///   `CreateProcessWithLogonW` gives the runner the **caller's** logon SID,
///   which admits it to the objects the owner's processes make, so the
///   command's token must hold that SID deny-only.
/// - [`LogonSidPolicy::Keep`], for a command the launcher service starts in
///   session 0: the token comes from `LogonUser` for the sandbox user alone,
///   so its logon SID names a logon session of that user that nothing else
///   shares. Nothing of the owner's admits it, and the object directory of
///   session 0 is reached through it, which is the point of that path
///   (`super::session0`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LogonSidPolicy {
    /// Make every logon SID in the command's token deny-only. The default:
    /// a missing or unreadable option must never leave the SID enabled.
    #[default]
    Deny,
    /// Leave the logon SID enabled.
    Keep,
}

impl LogonSidPolicy {
    pub fn key(self) -> &'static str {
        match self {
            LogonSidPolicy::Deny => "deny",
            LogonSidPolicy::Keep => "keep",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "deny" => Some(LogonSidPolicy::Deny),
            "keep" => Some(LogonSidPolicy::Keep),
            _ => None,
        }
    }
}

const PIPE_PREFIX: &str = r"\\.\pipe\kgraf-sandbox-run-";

impl RunnerArgs {
    /// Arguments for a fresh run: a pipe name from the system random source.
    pub fn fresh(owner_pid: u32) -> Result<Self, RunnerError> {
        let nonce = new_nonce().map_err(|e| RunnerError::Args(e.to_string()))?;
        Ok(Self {
            pipe: format!("{PIPE_PREFIX}{nonce}"),
            owner_pid,
            logon_sid: LogonSidPolicy::default(),
        })
    }

    /// The same arguments with `logon_sid` set.
    #[must_use]
    pub fn with_logon_sid(mut self, policy: LogonSidPolicy) -> Self {
        self.logon_sid = policy;
        self
    }

    /// The arguments after the program name. None of them needs quoting.
    pub fn to_args(&self) -> [String; 6] {
        [
            "--pipe".to_owned(),
            self.pipe.clone(),
            "--owner-pid".to_owned(),
            self.owner_pid.to_string(),
            "--logon-sid".to_owned(),
            self.logon_sid.key().to_owned(),
        ]
    }

    /// Parses the arguments after the program name. Exactly the two options,
    /// once each: the runner acts on nothing else from its command line.
    pub fn parse<I, S>(args: I) -> Result<Self, RunnerError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let bad = |why: &str| RunnerError::Args(why.to_owned());
        let mut pipe = None;
        let mut owner_pid = None;
        let mut logon_sid = None;
        let mut it = args.into_iter();
        while let Some(flag) = it.next() {
            let value = it.next().ok_or_else(|| bad("an option has no value"))?;
            let value = value.as_ref();
            match flag.as_ref() {
                "--pipe" if pipe.is_none() => {
                    let nonce = value
                        .strip_prefix(PIPE_PREFIX)
                        .ok_or_else(|| bad("the pipe name is not a runner pipe"))?;
                    if !is_nonce(nonce) {
                        return Err(bad("the pipe name is not a runner pipe"));
                    }
                    pipe = Some(value.to_owned());
                }
                "--owner-pid" if owner_pid.is_none() => {
                    let pid: u32 = value
                        .parse()
                        .map_err(|_| bad("the owner PID is not a number"))?;
                    if pid == 0 {
                        return Err(bad("the owner PID is zero"));
                    }
                    owner_pid = Some(pid);
                }
                "--logon-sid" if logon_sid.is_none() => {
                    logon_sid = Some(
                        LogonSidPolicy::from_key(value)
                            .ok_or_else(|| bad("the logon SID policy is not understood"))?,
                    );
                }
                other => return Err(bad(&format!("unexpected argument {other:?}"))),
            }
        }
        Ok(Self {
            pipe: pipe.ok_or_else(|| bad("--pipe is missing"))?,
            owner_pid: owner_pid.ok_or_else(|| bad("--owner-pid is missing"))?,
            // A missing option means the deny-only default, which is what
            // every launch before this option asked for.
            logon_sid: logon_sid.unwrap_or_default(),
        })
    }
}

/// The runner's one reply line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    /// The command is running with this process ID.
    Started { pid: u32 },
    /// The runner could not start the command. `code` is the Win32 error, 0
    /// when none applies.
    Failed {
        step: String,
        code: u32,
        message: String,
    },
}

impl Reply {
    /// One line, with its newline. A failure's step and text are cut to one
    /// printable line that fits [`MAX_REPLY`].
    pub fn encode(&self) -> String {
        match self {
            Reply::Started { pid } => format!("started {pid}\n"),
            Reply::Failed {
                step,
                code,
                message,
            } => {
                let step: String = step
                    .chars()
                    .map(|c| {
                        if c.is_ascii_alphanumeric() || c == '-' {
                            c
                        } else {
                            '-'
                        }
                    })
                    .take(64)
                    .collect();
                let step = if step.is_empty() {
                    "-".to_owned()
                } else {
                    step
                };
                let mut text: String = message
                    .chars()
                    .map(|c| if c.is_control() { ' ' } else { c })
                    .collect();
                let cap = MAX_REPLY - 128;
                if text.len() > cap {
                    let mut end = cap;
                    while !text.is_char_boundary(end) {
                        end -= 1;
                    }
                    text.truncate(end);
                }
                format!("fail {step} {code} {text}\n")
            }
        }
    }

    pub fn parse(line: &str) -> Result<Self, RunnerError> {
        if line.len() > MAX_REPLY {
            return Err(RunnerError::Reply);
        }
        let line = line.strip_suffix('\n').unwrap_or(line);
        let mut words = line.splitn(4, ' ');
        match (words.next(), words.next(), words.next(), words.next()) {
            (Some("started"), Some(pid), None, None) => {
                let pid = pid.parse().map_err(|_| RunnerError::Reply)?;
                if pid == 0 {
                    return Err(RunnerError::Reply);
                }
                Ok(Reply::Started { pid })
            }
            (Some("fail"), Some(step), Some(code), message) if !step.is_empty() => {
                Ok(Reply::Failed {
                    step: step.to_owned(),
                    code: code.parse().map_err(|_| RunnerError::Reply)?,
                    message: message.unwrap_or_default().to_owned(),
                })
            }
            _ => Err(RunnerError::Reply),
        }
    }
}

impl fmt::Display for Reply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Reply::Started { pid } => write!(f, "the command started as process {pid}"),
            Reply::Failed {
                step,
                code,
                message,
            } => write!(f, "the runner failed to {step} (error {code}): {message}"),
        }
    }
}

/// Exit codes for a runner that cannot report over its pipe. Each sets bit 29
/// (the customer bit of an NTSTATUS-shaped code), so none is a code a program
/// or Windows itself returns. Once the pipe is connected, a failure is sent
/// as a [`Reply::Failed`] instead, and the owner reads that first.
pub const EXIT_ARGS: i32 = 0x2B47_0001;
pub const EXIT_PIPE: i32 = 0x2B47_0002;
pub const EXIT_FAILED: i32 = 0x2B47_0003;
pub const EXIT_WAIT: i32 = 0x2B47_0004;
pub const EXIT_SELF_PROTECT: i32 = 0x2B47_0005;

/// `STATUS_DLL_INIT_FAILED`: a DLL's start-up routine failed. For a process
/// that loads `user32.dll`, the usual cause is that it could not open its
/// window station or desktop.
pub const STATUS_DLL_INIT_FAILED: u32 = 0xC000_0142;
/// `STATUS_ACCESS_DENIED` at process start.
pub const STATUS_ACCESS_DENIED: u32 = 0xC000_0022;
/// `STATUS_DLL_NOT_FOUND` at process start.
pub const STATUS_DLL_NOT_FOUND: u32 = 0xC000_0135;
/// `STILL_ACTIVE`: what `GetExitCodeProcess` reports for a process that has
/// not ended.
pub const STILL_ACTIVE: u32 = 259;

/// What the exit code of a runner that ended before it connected to its pipe
/// says, for the error the owner reports. The code itself is always in it.
pub fn describe_runner_exit(code: u32) -> String {
    let why = match code {
        c if c == EXIT_ARGS as u32 => "the runner did not understand its arguments",
        c if c == EXIT_PIPE as u32 => {
            "the runner could not open its pipe or confirm that kGraf serves it; another \
             process of the same sandbox user may have held the pipe"
        }
        c if c == EXIT_SELF_PROTECT as u32 => {
            "the runner could not limit what the sandbox user may do to its own process, \
             threads and token, and stopped before it started the command"
        }
        c if c == EXIT_FAILED as u32 => "the runner could not start the command",
        c if c == EXIT_WAIT as u32 => "the runner lost track of the command",
        STATUS_DLL_INIT_FAILED => {
            "a DLL failed to start (STATUS_DLL_INIT_FAILED); most likely the runner could \
             not open its window station or desktop"
        }
        STATUS_ACCESS_DENIED => {
            "access was denied while the runner started (STATUS_ACCESS_DENIED); the runner \
             or a DLL it loads may not be readable by the sandbox user"
        }
        STATUS_DLL_NOT_FOUND => "a DLL the runner needs was not found (STATUS_DLL_NOT_FOUND)",
        STILL_ACTIVE => "the runner is still running",
        _ => "",
    };
    if why.is_empty() {
        format!("the runner exited with code 0x{code:08X} ({code})")
    } else {
        format!("{why} (runner exit code 0x{code:08X})")
    }
}

/// Win32 errors a logon as the sandbox user can return.
pub const ERROR_SERVICE_DISABLED: u32 = 1058;
pub const ERROR_LOGON_FAILURE: u32 = 1326;
pub const ERROR_ACCOUNT_DISABLED: u32 = 1331;
pub const ERROR_LOGON_TYPE_NOT_GRANTED: u32 = 1385;
pub const ERROR_ACCOUNT_LOCKED_OUT: u32 = 1909;

/// What to do about a failed logon as the sandbox user, for the Win32 errors
/// with a known cause.
pub fn logon_error_hint(code: u32) -> Option<&'static str> {
    Some(match code {
        ERROR_ACCOUNT_LOCKED_OUT => {
            "the sandbox user is locked out after too many failed logons. Run `kgraf sandbox \
             setup` again: it resets the sandbox users' passwords and clears the lockout. \
             Without it, the lockout clears when the account lockout policy's duration ends"
        }
        ERROR_LOGON_FAILURE => {
            "the stored password does not match the sandbox user's; run `kgraf sandbox setup` \
             again"
        }
        ERROR_ACCOUNT_DISABLED => "the sandbox user is disabled; run `kgraf sandbox setup` again",
        ERROR_LOGON_TYPE_NOT_GRANTED => {
            "a policy denies the sandbox user the interactive logon that the Secondary Logon \
             service performs; check \"Deny log on locally\" in the local or group policy"
        }
        ERROR_SERVICE_DISABLED => {
            "the Secondary Logon service (seclogon) is disabled; set its start type to Manual"
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_runner_exit_code_is_named_and_always_shown() {
        let text = describe_runner_exit(STATUS_DLL_INIT_FAILED);
        assert!(text.contains("window station"), "{text}");
        assert!(text.contains("0xC0000142"), "{text}");
        let text = describe_runner_exit(EXIT_SELF_PROTECT as u32);
        assert!(
            text.contains("limit what the sandbox user may do"),
            "{text}"
        );
        assert!(text.contains("0x2B470005"), "{text}");
        let text = describe_runner_exit(EXIT_PIPE as u32);
        assert!(text.contains("pipe"), "{text}");
        for code in [EXIT_ARGS, EXIT_FAILED, EXIT_WAIT] {
            let text = describe_runner_exit(code as u32);
            assert!(text.contains(&format!("0x{:08X}", code as u32)), "{text}");
            assert!(!text.starts_with("the runner exited with code"), "{text}");
        }
        // Unknown codes are shown as they are, in both bases.
        assert_eq!(
            describe_runner_exit(7),
            "the runner exited with code 0x00000007 (7)"
        );
    }

    #[test]
    fn a_locked_out_sandbox_user_is_named_with_the_fix() {
        let hint = logon_error_hint(ERROR_ACCOUNT_LOCKED_OUT).unwrap();
        assert!(hint.contains("locked out"), "{hint}");
        assert!(hint.contains("kgraf sandbox setup"), "{hint}");
        assert!(hint.contains("lockout policy"), "{hint}");
        for code in [
            ERROR_LOGON_FAILURE,
            ERROR_ACCOUNT_DISABLED,
            ERROR_LOGON_TYPE_NOT_GRANTED,
            ERROR_SERVICE_DISABLED,
        ] {
            assert!(logon_error_hint(code).is_some(), "{code}");
        }
        assert_eq!(logon_error_hint(5), None);
    }

    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    fn sample() -> Request {
        let mut env = w("PATH=C:\\Windows\0=C:=C:\\work\0Ünïcode=✓\0");
        env.push(0);
        Request {
            application: w(r"C:\Program Files\Git\bin\bash.exe"),
            command_line: w(r#""C:\Program Files\Git\bin\bash.exe" -c "echo hi""#),
            directory: w(r"C:\work\tree"),
            environment: env,
            desktop: w(r"kgraf-cmd-0123456789abcdef\Default"),
        }
    }

    #[test]
    fn a_request_round_trips() {
        let req = sample();
        let bytes = req.encode().unwrap();
        assert!(bytes.starts_with(REQUEST_MAGIC));
        let back = Request::read_from(&mut bytes.as_slice()).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn a_command_line_longer_than_the_logon_calls_limit_is_carried() {
        let mut req = sample();
        req.command_line = w(&format!("\"a.exe\" {}", "x".repeat(20_000)));
        let back = Request::read_from(&mut req.encode().unwrap().as_slice()).unwrap();
        assert_eq!(back.command_line.len(), req.command_line.len());
        req.command_line = vec![u16::from(b'x'); MAX_TEXT_UNITS + 1];
        assert_eq!(
            req.encode().unwrap_err(),
            RunnerError::TooLong("command line")
        );
    }

    #[test]
    fn an_empty_environment_is_two_nuls() {
        let mut req = sample();
        req.environment = vec![0, 0];
        assert!(req.encode().is_ok());
        for bad in [
            vec![],
            vec![0],
            w("A=1\0"),
            w("A=1\0\0B=2\0\0"),
            w("NOEQUALS\0\0"),
        ] {
            req.environment = bad.clone();
            assert_eq!(
                req.validate().unwrap_err(),
                RunnerError::Field("environment"),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn the_commands_desktop_travels_in_the_request_and_must_name_a_station_and_a_desktop() {
        let req = sample();
        let back = Request::read_from(&mut req.encode().unwrap().as_slice()).unwrap();
        assert_eq!(
            String::from_utf16(&back.desktop).unwrap(),
            r"kgraf-cmd-0123456789abcdef\Default"
        );
        // Empty is the window station Windows makes for the runner's own
        // logon session, which the session-0 path asks for.
        let mut empty = sample();
        empty.desktop = Vec::new();
        assert_eq!(
            Request::read_from(&mut empty.encode().unwrap().as_slice())
                .unwrap()
                .desktop,
            Vec::<u16>::new()
        );
        for bad in [
            "Default",
            r"\Default",
            r"kgraf-cmd-1\",
            r"a\b\c",
            "kgraf-cmd-1\0\\Default",
            "kgraf-cmd-1\\Def\0ault",
        ] {
            let mut req = sample();
            req.desktop = w(bad);
            assert_eq!(
                req.encode().unwrap_err(),
                RunnerError::Field("desktop"),
                "{bad:?}"
            );
        }
        let mut req = sample();
        req.desktop = w(&format!(r"{}\Default", "s".repeat(MAX_DESKTOP_UNITS)));
        assert_eq!(req.encode().unwrap_err(), RunnerError::TooLong("desktop"));
        // A frame cut before the desktop field is refused, not read short.
        let good = sample().encode().unwrap();
        let cut = good.len() - sample().desktop.len() * 2 - 4;
        assert_eq!(
            Request::read_from(&mut &good[..cut]).unwrap_err(),
            RunnerError::Truncated
        );
    }

    #[test]
    fn a_nul_or_an_empty_path_is_refused() {
        let mut req = sample();
        req.application = w("C:\\a\0b.exe");
        assert_eq!(
            req.validate().unwrap_err(),
            RunnerError::Field("application")
        );
        let mut req = sample();
        req.directory.clear();
        assert_eq!(req.validate().unwrap_err(), RunnerError::Field("directory"));
    }

    #[test]
    fn a_damaged_or_oversized_frame_is_refused_before_it_is_read() {
        let good = sample().encode().unwrap();
        assert_eq!(
            Request::read_from(&mut &b"NOTKGRAF\x01"[..]).unwrap_err(),
            RunnerError::Magic
        );
        // A runner from before the desktop field reads version 1 only, and
        // this one refuses it: neither side acts on the other's frame.
        let mut v1 = good.clone();
        v1[8] = 1;
        assert_eq!(
            Request::read_from(&mut v1.as_slice()).unwrap_err(),
            RunnerError::Version(1)
        );
        for cut in [3, 9, 12, good.len() - 1] {
            assert_eq!(
                Request::read_from(&mut &good[..cut]).unwrap_err(),
                RunnerError::Truncated,
                "cut at {cut}"
            );
        }
        // A length of 4 GiB units: refused from the length alone, with no
        // allocation of that size.
        let mut huge = good[..9].to_vec();
        huge.extend_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(
            Request::read_from(&mut huge.as_slice()).unwrap_err(),
            RunnerError::TooLong("application")
        );
        // A NUL inside a field read from the pipe.
        let mut req = sample();
        req.directory = w("C:\\x");
        let mut bytes = req.encode().unwrap();
        let at = bytes.len() - req.desktop.len() * 2 - 4 - req.environment.len() * 2 - 4 - 2;
        bytes[at] = 0;
        bytes[at + 1] = 0;
        assert_eq!(
            Request::read_from(&mut bytes.as_slice()).unwrap_err(),
            RunnerError::Field("directory")
        );
    }

    #[test]
    fn runner_args_round_trip_and_refuse_anything_else() {
        let args = RunnerArgs::fresh(4242).unwrap();
        assert!(args.pipe.starts_with(PIPE_PREFIX));
        assert_eq!(RunnerArgs::parse(args.to_args()).unwrap(), args);
        let other = RunnerArgs::fresh(4242).unwrap();
        assert_ne!(args.pipe, other.pipe, "each run has its own pipe name");

        let pipe = args.pipe.as_str();
        for bad in [
            vec![],
            vec!["--pipe", pipe],
            vec!["--owner-pid", "7"],
            vec!["--pipe", pipe, "--owner-pid", "0"],
            vec!["--pipe", pipe, "--owner-pid", "x"],
            vec!["--pipe", pipe, "--owner-pid", "7", "--pipe", pipe],
            vec!["--pipe", r"\\.\pipe\other", "--owner-pid", "7"],
            vec![
                "--pipe",
                r"\\.\pipe\kgraf-sandbox-run-XYZ",
                "--owner-pid",
                "7",
            ],
            vec!["--pipe", pipe, "--owner-pid", "7", "--extra", "1"],
            vec!["--pipe", pipe, "--owner-pid"],
            vec!["--pipe", pipe, "--owner-pid", "7", "--logon-sid", "maybe"],
            vec![
                "--pipe",
                pipe,
                "--owner-pid",
                "7",
                "--logon-sid",
                "keep",
                "--logon-sid",
                "deny",
            ],
        ] {
            assert!(RunnerArgs::parse(&bad).is_err(), "{bad:?}");
        }
    }

    /// The owner's side says what the runner does with the logon SID, and a
    /// command line that does not say asks for the deny-only default: a
    /// launch that loses the option must never leave the SID enabled.
    #[test]
    fn the_logon_sid_policy_travels_and_defaults_to_deny_only() {
        let args = RunnerArgs::fresh(7).unwrap();
        assert_eq!(args.logon_sid, LogonSidPolicy::Deny);
        for policy in [LogonSidPolicy::Deny, LogonSidPolicy::Keep] {
            let asked = args.clone().with_logon_sid(policy);
            assert_eq!(RunnerArgs::parse(asked.to_args()).unwrap(), asked);
            assert_eq!(LogonSidPolicy::from_key(policy.key()), Some(policy));
        }
        let without = ["--pipe", args.pipe.as_str(), "--owner-pid", "7"];
        assert_eq!(
            RunnerArgs::parse(without).unwrap().logon_sid,
            LogonSidPolicy::Deny
        );
        assert_eq!(LogonSidPolicy::from_key("Keep"), None);
    }

    #[test]
    fn replies_round_trip_as_one_line() {
        let started = Reply::Started { pid: 912 };
        assert_eq!(started.encode(), "started 912\n");
        assert_eq!(Reply::parse(&started.encode()).unwrap(), started);

        let failed = Reply::Failed {
            step: "start the command".into(),
            code: 5,
            message: "Access is denied.\r\nsecond line".into(),
        };
        let line = failed.encode();
        assert_eq!(line.matches('\n').count(), 1);
        assert_eq!(
            Reply::parse(&line).unwrap(),
            Reply::Failed {
                step: "start-the-command".into(),
                code: 5,
                message: "Access is denied.  second line".into(),
            }
        );
        let long = Reply::Failed {
            step: String::new(),
            code: 0,
            message: "é".repeat(MAX_REPLY),
        };
        let line = long.encode();
        assert!(line.len() <= MAX_REPLY);
        assert!(Reply::parse(&line).is_ok());

        for bad in [
            "",
            "started",
            "started 0",
            "started x",
            "fail",
            "fail x y",
            "end",
        ] {
            assert!(Reply::parse(bad).is_err(), "{bad:?}");
        }
    }
}

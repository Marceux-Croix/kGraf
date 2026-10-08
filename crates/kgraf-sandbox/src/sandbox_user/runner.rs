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
//!     create pipe (first instance, DACL:
//!       owner and sandbox user)
//!     CreateProcessWithLogonW(runner --pipe <name> --owner-pid <pid>)
//!                                           connect; server PID == owner PID?
//!     client PID == runner PID?
//!     --- request (binary, below) --------->
//!                                           start the command
//!     <-------------- "started <pid>\n" ---  or "fail <step> <code> <text>\n"
//!                                           wait; exit with the command's code
//! ```
//!
//! The request is one binary frame, little-endian:
//!
//! ```text
//! "KGRUNREQ" | version u8 = 1
//! 4 fields, each: length u32 (UTF-16 units) | UTF-16LE units
//!   application   full path of the program, no NUL
//!   command line  argv[0] and the arguments, quoted, no NUL
//!   directory     the working directory, no NUL
//!   environment   a CreateProcess environment block: NAME=value\0 ...\0
//! ```
//!
//! Everything here is platform-neutral, so the format is tested on every
//! host; the Windows calls are in `windows::runner` and `windows::launch`.

use std::fmt;
use std::io::Read;

use super::handshake::{is_nonce, new_nonce};

pub const REQUEST_MAGIC: &[u8; 8] = b"KGRUNREQ";
pub const REQUEST_VERSION: u8 = 1;

/// The longest path, command line or working directory a Windows process
/// call accepts, in UTF-16 units (`CreateProcessW`'s command line limit).
pub const MAX_TEXT_UNITS: usize = 32_767;

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
}

impl Request {
    /// Checks every field, so neither end sends or acts on a request the
    /// other would refuse.
    pub fn validate(&self) -> Result<(), RunnerError> {
        text("application", &self.application)?;
        text("command line", &self.command_line)?;
        text("directory", &self.directory)?;
        env_block(&self.environment)
    }

    pub fn encode(&self) -> Result<Vec<u8>, RunnerError> {
        self.validate()?;
        let units = self.application.len()
            + self.command_line.len()
            + self.directory.len()
            + self.environment.len();
        let mut out = Vec::with_capacity(9 + 16 + units * 2);
        out.extend_from_slice(REQUEST_MAGIC);
        out.push(REQUEST_VERSION);
        for field in [
            &self.application,
            &self.command_line,
            &self.directory,
            &self.environment,
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
}

const PIPE_PREFIX: &str = r"\\.\pipe\kgraf-sandbox-run-";

impl RunnerArgs {
    /// Arguments for a fresh run: a pipe name from the system random source.
    pub fn fresh(owner_pid: u32) -> Result<Self, RunnerError> {
        let nonce = new_nonce().map_err(|e| RunnerError::Args(e.to_string()))?;
        Ok(Self {
            pipe: format!("{PIPE_PREFIX}{nonce}"),
            owner_pid,
        })
    }

    /// The arguments after the program name. None of them needs quoting.
    pub fn to_args(&self) -> [String; 4] {
        [
            "--pipe".to_owned(),
            self.pipe.clone(),
            "--owner-pid".to_owned(),
            self.owner_pid.to_string(),
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
                other => return Err(bad(&format!("unexpected argument {other:?}"))),
            }
        }
        Ok(Self {
            pipe: pipe.ok_or_else(|| bad("--pipe is missing"))?,
            owner_pid: owner_pid.ok_or_else(|| bad("--owner-pid is missing"))?,
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let mut v2 = good.clone();
        v2[8] = 2;
        assert_eq!(
            Request::read_from(&mut v2.as_slice()).unwrap_err(),
            RunnerError::Version(2)
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
        let at = bytes.len() - req.environment.len() * 2 - 4 - 2;
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
        ] {
            assert!(RunnerArgs::parse(&bad).is_err(), "{bad:?}");
        }
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

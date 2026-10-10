//! What the launcher service is started with, who its pipe admits, which
//! handles it accepts, and what the runner's logon SID must look like
//! (decision 0097).
//!
//! The service control manager starts the service with the command line an
//! administrator installed, and only an administrator can change it. So the
//! three values the service trusts for its whole life travel there: the
//! owner's SID, the pipe's name and the full path of the runner the service
//! may start. Everything else comes over the pipe and is checked against
//! these.
//!
//! Everything in this file is platform-neutral, so the parse, the pipe's
//! access list, the handle types and the logon-SID rule are tested on every
//! host. The Windows calls that use them are in `windows::service_main` and
//! `windows::runner`.

use std::fmt;

use super::names::is_sid_string;
use super::runner::LogonSidPolicy;

/// The service's arguments are refused with the reason, and the service does
/// not start.
#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
#[error("the launcher service's arguments are malformed: {0}")]
pub struct ArgsError(pub String);

/// The longest pipe name accepted, in characters. A pipe's name is one path
/// element below `\\.\pipe\`, and Windows limits that element to 256
/// characters.
pub const MAX_PIPE_NAME: usize = 256;

/// The start of every pipe path on the local machine.
pub const PIPE_ROOT: &str = r"\\.\pipe\";

/// What the service control manager starts the service with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceArgs {
    /// The one user whose requests the service answers. Checked against the
    /// user SID of the pipe client's own token, so a second account on the
    /// machine cannot ask the service for anything.
    pub owner_sid: String,
    /// The pipe's name below `\\.\pipe\`, without that prefix.
    pub pipe: String,
    /// The full path of the runner the service may start. The service refuses
    /// a request that names any other program, so a request can choose the
    /// slot user and the command line but never the program.
    pub runner: String,
}

impl ServiceArgs {
    /// The arguments after the subcommand, in the order the installer writes
    /// them. None of them needs quoting but the runner's path, which the
    /// installer quotes.
    /// The service's whole command line after the program: the subcommand
    /// kGraf's program dispatches on ([`super::session0::SERVICE_SUBCOMMAND`]),
    /// then [`Self::to_args`]. The installed image path is built from this,
    /// so the program the service manager starts enters the service at once.
    pub fn command_line(&self) -> Vec<String> {
        let mut out = vec![super::session0::SERVICE_SUBCOMMAND.to_owned()];
        out.extend(self.to_args());
        out
    }

    /// The service's arguments when `argv` (a program's arguments, without
    /// the program name) starts with the service subcommand; `None` for any
    /// other command line, which is not the service's.
    pub fn from_command_line<S: AsRef<str>>(argv: &[S]) -> Option<Result<Self, ArgsError>> {
        let (first, rest) = argv.split_first()?;
        (first.as_ref() == super::session0::SERVICE_SUBCOMMAND)
            .then(|| Self::parse(rest.iter().map(|s| s.as_ref())))
    }

    pub fn to_args(&self) -> [String; 6] {
        [
            "--owner-sid".to_owned(),
            self.owner_sid.clone(),
            "--pipe".to_owned(),
            self.pipe.clone(),
            "--runner".to_owned(),
            self.runner.clone(),
        ]
    }

    /// Parses the arguments after the subcommand. Exactly the three options,
    /// once each: the service acts on nothing else from its command line.
    /// Each value is checked here, so the service never runs with a SID that
    /// admits nobody, a pipe name Windows cannot make, or a runner path that
    /// a current directory would resolve.
    pub fn parse<I, S>(args: I) -> Result<Self, ArgsError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let bad = |why: &str| ArgsError(why.to_owned());
        let mut owner_sid = None;
        let mut pipe = None;
        let mut runner = None;
        let mut it = args.into_iter();
        while let Some(flag) = it.next() {
            let value = it.next().ok_or_else(|| bad("an option has no value"))?;
            let value = value.as_ref();
            match flag.as_ref() {
                "--owner-sid" if owner_sid.is_none() => {
                    if !is_sid_string(value) {
                        return Err(bad("the owner SID is not a SID"));
                    }
                    owner_sid = Some(value.to_owned());
                }
                "--pipe" if pipe.is_none() => {
                    check_pipe_name(value)?;
                    pipe = Some(value.to_owned());
                }
                "--runner" if runner.is_none() => {
                    if !is_absolute_windows_path(value) {
                        return Err(bad("the runner path is not absolute"));
                    }
                    if value.contains('\0') {
                        return Err(bad("the runner path holds a NUL"));
                    }
                    runner = Some(value.to_owned());
                }
                other => return Err(bad(&format!("unexpected argument {other:?}"))),
            }
        }
        Ok(Self {
            owner_sid: owner_sid.ok_or_else(|| bad("--owner-sid is missing"))?,
            pipe: pipe.ok_or_else(|| bad("--pipe is missing"))?,
            runner: runner.ok_or_else(|| bad("--runner is missing"))?,
        })
    }

    /// The pipe's full path, which both ends open.
    pub fn pipe_path(&self) -> String {
        format!("{PIPE_ROOT}{}", self.pipe)
    }

    /// The pipe's access list in SDDL: SYSTEM, which the service itself runs
    /// as, and the owner alone.
    pub fn pipe_sddl(&self) -> String {
        pipe_sddl(&self.owner_sid)
    }
}

impl fmt::Display for ServiceArgs {
    /// For a status line: the SID and the pipe, never a path.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the launcher service answers {} on {}",
            self.owner_sid,
            self.pipe_path()
        )
    }
}

/// A pipe name: one path element below `\\.\pipe\`, not empty, within
/// [`MAX_PIPE_NAME`], with no backslash (which would name another directory
/// of the pipe namespace) and no NUL (which a Windows call reads as the end
/// of the string).
fn check_pipe_name(name: &str) -> Result<(), ArgsError> {
    let bad = |why: &str| ArgsError(why.to_owned());
    if name.is_empty() {
        return Err(bad("the pipe name is empty"));
    }
    if name.chars().count() > MAX_PIPE_NAME {
        return Err(bad("the pipe name is too long"));
    }
    if name.contains('\\') || name.contains('/') {
        return Err(bad("the pipe name holds a path separator"));
    }
    if name.contains('\0') {
        return Err(bad("the pipe name holds a NUL"));
    }
    Ok(())
}

/// Whether `path` is an absolute Windows path: a drive letter and a
/// separator (`C:\...`), or a UNC path (`\\server\share\...`). The rule is
/// Windows' own and is applied on every host, because the path is a Windows
/// path wherever this is tested. A relative path is refused: the service
/// control manager's working directory would decide what the service runs.
fn is_absolute_windows_path(path: &str) -> bool {
    let b = path.as_bytes();
    let unc = matches!(b, [b'\\' | b'/', b'\\' | b'/', rest @ ..] if !rest.is_empty());
    let drive = matches!(
        b,
        [letter, b':', b'\\' | b'/', ..] if letter.is_ascii_alphabetic()
    );
    unc || drive
}

// ---- the pipe's access list -------------------------------------------------

/// `FILE_CREATE_PIPE_INSTANCE`, which is also `FILE_APPEND_DATA`: the right
/// to create another instance of a pipe that already exists. It must reach
/// nobody but SYSTEM. `FILE_GENERIC_WRITE` carries it, so the owner is
/// granted the exact rights below instead of a generic right, and the client
/// opens the pipe with [`PIPE_CLIENT_MASK`] rather than `GENERIC_WRITE`,
/// which would ask for a right the list does not grant and be refused.
pub const FILE_CREATE_PIPE_INSTANCE: u32 = 0x0004;

const FILE_READ_DATA: u32 = 0x0001;
const FILE_WRITE_DATA: u32 = 0x0002;
const FILE_READ_ATTRIBUTES: u32 = 0x0080;
const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;
const READ_CONTROL: u32 = 0x0002_0000;
const SYNCHRONIZE: u32 = 0x0010_0000;

/// What the owner may do to the service's pipe: connect and wait
/// (`SYNCHRONIZE`), send the request (`FILE_WRITE_DATA`), read the reply
/// (`FILE_READ_DATA`), read and write the pipe's own attributes, and read the
/// access list (`READ_CONTROL`), so the owner's side can say who the pipe
/// admits. Never [`FILE_CREATE_PIPE_INSTANCE`]: with it, a program running as
/// the owner could add an instance of the pipe's name and answer in the
/// service's place.
pub const PIPE_OWNER_MASK: u32 = FILE_READ_DATA
    | FILE_WRITE_DATA
    | FILE_READ_ATTRIBUTES
    | FILE_WRITE_ATTRIBUTES
    | READ_CONTROL
    | SYNCHRONIZE;

/// What the owner's side asks for when it opens the pipe. A subset of
/// [`PIPE_OWNER_MASK`], named here so the two cannot drift apart.
pub const PIPE_CLIENT_MASK: u32 =
    FILE_READ_DATA | FILE_WRITE_DATA | FILE_READ_ATTRIBUTES | SYNCHRONIZE;

/// The pipe's access list: SYSTEM fully, the owner `owner_sid` to
/// [`PIPE_OWNER_MASK`], nobody else. `P`: inherit nothing from anywhere.
///
/// `owner_sid` is checked as a SID string before it gets here
/// ([`ServiceArgs::parse`]), so it holds no SDDL syntax character.
pub fn pipe_sddl(owner_sid: &str) -> String {
    format!("D:P(A;;GA;;;SY)(A;;0x{PIPE_OWNER_MASK:08x};;;{owner_sid})")
}

// ---- the handles the service accepts ----------------------------------------

/// The object type names the service accepts for a handle it duplicates out
/// of the owner's process: the owner's standard input, output and error, and
/// the owner's job. A pipe and a file are both `File` objects in the object
/// manager, so one name covers both.
///
/// The check exists because the service puts nothing in the sandbox but the
/// owner's own standard handles and job. Without it a request could hand over
/// a handle to any object the owner's process holds, a process, a thread, a
/// token or a section among them, and the command would start with it open.
pub const ALLOWED_HANDLE_TYPES: [&str; 2] = ["File", "Job"];

/// Whether `type_name`, as the object manager reports it, is one the service
/// accepts. The comparison ignores ASCII case: the names come from Windows
/// itself and are stable, and a case difference is not a reason to refuse.
pub fn handle_type_allowed(type_name: &str) -> bool {
    ALLOWED_HANDLE_TYPES
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(type_name))
}

// ---- the owner's job --------------------------------------------------------

/// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`: when the last handle to the job is
/// closed, every process in it is ended.
pub const JOB_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x0000_2000;
/// `JOB_OBJECT_LIMIT_BREAKAWAY_OK`: a child may leave the job by asking for
/// `CREATE_BREAKAWAY_FROM_JOB`.
pub const JOB_LIMIT_BREAKAWAY_OK: u32 = 0x0000_0800;
/// `JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK`: a child leaves the job without
/// asking for anything.
pub const JOB_LIMIT_SILENT_BREAKAWAY_OK: u32 = 0x0000_1000;

/// What the job named in a request must limit. The job is the one thing that
/// ends a sandboxed command when the side that started it goes away, so a job
/// that does not end its processes on close is not a job the launcher will put
/// a command in.
pub const REQUIRED_JOB_LIMITS: u32 = JOB_LIMIT_KILL_ON_JOB_CLOSE;

/// What the job named in a request must not allow. Either breakaway flag lets
/// a process leave the job, and a process outside the job answers no kill, no
/// timeout and no limit.
pub const FORBIDDEN_JOB_LIMITS: u32 = JOB_LIMIT_BREAKAWAY_OK | JOB_LIMIT_SILENT_BREAKAWAY_OK;

/// Whether a job whose extended limits are `limit_flags` may hold a sandboxed
/// command. The object type alone does not answer that: every job object is a
/// `Job`, and a job without [`REQUIRED_JOB_LIMITS`] or with one of
/// [`FORBIDDEN_JOB_LIMITS`] gives exactly what the launcher exists to prevent,
/// a command that outlives the side that asked for it.
///
/// The rule is platform-neutral and tested on every host; the Windows call
/// that reads `limit_flags` is `QueryInformationJobObject` in
/// `windows::service_main`.
pub fn job_limits_allowed(limit_flags: u32) -> Result<(), &'static str> {
    if limit_flags & JOB_LIMIT_BREAKAWAY_OK != 0 {
        return Err("the job lets a process break away from it (JOB_OBJECT_LIMIT_BREAKAWAY_OK)");
    }
    if limit_flags & JOB_LIMIT_SILENT_BREAKAWAY_OK != 0 {
        return Err("the job lets a process break away from it without asking \
             (JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK)");
    }
    if limit_flags & JOB_LIMIT_KILL_ON_JOB_CLOSE == 0 {
        return Err("the job does not end its processes when it is closed \
             (JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE is not set)");
    }
    Ok(())
}

// ---- the runner the service may start ---------------------------------------

/// Whether `asked` names the same program as `installed`, the runner path the
/// administrator installed the service with.
///
/// The two are compared as Windows compares a file name: ignoring case. Only
/// ASCII case is folded here. A name whose only difference is the case of a
/// character outside ASCII is refused, because the case tables Windows uses
/// for the rest are the file system's, not this code's, and a refusal is the
/// safe answer: the owner sends the path the installer recorded.
pub fn same_runner_path(installed: &[u16], asked: &[u16]) -> bool {
    installed.len() == asked.len()
        && installed
            .iter()
            .zip(asked)
            .all(|(a, b)| fold_ascii(*a) == fold_ascii(*b))
}

/// One UTF-16 code unit with ASCII lowercase folded to uppercase.
fn fold_ascii(unit: u16) -> u16 {
    match u8::try_from(unit) {
        Ok(b) => u16::from(b.to_ascii_uppercase()),
        Err(_) => unit,
    }
}

// ---- the command token's logon SID ------------------------------------------

/// Whether the logon SIDs of the token the runner built are what the launch
/// path asked for. `deny_only` holds one entry per logon SID in that token:
/// `true` where the SID is deny-only.
///
/// Either policy fails loud, so a token that is not what was asked for never
/// starts a command: on the owner-session path an enabled logon SID would
/// admit the command to the objects the owner's processes make, and on the
/// session-0 path a deny-only one would shut the command out of the object
/// directory that path exists to reach. A token with no logon SID at all
/// fails for both: the SID was lost, not changed.
pub fn logon_sid_check(policy: LogonSidPolicy, deny_only: &[bool]) -> Result<(), &'static str> {
    if deny_only.is_empty() {
        return Err("the command token has no logon SID");
    }
    match policy {
        LogonSidPolicy::Deny if !deny_only.iter().all(|d| *d) => {
            Err("the logon SID is still enabled in the command token")
        }
        LogonSidPolicy::Keep if deny_only.iter().any(|d| *d) => Err(
            "the logon SID is deny-only in the command token, and this launch path needs \
                 it enabled",
        ),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SID: &str = "S-1-5-21-1004336348-1177238915-682003330-1001";
    const RUNNER: &str = r"C:\Program Files\kGraf\kgraf-sandbox-runner.exe";

    fn args() -> ServiceArgs {
        ServiceArgs {
            owner_sid: SID.to_owned(),
            pipe: "kgraf-launcher-00ff".to_owned(),
            runner: RUNNER.to_owned(),
        }
    }

    /// The image path setup installs starts with the subcommand the program
    /// dispatches on, and the program reads it back. Without the subcommand
    /// the service manager started the program with bare flags, the program
    /// refused them as an unknown command, and the service never reported to
    /// the manager (error 1053).
    #[test]
    fn the_installed_command_line_enters_the_service_and_reads_back() {
        let built = ServiceArgs {
            owner_sid: "S-1-5-21-1-2-3-1001".to_owned(),
            pipe: "kgraf-launcher-1a2b3c4d".to_owned(),
            runner: r"C:\Program Files\kGraf\kgraf-sandbox-runner.exe".to_owned(),
        };
        let line = built.command_line();
        assert_eq!(line[0], super::super::session0::SERVICE_SUBCOMMAND);
        assert_eq!(
            ServiceArgs::from_command_line(&line)
                .expect("the service's line")
                .unwrap(),
            built
        );
        assert!(ServiceArgs::from_command_line(&built.to_args()).is_none());
        assert!(ServiceArgs::from_command_line::<&str>(&[]).is_none());
        assert!(ServiceArgs::from_command_line(&["sandbox", "status"]).is_none());
    }

    #[test]
    fn the_service_arguments_round_trip() {
        let built = args();
        assert_eq!(ServiceArgs::parse(built.to_args()).unwrap(), built);
        assert_eq!(
            built.pipe_path(),
            r"\\.\pipe\kgraf-launcher-00ff",
            "the pipe's full path"
        );
    }

    /// Exactly the three options, once each, with a value each, and nothing
    /// else: an administrator installs this command line, and the service
    /// acts on no other word of it.
    #[test]
    fn anything_but_the_three_options_is_refused() {
        for bad in [
            vec![],
            vec!["--owner-sid", SID],
            vec!["--owner-sid", SID, "--pipe", "p"],
            vec!["--pipe", "p", "--runner", RUNNER],
            vec!["--owner-sid", SID, "--runner", RUNNER],
            vec!["--owner-sid", SID, "--pipe", "p", "--runner"],
            vec![
                "--owner-sid",
                SID,
                "--pipe",
                "p",
                "--runner",
                RUNNER,
                "--extra",
                "1",
            ],
            vec![
                "--owner-sid",
                SID,
                "--pipe",
                "p",
                "--runner",
                RUNNER,
                "--pipe",
                "q",
            ],
            vec![
                "--owner-sid",
                SID,
                "--owner-sid",
                SID,
                "--pipe",
                "p",
                "--runner",
                RUNNER,
            ],
        ] {
            assert!(ServiceArgs::parse(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_value_that_is_not_what_it_names_is_refused() {
        let cases: [(&str, &str); 8] = [
            ("--owner-sid", "not-a-sid"),
            ("--owner-sid", ""),
            ("--owner-sid", "S-1-5-21-1-2-3-1001; drop"),
            ("--pipe", ""),
            ("--pipe", r"sub\name"),
            ("--pipe", "name/with/slash"),
            ("--runner", r"kgraf-sandbox-runner.exe"),
            ("--runner", r"\kgraf\runner.exe"),
        ];
        for (flag, value) in cases {
            let mut argv = args().to_args().to_vec();
            for i in (0..argv.len()).step_by(2) {
                if argv[i] == flag {
                    argv[i + 1] = value.to_owned();
                }
            }
            assert!(ServiceArgs::parse(&argv).is_err(), "{flag} {value:?}");
        }
        let mut long = args();
        long.pipe = "p".repeat(MAX_PIPE_NAME + 1);
        assert!(ServiceArgs::parse(long.to_args()).is_err());
    }

    #[test]
    fn an_absolute_windows_path_is_a_drive_or_a_unc_path() {
        for good in [
            r"C:\x\runner.exe",
            r"c:/x/runner.exe",
            r"\\host\share\runner.exe",
            r"//host/share/runner.exe",
        ] {
            assert!(is_absolute_windows_path(good), "{good}");
        }
        for bad in [
            "",
            "runner.exe",
            r"x\runner.exe",
            r"\runner.exe",
            "C:runner.exe",
            r"1:\runner.exe",
            r"\\",
        ] {
            assert!(!is_absolute_windows_path(bad), "{bad}");
        }
    }

    /// The owner's entry must never carry `FILE_CREATE_PIPE_INSTANCE`: with
    /// it, a program running as the owner could create another instance of
    /// the pipe's name and answer in the service's place.
    #[test]
    fn the_pipes_list_admits_system_and_the_owner_without_the_right_to_add_an_instance() {
        assert_eq!(
            PIPE_OWNER_MASK & FILE_CREATE_PIPE_INSTANCE,
            0,
            "the owner's mask carries FILE_CREATE_PIPE_INSTANCE (0x{FILE_CREATE_PIPE_INSTANCE:04x})"
        );
        assert_eq!(PIPE_CLIENT_MASK & FILE_CREATE_PIPE_INSTANCE, 0);
        assert_eq!(
            PIPE_CLIENT_MASK & !PIPE_OWNER_MASK,
            0,
            "the client asks for a right the list does not grant"
        );
        // Connect, read, write and read the list: what a client needs and no
        // more.
        assert_eq!(PIPE_OWNER_MASK & SYNCHRONIZE, SYNCHRONIZE);
        assert_eq!(PIPE_OWNER_MASK & FILE_READ_DATA, FILE_READ_DATA);
        assert_eq!(PIPE_OWNER_MASK & FILE_WRITE_DATA, FILE_WRITE_DATA);
        assert_eq!(PIPE_OWNER_MASK & READ_CONTROL, READ_CONTROL);

        let sddl = args().pipe_sddl();
        assert_eq!(sddl, format!("D:P(A;;GA;;;SY)(A;;0x00120183;;;{SID})"));
        assert!(
            sddl.starts_with("D:P("),
            "the list inherits nothing: {sddl}"
        );
        assert_eq!(sddl.matches("(A;;").count(), 2, "two entries only: {sddl}");
    }

    /// The service puts nothing in the sandbox but the owner's own standard
    /// handles and job, so a handle whose object is anything else is refused.
    #[test]
    fn only_a_pipe_a_file_or_a_job_is_accepted_as_a_handle() {
        for allowed in ["File", "file", "Job", "JOB"] {
            assert!(handle_type_allowed(allowed), "{allowed}");
        }
        for refused in [
            "Process",
            "Thread",
            "Token",
            "Section",
            "Event",
            "Semaphore",
            "Mutant",
            "Directory",
            "SymbolicLink",
            "Key",
            "WindowStation",
            "Desktop",
            "",
            "FileX",
        ] {
            assert!(!handle_type_allowed(refused), "{refused}");
        }
        assert_eq!(ALLOWED_HANDLE_TYPES, ["File", "Job"]);
    }

    /// A job is accepted on its limits, never on its object type: a job that
    /// does not end its processes on close, or that allows a breakaway, would
    /// leave a command able to outlive the side that asked for it.
    #[test]
    fn a_job_without_kill_on_close_or_with_a_breakaway_is_refused() {
        // The limits kGraf's own job carries, and the same with other flags
        // beside them: accepted.
        const DIE_ON_UNHANDLED_EXCEPTION: u32 = 0x0000_0400;
        assert_eq!(job_limits_allowed(JOB_LIMIT_KILL_ON_JOB_CLOSE), Ok(()));
        assert_eq!(
            job_limits_allowed(JOB_LIMIT_KILL_ON_JOB_CLOSE | DIE_ON_UNHANDLED_EXCEPTION),
            Ok(())
        );

        assert_eq!(
            job_limits_allowed(0),
            Err("the job does not end its processes when it is closed \
                 (JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE is not set)")
        );
        assert_eq!(
            job_limits_allowed(DIE_ON_UNHANDLED_EXCEPTION),
            Err("the job does not end its processes when it is closed \
                 (JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE is not set)")
        );
        assert_eq!(
            job_limits_allowed(JOB_LIMIT_KILL_ON_JOB_CLOSE | JOB_LIMIT_BREAKAWAY_OK),
            Err("the job lets a process break away from it (JOB_OBJECT_LIMIT_BREAKAWAY_OK)")
        );
        assert_eq!(
            job_limits_allowed(JOB_LIMIT_KILL_ON_JOB_CLOSE | JOB_LIMIT_SILENT_BREAKAWAY_OK),
            Err("the job lets a process break away from it without asking \
                 (JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK)")
        );
        // Every flag set: still refused, because a breakaway is allowed.
        assert!(job_limits_allowed(u32::MAX).is_err());

        // The two sets name different flags, so a job can satisfy both.
        assert_eq!(REQUIRED_JOB_LIMITS & FORBIDDEN_JOB_LIMITS, 0);
        assert_eq!(REQUIRED_JOB_LIMITS, JOB_LIMIT_KILL_ON_JOB_CLOSE);
        assert_eq!(
            FORBIDDEN_JOB_LIMITS,
            JOB_LIMIT_BREAKAWAY_OK | JOB_LIMIT_SILENT_BREAKAWAY_OK
        );
        assert_eq!(job_limits_allowed(REQUIRED_JOB_LIMITS), Ok(()));
        for forbidden in [JOB_LIMIT_BREAKAWAY_OK, JOB_LIMIT_SILENT_BREAKAWAY_OK] {
            assert!(
                job_limits_allowed(REQUIRED_JOB_LIMITS | forbidden).is_err(),
                "0x{forbidden:04x}"
            );
        }
    }

    #[test]
    fn the_runner_path_is_compared_as_windows_compares_a_file_name() {
        let w = |s: &str| s.encode_utf16().collect::<Vec<u16>>();
        let installed = w(RUNNER);
        assert!(same_runner_path(&installed, &installed));
        assert!(same_runner_path(
            &installed,
            &w(r"c:\program files\kgraf\KGRAF-SANDBOX-RUNNER.EXE")
        ));
        for other in [
            r"C:\Program Files\kGraf\kgraf-sandbox-runner.exe.exe",
            r"C:\Program Files\kGraf\other.exe",
            r"C:\Program Files\kGraf\kgraf-sandbox-runner.ex",
            r"D:\Program Files\kGraf\kgraf-sandbox-runner.exe",
            "",
        ] {
            assert!(!same_runner_path(&installed, &w(other)), "{other}");
        }
        // Case outside ASCII is not folded, so it is refused.
        assert!(!same_runner_path(&w(r"C:\Ü\r.exe"), &w(r"C:\ü\r.exe")));
    }

    /// Both policies fail loud, each on the opposite token.
    #[test]
    fn the_logon_sid_rule_refuses_the_token_the_other_path_needs() {
        assert_eq!(logon_sid_check(LogonSidPolicy::Deny, &[true]), Ok(()));
        assert_eq!(logon_sid_check(LogonSidPolicy::Deny, &[true, true]), Ok(()));
        assert_eq!(logon_sid_check(LogonSidPolicy::Keep, &[false]), Ok(()));

        assert_eq!(
            logon_sid_check(LogonSidPolicy::Deny, &[true, false]),
            Err("the logon SID is still enabled in the command token")
        );
        assert!(logon_sid_check(LogonSidPolicy::Keep, &[true]).is_err());
        assert!(logon_sid_check(LogonSidPolicy::Keep, &[false, true]).is_err());
        for policy in [LogonSidPolicy::Deny, LogonSidPolicy::Keep] {
            assert_eq!(
                logon_sid_check(policy, &[]),
                Err("the command token has no logon SID"),
                "{policy:?}"
            );
        }
    }
}

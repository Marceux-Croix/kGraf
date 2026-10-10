//! The launcher service's access list and image path, as the text the
//! service control manager takes.
//!
//! Both are strings, and a wrong string is a boundary defect rather than a
//! build failure: an access list that grants one right too many hands the
//! owner's account control of a service that runs as LocalSystem, and an
//! image path that the service control manager parses differently from what
//! setup meant starts another program. So both are built here, where every
//! host runs the tests, and the Windows calls take the result unchanged
//! (`windows::service`).

use std::path::Path;

use super::names::is_sid_string;

/// Service access rights, by their documented values. The Windows headers
/// hold the same numbers; `windows::service` checks each one against the
/// `windows-sys` constant when it compiles, so these cannot drift.
pub const SERVICE_QUERY_CONFIG: u32 = 0x0000_0001;
pub const SERVICE_CHANGE_CONFIG: u32 = 0x0000_0002;
pub const SERVICE_QUERY_STATUS: u32 = 0x0000_0004;
pub const SERVICE_START: u32 = 0x0000_0010;
pub const SERVICE_STOP: u32 = 0x0000_0020;
/// Standard rights, which every securable Windows object shares.
pub const DELETE: u32 = 0x0001_0000;
pub const READ_CONTROL: u32 = 0x0002_0000;
pub const WRITE_DAC: u32 = 0x0004_0000;

/// What the owner's own account may do to the launcher service: start it,
/// stop it, read its state and read its access list. Nothing else.
///
/// The owner needs start because the service is demand start: it is not
/// running until kGraf asks for it, so it runs only while kGraf needs it,
/// and the owner's unelevated process is what asks. Stop and the two reads
/// let `kgraf sandbox status` and `selfcheck` report the truth and let kGraf
/// put the service back to rest.
///
/// What is left out, and why: [`SERVICE_CHANGE_CONFIG`] would let the owner
/// point a LocalSystem service at another program, [`WRITE_DAC`] would let
/// the owner grant itself that right, and [`DELETE`] would let it remove the
/// service that the elevated setup installed. Each of those is an
/// administrator action, and the setup is the only place kGraf takes
/// administrator rights.
pub const OWNER_RIGHTS: u32 =
    SERVICE_START | SERVICE_STOP | SERVICE_QUERY_STATUS | SERVICE_QUERY_CONFIG | READ_CONTROL;

/// Text the service control manager could not take.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ServiceTextError {
    #[error(
        "{0:?} is not a SID in string form (S-1-...), so it cannot name an account in a service's access list"
    )]
    BadSid(String),
    #[error("the service's executable path is empty")]
    EmptyExe,
    #[error(
        "the service's executable path {0:?} holds a double quote, which the service control \
         manager cannot parse"
    )]
    QuoteInExe(String),
    #[error("the service's executable path cannot be written as Unicode text")]
    ExeNotUnicode,
}

/// The launcher service's own access list, in SDDL.
///
/// SYSTEM and the local Administrators group keep full control: the service
/// runs as LocalSystem and only an administrator installs, repairs or removes
/// it. The owner's account gets [`OWNER_RIGHTS`] and nothing else. The list
/// is protected (`P`), so no later inheritance widens it.
///
/// `owner_sid` is checked as a SID string first. A SID is the only part of
/// this text that does not come from kGraf itself, and SDDL has its own
/// syntax characters, so anything that is not a SID is refused rather than
/// written into the list.
pub fn dacl_sddl(owner_sid: &str) -> Result<String, ServiceTextError> {
    if !is_sid_string(owner_sid) {
        return Err(ServiceTextError::BadSid(owner_sid.to_owned()));
    }
    // `SY` is SYSTEM and `BA` the local Administrators group: SDDL's own
    // abbreviations, so the list does not depend on the system language. The
    // owner's rights are written as a hex mask, which is the same number
    // [`OWNER_RIGHTS`] is tested against.
    Ok(format!(
        "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x{OWNER_RIGHTS:08x};;;{owner_sid})"
    ))
}

/// The service's image path: the executable in quotes, then each argument.
///
/// The quotes are the point. The service control manager reads an image path
/// as a command line, so an unquoted `C:\Program Files\kGraf\kgraf.exe`
/// names `C:\Program.exe` with `Files\kGraf\kgraf.exe` as its first
/// argument, and Windows starts `C:\Program.exe` where one exists.
pub fn image_path(exe: &Path, args: &[String]) -> Result<String, ServiceTextError> {
    let exe = exe.to_str().ok_or(ServiceTextError::ExeNotUnicode)?;
    if exe.is_empty() {
        return Err(ServiceTextError::EmptyExe);
    }
    // A Windows path cannot hold a double quote, so this is a programming
    // error rather than a path to escape.
    if exe.contains('"') {
        return Err(ServiceTextError::QuoteInExe(exe.to_owned()));
    }
    let mut out = String::with_capacity(exe.len() + 2 + 16 * args.len());
    out.push('"');
    out.push_str(exe);
    out.push('"');
    for arg in args {
        out.push(' ');
        out.push_str(&quote_argument(arg));
    }
    Ok(out)
}

/// One argument as the Windows command-line parser reads it back.
///
/// An argument that holds no space, tab or quote is written as it is. Any
/// other argument is wrapped in quotes, with each backslash that runs up to
/// a quote doubled, because the parser reads `\"` as a literal quote. The
/// rule is the one `CommandLineToArgvW` documents.
pub fn quote_argument(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_owned();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => {
                backslashes += 1;
                out.push('\\');
            }
            '"' => {
                // The backslashes before a quote, and the backslash this
                // code adds to make the quote literal.
                for _ in 0..=backslashes {
                    out.push('\\');
                }
                backslashes = 0;
                out.push('"');
            }
            _ => {
                backslashes = 0;
                out.push(c);
            }
        }
    }
    // The closing quote would otherwise end an odd run of backslashes.
    for _ in 0..backslashes {
        out.push('\\');
    }
    out.push('"');
    out
}

/// The program and the arguments of a command line, as a Rust program on
/// Windows reads its own (`std::env::args_os`), which is how kGraf's service
/// reads the image path it was installed with. The other side of
/// [`image_path`], so the owner's side can read what the installed service
/// trusts (the runner path above all) from the service's configuration.
///
/// The program name has no escapes: a quote toggles quoting and a space or
/// tab outside quotes ends it. In the arguments, backslashes are literal
/// unless a quote follows them: then each pair is one backslash, and an odd
/// one left makes the quote literal. Inside quotes, two quotes are one
/// literal quote. These are the rules of the Rust standard library, which
/// follow those `CommandLineToArgvW` documents.
pub fn split_command_line(line: &str) -> (String, Vec<String>) {
    let mut units = line.chars().peekable();
    let mut program = String::new();
    let mut in_quotes = false;
    for c in units.by_ref() {
        match c {
            '"' => in_quotes = !in_quotes,
            ' ' | '\t' if !in_quotes => break,
            _ => program.push(c),
        }
    }
    while units.next_if(|c| *c == ' ' || *c == '\t').is_some() {}
    let mut args: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    while let Some(c) = units.next() {
        match c {
            ' ' | '\t' if !in_quotes => {
                args.push(std::mem::take(&mut current));
                while units.next_if(|c| *c == ' ' || *c == '\t').is_some() {}
            }
            '\\' => {
                let mut count = 1usize;
                while units.next_if_eq(&'\\').is_some() {
                    count += 1;
                }
                if units.peek() == Some(&'"') {
                    current.extend(std::iter::repeat_n('\\', count / 2));
                    if count % 2 == 1 {
                        units.next();
                        current.push('"');
                    }
                } else {
                    current.extend(std::iter::repeat_n('\\', count));
                }
            }
            '"' if in_quotes => match units.peek() {
                Some('"') => {
                    current.push('"');
                    units.next();
                }
                Some(_) => in_quotes = false,
                // The end of the line inside quotes: the argument is kept
                // even when it is empty.
                None => break,
            },
            '"' => in_quotes = true,
            _ => current.push(c),
        }
    }
    if !current.is_empty() || in_quotes {
        args.push(current);
    }
    (program, args)
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWNER: &str = "S-1-5-21-1004336348-1177238915-682003330-1001";

    /// The command line parser of `CommandLineToArgvW`, for the arguments
    /// after the program name: the other side of [`quote_argument`], so a
    /// test reads back what the quoting wrote.
    fn argv(line: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut current = String::new();
        let mut started = false;
        let mut in_quotes = false;
        let mut backslashes = 0usize;
        let flush = |current: &mut String, out: &mut Vec<String>, started: &mut bool| {
            if *started {
                out.push(std::mem::take(current));
                *started = false;
            }
        };
        for c in line.chars() {
            match c {
                '\\' => {
                    backslashes += 1;
                    started = true;
                }
                '"' => {
                    for _ in 0..backslashes / 2 {
                        current.push('\\');
                    }
                    if backslashes % 2 == 1 {
                        current.push('"');
                    } else {
                        in_quotes = !in_quotes;
                    }
                    backslashes = 0;
                    started = true;
                }
                ' ' | '\t' if !in_quotes => {
                    for _ in 0..backslashes {
                        current.push('\\');
                    }
                    backslashes = 0;
                    flush(&mut current, &mut out, &mut started);
                }
                _ => {
                    for _ in 0..backslashes {
                        current.push('\\');
                    }
                    backslashes = 0;
                    current.push(c);
                    started = true;
                }
            }
        }
        for _ in 0..backslashes {
            current.push('\\');
        }
        flush(&mut current, &mut out, &mut started);
        out
    }

    /// The image path reads back as the program and the arguments it was
    /// built from, so the owner's side reads the runner path the service
    /// itself trusts.
    #[test]
    fn an_image_path_splits_back_into_its_program_and_arguments() {
        let exe = r"C:\Program Files\kGraf\kgraf.exe";
        let args: Vec<String> = [
            "sandbox-launcher-service",
            "--runner",
            r"C:\Program Files\kGraf\kgraf-sandbox-runner.exe",
            "a tag with spaces",
            r"C:\a folder\\",
            r#"holds "quotes""#,
            "",
            "plain",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        let path = image_path(Path::new(exe), &args).expect("image path");
        let (program, back) = split_command_line(&path);
        assert_eq!(program, exe);
        assert_eq!(back, args, "{path}");
        assert_eq!(back, argv(path.split_once(r#".exe""#).expect("exe").1));
    }

    /// The rules of the Rust standard library, case by case: a program name
    /// without quotes, backslashes before a quote and elsewhere, two quotes
    /// inside quotes, runs of white space, and an empty last argument.
    #[test]
    fn a_command_line_splits_as_the_rust_standard_library_splits_it() {
        let split = |line: &str| split_command_line(line);
        let owned = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(split(r"C:\k.exe"), (r"C:\k.exe".to_owned(), vec![]));
        assert_eq!(
            split("C:\\k.exe  a\t b  "),
            ("C:\\k.exe".to_owned(), owned(&["a", "b"]))
        );
        assert_eq!(
            split(r#"k a\\b "c\\" d\"e"#),
            ("k".to_owned(), owned(&[r"a\\b", r"c\", r#"d"e"#]))
        );
        assert_eq!(
            split(r#"k "a""b" """#),
            ("k".to_owned(), owned(&[r#"a"b"#, ""]))
        );
        assert_eq!(
            split(r#""a b\c"d e"#),
            (r"a b\cd".to_owned(), owned(&["e"]))
        );
        assert_eq!(split(""), (String::new(), vec![]));
    }

    /// The trap this quoting exists for: an unquoted path with a space in it
    /// names another program.
    #[test]
    fn the_image_path_quotes_an_executable_whose_folder_holds_a_space() {
        let path = image_path(
            Path::new(r"C:\Program Files\kGraf\kgraf.exe"),
            &["sandbox-launcher-service".to_owned()],
        )
        .expect("image path");
        assert_eq!(
            path,
            r#""C:\Program Files\kGraf\kgraf.exe" sandbox-launcher-service"#
        );
        assert!(path.starts_with('"'), "{path}");
        // The quote closes before the first argument, so the manager reads
        // the whole path as the program.
        let (quoted, rest) = path[1..].split_once('"').expect("a closing quote");
        assert_eq!(quoted, r"C:\Program Files\kGraf\kgraf.exe");
        assert_eq!(rest, " sandbox-launcher-service");
    }

    /// Every argument comes back out of the image path exactly as it went
    /// in, so the service reads what setup installed it with.
    #[test]
    fn an_argument_with_a_space_a_quote_or_a_backslash_is_read_back_unchanged() {
        let args: Vec<String> = [
            "sandbox-launcher-service",
            "--tag",
            "a tag with spaces",
            r"C:\a folder\\",
            r#"holds "quotes""#,
            "",
            "plain",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        let path =
            image_path(Path::new(r"C:\Program Files\kGraf\kgraf.exe"), &args).expect("image path");
        let after_exe = path
            .split_once(r#".exe""#)
            .expect("the quoted executable")
            .1;
        assert_eq!(argv(after_exe), args, "{path}");
    }

    #[test]
    fn an_empty_or_unquotable_executable_path_is_refused() {
        assert_eq!(
            image_path(Path::new(""), &[]),
            Err(ServiceTextError::EmptyExe)
        );
        let odd = r#"C:\a"b\kgraf.exe"#;
        assert_eq!(
            image_path(Path::new(odd), &[]),
            Err(ServiceTextError::QuoteInExe(odd.to_owned()))
        );
    }

    /// The owner may start and stop the service and read its state. It may
    /// not point the service at another program, widen its own rights or
    /// remove it: those stay with the elevated setup.
    #[test]
    fn the_owners_rights_exclude_change_config_write_dac_and_delete() {
        for (name, right) in [
            ("change config", SERVICE_CHANGE_CONFIG),
            ("write DAC", WRITE_DAC),
            ("delete", DELETE),
        ] {
            assert_eq!(OWNER_RIGHTS & right, 0, "the owner must not hold {name}");
        }
        for (name, right) in [
            ("start", SERVICE_START),
            ("stop", SERVICE_STOP),
            ("query status", SERVICE_QUERY_STATUS),
            ("query config", SERVICE_QUERY_CONFIG),
            ("read control", READ_CONTROL),
        ] {
            assert_eq!(OWNER_RIGHTS & right, right, "the owner needs {name}");
        }
        // The whole mask, so a change to it is seen in review.
        assert_eq!(OWNER_RIGHTS, 0x0002_0035);
    }

    #[test]
    fn the_access_list_gives_system_and_administrators_full_control_and_the_owner_its_mask() {
        let sddl = dacl_sddl(OWNER).expect("access list");
        assert_eq!(
            sddl,
            format!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x00020035;;;{OWNER})")
        );
        assert!(sddl.starts_with("D:P"), "the list is protected: {sddl}");
        assert!(sddl.contains("(A;;GA;;;SY)"), "{sddl}");
        assert!(sddl.contains("(A;;GA;;;BA)"), "{sddl}");
        assert!(
            sddl.contains(&format!("0x{OWNER_RIGHTS:08x};;;{OWNER}")),
            "the owner's ACE carries the tested mask: {sddl}"
        );
        // No deny ACE and no second ACE for the owner.
        assert_eq!(sddl.matches("(D;").count(), 0, "{sddl}");
        assert_eq!(sddl.matches(OWNER).count(), 1, "{sddl}");
    }

    /// A SID is the only part of the list that setup does not write itself,
    /// and SDDL has its own syntax characters, so anything that is not a SID
    /// is refused rather than written into the list.
    #[test]
    fn a_bad_owner_sid_cannot_reach_the_access_list() {
        for bad in [
            "",
            "SY",
            "everyone",
            "S-1-5-21-1001)(A;;GA;;;WD",
            "S-1-5-21-x",
        ] {
            assert_eq!(
                dacl_sddl(bad),
                Err(ServiceTextError::BadSid(bad.to_owned())),
                "{bad:?}"
            );
        }
    }
}

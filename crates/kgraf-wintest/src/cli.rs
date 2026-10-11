//! Command-line parsing. Double-clicking the exe gives no arguments, which
//! is the full run.

use std::path::PathBuf;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunOptions {
    /// `Some(true)` runs the extended set, `Some(false)` skips it, `None` asks.
    pub extended: Option<bool>,
    /// Do not offer to delete the unzipped bundle folder at the end.
    pub keep_bundle: bool,
    /// Do not wait for a key at the end.
    pub no_wait: bool,
    /// Write the report here instead of the Desktop.
    pub report_dir: Option<PathBuf>,
    /// After the suite, set the Windows sandbox user up, re-run the parts it
    /// changes, and remove it again (needs administrator rights).
    pub sandbox_user: bool,
    /// Run only the named binaries, tests and end-to-end checks
    /// ([`crate::only`]).
    pub only: Option<crate::only::Only>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackOptions {
    pub messages: PathBuf,
    pub build_root: String,
    pub out: PathBuf,
    pub version: String,
    pub commit: String,
    pub built_utc: String,
    /// Files (relative to the build root) copied into the source mirror.
    pub src_list: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Run(RunOptions),
    /// Run one program inside kGraf's Windows sandbox (for manual tests).
    SandboxRun {
        program: String,
        args: Vec<String>,
    },
    /// Build the bundle manifest and stage its files (on the build machine).
    Pack(PackOptions),
    /// Only the end-to-end checks, against a given kgraf (for development).
    E2e {
        kgraf: PathBuf,
    },
    Help,
}

pub const USAGE: &str = "\
kgraf-wintest: runs kGraf's Windows tests and writes one report to the Desktop.

  kgraf-wintest                      full run; asks whether to add the extended set
  kgraf-wintest --core               the Windows-specific tests and end-to-end checks only
  kgraf-wintest --all                every shipped test, no question
      --keep-bundle                  do not offer to delete this folder at the end
      --no-wait                      do not wait for a key at the end
      --report-dir <folder>          write the report there instead of the Desktop
      --sandbox-user                 after the suite: set the Windows sandbox user up, re-run the
                                     parts it changes, remove it (needs administrator rights)
      --only <filter>                run only these, in the suite and in the sandbox user re-run:
                                     comma-separated `binary-id` or `binary-id::test-name` items
                                     (run ids as in the report); `e2e` or `e2e::<check-id>` adds
                                     end-to-end checks, which otherwise do not run
  kgraf-wintest sandbox-run -- <program> [args...]
                                     run one program inside kGraf's sandbox (for manual tests)
";

fn need<T>(v: Option<T>, n: &str) -> Result<T, String> {
    v.ok_or_else(|| format!("pack needs {n}"))
}

fn value(args: &mut std::slice::Iter<'_, String>, flag: &str) -> Result<String, String> {
    args.next()
        .cloned()
        .ok_or_else(|| format!("{flag} needs a value"))
}

/// Parse the arguments after the program name.
pub fn parse(args: &[String]) -> Result<Command, String> {
    match args.first().map(String::as_str) {
        Some("sandbox-run") => {
            let rest = &args[1..];
            let rest = match rest.first().map(String::as_str) {
                Some("--") => &rest[1..],
                _ => rest,
            };
            let (program, tail) = rest.split_first().ok_or(
                "sandbox-run needs a program: kgraf-wintest sandbox-run -- <program> [args...]",
            )?;
            Ok(Command::SandboxRun {
                program: program.clone(),
                args: tail.to_vec(),
            })
        }
        Some("pack") => {
            let mut it = args[1..].iter();
            let (
                mut messages,
                mut build_root,
                mut out,
                mut version,
                mut commit,
                mut built,
                mut src,
            ) = (None, None, None, None, None, None, None);
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--messages" => messages = Some(PathBuf::from(value(&mut it, a)?)),
                    "--build-root" => build_root = Some(value(&mut it, a)?),
                    "--out" => out = Some(PathBuf::from(value(&mut it, a)?)),
                    "--version" => version = Some(value(&mut it, a)?),
                    "--commit" => commit = Some(value(&mut it, a)?),
                    "--built" => built = Some(value(&mut it, a)?),
                    "--src-list" => src = Some(PathBuf::from(value(&mut it, a)?)),
                    other => return Err(format!("pack: unknown argument {other}")),
                }
            }
            Ok(Command::Pack(PackOptions {
                messages: need(messages, "--messages")?,
                build_root: need(build_root, "--build-root")?,
                out: need(out, "--out")?,
                version: need(version, "--version")?,
                commit: need(commit, "--commit")?,
                built_utc: need(built, "--built")?,
                src_list: need(src, "--src-list")?,
            }))
        }
        Some("e2e") => match (args.get(1).map(String::as_str), args.get(2)) {
            (Some("--kgraf"), Some(p)) => Ok(Command::E2e {
                kgraf: PathBuf::from(p),
            }),
            _ => Err("e2e needs --kgraf <path>".into()),
        },
        _ => {
            let mut o = RunOptions::default();
            let mut it = args.iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--core" => o.extended = Some(false),
                    "--all" => o.extended = Some(true),
                    "--keep-bundle" => o.keep_bundle = true,
                    "--no-wait" => o.no_wait = true,
                    "--sandbox-user" => o.sandbox_user = true,
                    "--report-dir" => o.report_dir = Some(PathBuf::from(value(&mut it, a)?)),
                    "--only" => {
                        let text = value(&mut it, a)?;
                        // An empty value is the full run, so a workflow can
                        // pass its input through whether or not it is set.
                        if !text.trim().is_empty() {
                            o.only = Some(crate::only::Only::parse(&text)?);
                        }
                    }
                    "-h" | "--help" | "/?" => return Ok(Command::Help),
                    other => return Err(format!("unknown argument {other}\n\n{USAGE}")),
                }
            }
            Ok(Command::Run(o))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Result<Command, String> {
        parse(&args.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
    }

    #[test]
    fn sandbox_run_takes_everything_after_the_separator_verbatim() {
        let c = p(&[
            "sandbox-run",
            "--",
            r"C:\kgraf-fwtest\curl.exe",
            "-sS",
            "-o",
            "NUL",
            "-w",
            "%{http_code}\n",
            "https://example.com",
        ])
        .expect("parses");
        assert_eq!(
            c,
            Command::SandboxRun {
                program: r"C:\kgraf-fwtest\curl.exe".into(),
                args: vec![
                    "-sS",
                    "-o",
                    "NUL",
                    "-w",
                    "%{http_code}\n",
                    "https://example.com"
                ]
                .into_iter()
                .map(String::from)
                .collect(),
            }
        );
        // An argument that looks like a flag of the runner still goes to the program.
        let c = p(&["sandbox-run", "--", "cmd", "--all"]).expect("parses");
        assert_eq!(
            c,
            Command::SandboxRun {
                program: "cmd".into(),
                args: vec!["--all".into()]
            }
        );
        // Windows PowerShell can drop a bare `--` before a native program.
        let c = p(&["sandbox-run", "cmd", "/c", "echo"]).expect("parses");
        assert_eq!(
            c,
            Command::SandboxRun {
                program: "cmd".into(),
                args: vec!["/c".into(), "echo".into()]
            }
        );
        assert!(p(&["sandbox-run", "--"]).is_err());
        assert!(p(&["sandbox-run"]).is_err());
    }

    #[test]
    fn no_arguments_is_the_full_run_and_flags_set_options() {
        assert_eq!(p(&[]).expect("parses"), Command::Run(RunOptions::default()));
        let Command::Run(o) = p(&[
            "--core",
            "--no-wait",
            "--sandbox-user",
            "--report-dir",
            "D:\\r",
        ])
        .expect("parses") else {
            panic!("a run");
        };
        assert_eq!(o.extended, Some(false));
        assert!(o.no_wait);
        assert!(o.sandbox_user);
        assert!(!RunOptions::default().sandbox_user);
        assert_eq!(o.report_dir, Some(PathBuf::from("D:\\r")));
        assert!(p(&["--bogus"]).is_err());
    }

    #[test]
    fn only_takes_a_filter_and_an_empty_one_is_the_full_run() {
        let Command::Run(o) = p(&["--all", "--only", "a/test/b::t,e2e"]).expect("parses") else {
            panic!("a run");
        };
        assert_eq!(
            o.only.as_ref().map(|f| f.text.as_str()),
            Some("a/test/b::t,e2e")
        );
        let Command::Run(o) = p(&["--only", ""]).expect("parses") else {
            panic!("a run");
        };
        assert_eq!(o.only, None);
        assert!(p(&["--only"]).is_err());
        assert!(p(&["--only", "a,,b"]).is_err());
    }
}

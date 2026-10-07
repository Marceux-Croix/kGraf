//! End-to-end checks with the real `kgraf.exe`: the worker shell through
//! Git Bash with a scripted model, the cmd and PowerShell readers under an
//! ask rule, the network notice, and the Git for Windows check.
//!
//! These automate sections 5, 6 and 6a of docs/WINDOWS-TESTING.md. The
//! model is [`crate::stub`], so no API key is used. Each run gets its own
//! repository, permission file and kGraf files inside the work folder. The
//! judges are pure functions of the tool results, tested on any host.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use serde::Serialize;

use crate::stub::{self, Step, Stub};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pass,
    Fail,
    Skipped,
}

#[derive(Clone, Debug, Serialize)]
pub struct Check {
    pub id: String,
    pub title: String,
    /// True when the condition was produced on purpose (for example a
    /// missing Git Bash named through the override) rather than found.
    pub simulated: bool,
    pub status: Status,
    pub detail: String,
    /// The output the verdict was read from (a tail).
    pub evidence: String,
}

fn check(id: &str, title: &str, ok: bool, detail: String, evidence: &str) -> Check {
    Check {
        id: id.into(),
        title: title.into(),
        simulated: false,
        status: if ok { Status::Pass } else { Status::Fail },
        detail,
        evidence: crate::libtest::tail_text(evidence, 30),
    }
}

fn skipped(id: &str, title: &str, why: &str) -> Check {
    Check {
        id: id.into(),
        title: title.into(),
        simulated: false,
        status: Status::Skipped,
        detail: why.into(),
        evidence: String::new(),
    }
}

/// The notice the first shell result carries on Windows (kgraf-sandbox's
/// `SHELL_NETWORK_OPEN_NOTICE` starts with it).
pub const NOTICE: &str = "network not blocked";
/// What an ask resolves to when nobody is attached (kgraf-host `worker::ask`).
pub const UNATTENDED_DENIAL: &str = "Nobody is attached";

/// The Git Bash worker run's script (section 5 of docs/WINDOWS-TESTING.md).
pub fn shell_script(windows: bool) -> Vec<Step> {
    let third: &[&str] = if windows {
        &["cmd", "/d", "/c", "echo", "second-call"]
    } else {
        &["echo", "second-call"]
    };
    vec![
        stub::shell(&[
            "sh",
            "-c",
            "echo hello from git bash; uname -s; printf '%s|' 'a b' c",
        ]),
        stub::shell(&["git", "status", "--short"]),
        stub::shell(third),
    ]
}

fn result(results: &BTreeMap<String, String>, n: usize) -> Option<&str> {
    results.get(&format!("call_{n}")).map(String::as_str)
}

/// Judge the Git Bash worker run.
pub fn judge_shell(results: &BTreeMap<String, String>, windows: bool) -> Vec<Check> {
    let mut out = Vec::new();
    let r1 = result(results, 1);
    let r2 = result(results, 2);
    let r3 = result(results, 3);
    let missing = "the worker did not return this tool result".to_owned();
    match r1 {
        None => out.push(check(
            "shell.git_bash",
            "sh -c runs through Git Bash",
            false,
            missing.clone(),
            "",
        )),
        Some(t) => {
            let uname_ok = !windows
                || ["MINGW", "MSYS", "CLANGARM64"]
                    .iter()
                    .any(|u| t.contains(u));
            let ok = t.contains("hello from git bash") && t.contains("a b|c|") && uname_ok;
            out.push(check(
                "shell.git_bash",
                "sh -c runs through Git Bash, quoting intact",
                ok,
                format!(
                    "expects `hello from git bash`, a MINGW/MSYS/CLANGARM64 uname line and `a b|c|`; uname line found: {uname_ok}"
                ),
                t,
            ));
        }
    }
    match r2 {
        None => out.push(check(
            "shell.git",
            "git status runs in the worktree",
            false,
            missing.clone(),
            "",
        )),
        Some(t) => {
            let ok = !t.contains("fatal:") && !t.contains("not a git repository");
            out.push(check(
                "shell.git",
                "git status runs in the worktree",
                ok,
                "expects no `fatal:` from git".into(),
                t,
            ));
        }
    }
    match r3 {
        None => out.push(check(
            "shell.plain",
            "a plain program runs",
            false,
            missing,
            "",
        )),
        Some(t) => out.push(check(
            "shell.plain",
            "a plain program runs (cmd /d /c echo)",
            t.contains("second-call"),
            "expects `second-call`".into(),
            t,
        )),
    }
    if windows {
        let first = r1.is_some_and(|t| t.contains(NOTICE));
        let later = [r2, r3]
            .iter()
            .flatten()
            .filter(|t| t.contains(NOTICE))
            .count();
        out.push(check(
            "notice.once",
            "the network notice comes with the first shell result only",
            first && later == 0,
            format!("first result has it: {first}; later results with it: {later}"),
            r1.unwrap_or_default(),
        ));
    }
    out
}

/// The permission file of section 6: Auto, one ask rule, granted programs.
pub const READERS_PERMISSIONS: &str = "mode = \"auto\"\nask = [\"shell(curl*)\"]\n[allow]\nprograms = [\"cmd\", \"powershell\", \"echo\", \"Get-Location\", \"Write-Output\"]\n";

/// Section 6's six argv lists: three plain (run on their grants), three
/// kGraf cannot read as plain text (asked, so denied when unattended).
pub fn readers_script() -> Vec<Step> {
    vec![
        stub::shell(&["cmd", "/d", "/c", "echo", "plain-cmd"]),
        stub::shell(&["cmd", "/d", "/c", "echo plain-cmd && echo second"]),
        stub::shell(&["powershell", "-NoProfile", "-Command", "Get-Location"]),
        stub::shell(&["cmd", "/d", "/c", "echo", "\"quoted\""]),
        stub::shell(&["cmd", "/d", "/c", "echo two words"]),
        stub::shell(&[
            "powershell",
            "-NoProfile",
            "-Command",
            "Write-Output 'quoted'",
        ]),
    ]
}

pub fn judge_readers(results: &BTreeMap<String, String>) -> Vec<Check> {
    let plain: [(&str, &[&str]); 3] = [
        ("readers.cmd_plain", &["plain-cmd"]),
        ("readers.cmd_operator", &["plain-cmd", "second"]),
        ("readers.powershell_plain", &[":\\"]),
    ];
    let mut out = Vec::new();
    for (i, (id, wants)) in plain.iter().enumerate() {
        let t = result(results, i + 1).unwrap_or_default();
        let ran =
            !t.is_empty() && !t.contains(UNATTENDED_DENIAL) && wants.iter().all(|w| t.contains(w));
        out.push(check(
            id,
            &format!("command {} runs on its grants without a question", i + 1),
            ran,
            format!("expects {wants:?} and no denial"),
            t,
        ));
    }
    let unreadable = [
        ("readers.cmd_quote", "holds `\"`"),
        (
            "readers.cmd_one_string",
            "one quoted string with no operator",
        ),
        ("readers.powershell_quote", "holds `'`"),
    ];
    for (i, (id, why)) in unreadable.iter().enumerate() {
        let t = result(results, i + 4).unwrap_or_default();
        let asked = t.contains(UNATTENDED_DENIAL);
        let names_rule = t.contains("curl");
        out.push(check(
            id,
            &format!("command {} asks ({why}) and is denied unattended", i + 4),
            asked,
            format!("expects the unattended denial; names the ask rule: {names_rule}"),
            t,
        ));
    }
    out
}

/// Judge one `kgraf` start for the Git for Windows check.
pub fn judge_git_check(
    id: &str,
    title: &str,
    code: Option<i32>,
    output: &str,
    expect: GitExpect,
) -> Check {
    let asked = output.contains("[y/N]");
    let (ok, detail) = match expect {
        GitExpect::Starts => (
            code == Some(0) && !output.contains("Git for Windows"),
            format!("expects exit 0 and no Git message; exit {code:?}"),
        ),
        GitExpect::BadOverride => (
            code != Some(0)
                && output.contains("KGRAF_GIT_BASH_PATH")
                && !output.contains("winget install")
                && !asked,
            format!(
                "expects an error naming KGRAF_GIT_BASH_PATH, no install offer, no question; exit {code:?}"
            ),
        ),
        GitExpect::NoWinget => (
            code != Some(0) && output.contains("winget is not available") && !asked,
            format!("expects `winget is not available` and no question; exit {code:?}"),
        ),
        GitExpect::DeclinedUnattended => (
            code != Some(0) && output.contains("To fix it by hand") && !asked,
            format!("expects the manual fix and no question (no terminal); exit {code:?}"),
        ),
    };
    check(id, title, ok, detail, output)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GitExpect {
    Starts,
    BadOverride,
    NoWinget,
    DeclinedUnattended,
}

/// The environment of a child: the runner's own, without anything
/// credential-shaped, without proxies, and without the user's own kGraf
/// overrides; then `set`.
pub fn child_env(
    base: impl IntoIterator<Item = (String, String)>,
    set: &[(String, String)],
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = base
        .into_iter()
        .filter(|(k, _)| {
            let up = k.to_ascii_uppercase();
            !kgraf_secrets::name_is_credential_shaped(k)
                && !up.starts_with("KGRAF_")
                && !up.starts_with("HAUSTORIA_")
                && !up.ends_with("_PROXY")
                && !up.starts_with("GIT_")
                && !up.starts_with("CARGO_")
        })
        .collect();
    for (k, v) in set {
        out.retain(|(ok, _)| !ok.eq_ignore_ascii_case(k));
        out.push((k.clone(), v.clone()));
    }
    out
}

/// `PATH` without the folders `winget` is found in (the WindowsApps alias
/// folder), to show kGraf's answer when winget is missing.
pub fn path_without_winget(path: &str) -> String {
    path.split(';')
        .filter(|d| !d.to_ascii_lowercase().contains("windowsapps"))
        .collect::<Vec<_>>()
        .join(";")
}

/// Everything the checks need from the runner.
pub struct Ctx<'a> {
    pub kgraf: PathBuf,
    /// A folder inside the work folder for this step's repositories.
    pub dir: PathBuf,
    /// The variables every kGraf child gets (diagnostics, settings, TEMP...).
    pub env: Vec<(String, String)>,
    /// Git Bash was found on this machine.
    pub git_bash_found: bool,
    pub cancel: &'a AtomicBool,
}

impl Ctx<'_> {
    fn command(&self, cwd: &Path, extra: &[(String, String)]) -> Command {
        let mut cmd = Command::new(&self.kgraf);
        let mut set = self.env.clone();
        set.extend_from_slice(extra);
        cmd.env_clear()
            .envs(child_env(std::env::vars(), &set))
            .current_dir(cwd);
        cmd
    }

    fn run(&self, cmd: Command, secs: u64) -> crate::proc::Finished {
        crate::proc::run(cmd, Duration::from_secs(secs), self.cancel, |_| {})
    }
}

fn git(dir: &Path, args: &[&str], env: &[(String, String)]) -> Result<(), String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=kgraf-wintest",
            "-c",
            "user.email=kgraf-wintest@localhost",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .env_clear()
        .envs(child_env(std::env::vars(), env))
        .output()
        .map_err(|e| format!("git did not start: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}

/// A repository with one commit at `dir`.
pub fn make_repo(dir: &Path, env: &[(String, String)]) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    git(dir, &["init", "-q", "-b", "main"], env)?;
    std::fs::write(dir.join("README.md"), "kgraf-wintest\n").map_err(|e| e.to_string())?;
    git(dir, &["add", "-A"], env)?;
    git(dir, &["commit", "-q", "-m", "start"], env)
}

/// Run `kgraf worker` against the stub playing `script`; the tool results
/// it received, and the worker's output.
fn scripted_worker(
    ctx: &Ctx,
    tag: &str,
    script: Vec<Step>,
    permissions: &str,
) -> Result<(BTreeMap<String, String>, crate::proc::Finished), String> {
    let base = ctx.dir.join(tag);
    let repo = base.join("repo");
    make_repo(&repo, &ctx.env)?;
    let perm = base.join("permissions.toml");
    std::fs::write(&perm, permissions).map_err(|e| e.to_string())?;
    let brief = base.join("brief.txt");
    std::fs::write(&brief, "Run the listed tool calls.").map_err(|e| e.to_string())?;
    let steps = script.len();
    let stub = Stub::start(script).map_err(|e| format!("stub did not start: {e}"))?;
    let extra = vec![
        (
            "KGRAF_PERMISSIONS_PATH".to_owned(),
            perm.display().to_string(),
        ),
        ("KGRAF_OLLAMA_BASE_URL".to_owned(), stub.base_url()),
        ("NO_PROXY".to_owned(), "127.0.0.1,localhost".to_owned()),
    ];
    let mut cmd = ctx.command(&repo, &extra);
    cmd.args([
        "worker",
        "--provider",
        "ollama",
        "--model",
        "stub",
        "--brief",
    ])
    .arg(&brief)
    .args(["--worker", "w1", "--task", "t1", "--budget"])
    .arg((steps + 2).to_string());
    let finished = ctx.run(cmd, 300);
    Ok((stub.results(), finished))
}

fn worker_check(tag: &str, f: &crate::proc::Finished, posts_seen: bool) -> Check {
    check(
        &format!("{tag}.worker"),
        "kgraf worker starts, reaches the scripted model and ends",
        f.start_error.is_none() && !f.timed_out && posts_seen,
        format!(
            "exit {:?}; timed out: {}; start error: {:?}",
            f.code, f.timed_out, f.start_error
        ),
        &f.tail_text(30),
    )
}

/// Every end-to-end check this machine can run.
pub fn run_all(ctx: &Ctx, windows: bool) -> Vec<Check> {
    let mut out = Vec::new();
    if !ctx.kgraf.is_file() {
        out.push(skipped(
            "e2e",
            "end-to-end checks",
            "kgraf.exe is not in the bundle",
        ));
        return out;
    }
    let git_ok = Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if windows {
        out.extend(git_checks(ctx));
    }
    if !git_ok {
        out.push(skipped(
            "shell",
            "worker shell run",
            "git is not on PATH; Git for Windows is needed",
        ));
        return out;
    }
    match scripted_worker(ctx, "shell", shell_script(windows), "mode = \"bypass\"\n") {
        Ok((results, f)) => {
            out.push(worker_check("shell", &f, !results.is_empty()));
            out.extend(judge_shell(&results, windows));
        }
        Err(e) => out.push(check("shell.setup", "worker shell run setup", false, e, "")),
    }
    if windows {
        match scripted_worker(ctx, "readers", readers_script(), READERS_PERMISSIONS) {
            Ok((results, f)) => {
                out.push(worker_check("readers", &f, !results.is_empty()));
                out.extend(judge_readers(&results));
            }
            Err(e) => out.push(check("readers.setup", "readers run setup", false, e, "")),
        }
    }
    out
}

fn git_checks(ctx: &Ctx) -> Vec<Check> {
    let mut out = Vec::new();
    let dir = ctx.dir.join("git-check");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        out.push(check(
            "git.setup",
            "Git check setup",
            false,
            e.to_string(),
            "",
        ));
        return out;
    }
    let start = |extra: &[(String, String)]| {
        let mut cmd = ctx.command(&dir, extra);
        cmd.arg("providers");
        ctx.run(cmd, 120)
    };
    let joined = |f: &crate::proc::Finished| {
        let mut t = f.tail.join("\n");
        if let Some(e) = &f.start_error {
            t.push_str(e);
        }
        t
    };
    let missing = dir.join("missing").join("bin").join("bash.exe");
    let missing_var = (
        "KGRAF_GIT_BASH_PATH".to_owned(),
        missing.display().to_string(),
    );
    let path = std::env::var("PATH").unwrap_or_default();
    let no_winget = ("PATH".to_owned(), path_without_winget(&path));

    if ctx.git_bash_found {
        let f = start(&[]);
        out.push(judge_git_check(
            "git.present",
            "Git for Windows present: kGraf starts with no question",
            f.code,
            &joined(&f),
            GitExpect::Starts,
        ));
        let f = start(std::slice::from_ref(&missing_var));
        let mut c = judge_git_check(
            "git.missing_simulated",
            "Git Bash missing (simulated: KGRAF_GIT_BASH_PATH names a missing file)",
            f.code,
            &joined(&f),
            GitExpect::BadOverride,
        );
        c.simulated = true;
        out.push(c);
        let f = start(&[missing_var, no_winget]);
        let mut c = judge_git_check(
            "git.no_winget_simulated",
            "Git Bash missing and no winget on PATH (simulated)",
            f.code,
            &joined(&f),
            GitExpect::BadOverride,
        );
        c.simulated = true;
        c.detail.push_str(
            "; with the override set kGraf reports the override and does not look for winget, \
             so the no-winget text itself is covered by the kgraf-sandbox unit tests of \
             git_for_windows::ensure",
        );
        out.push(c);
    } else {
        // Git for Windows is really missing: both answers are live, and
        // nothing is installed (stdin is not a terminal, so kGraf cannot ask).
        let f = start(std::slice::from_ref(&no_winget));
        out.push(judge_git_check(
            "git.no_winget",
            "Git missing, no winget on PATH: the error says so",
            f.code,
            &joined(&f),
            GitExpect::NoWinget,
        ));
        let f = start(&[]);
        out.push(judge_git_check(
            "git.missing_unattended",
            "Git missing, no terminal: no question, the manual fix",
            f.code,
            &joined(&f),
            GitExpect::DeclinedUnattended,
        ));
        let f = start(&[missing_var]);
        out.push(judge_git_check(
            "git.bad_override",
            "KGRAF_GIT_BASH_PATH names a missing file",
            f.code,
            &joined(&f),
            GitExpect::BadOverride,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn results(items: &[(&str, &str)]) -> BTreeMap<String, String> {
        items
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn the_shell_run_passes_only_with_the_notice_on_the_first_result() {
        let notice =
            "[sandbox] network not blocked: the Windows sandbox cannot deny network access";
        let good = results(&[
            (
                "call_1",
                &format!("hello from git bash\nMINGW64_NT-10.0-26100\na b|c|\n{notice}"),
            ),
            ("call_2", ""),
            ("call_3", "second-call\r\n"),
        ]);
        let checks = judge_shell(&good, true);
        assert!(
            checks.iter().all(|c| c.status == Status::Pass),
            "{checks:#?}"
        );
        let repeated = results(&[
            (
                "call_1",
                &format!("hello from git bash\nMINGW64_NT\na b|c|\n{notice}"),
            ),
            ("call_2", notice),
            ("call_3", "second-call"),
        ]);
        let c = judge_shell(&repeated, true);
        let once = c
            .iter()
            .find(|c| c.id == "notice.once")
            .expect("notice check");
        assert_eq!(once.status, Status::Fail);
        let wsl = results(&[("call_1", "hello from git bash\nLinux\na b|c|")]);
        let c = judge_shell(&wsl, true);
        assert_eq!(c[0].status, Status::Fail, "a WSL bash is not Git Bash");
        assert_eq!(c[1].status, Status::Fail, "a missing result fails");
    }

    #[test]
    fn readers_pass_when_plain_ones_run_and_unreadable_ones_are_denied() {
        let denial = "shell(curl*) cannot be checked... Nobody is attached to answer";
        let good = results(&[
            ("call_1", "plain-cmd"),
            ("call_2", "plain-cmd\nsecond"),
            ("call_3", "Path\n----\nC:\\work\\repo"),
            ("call_4", denial),
            ("call_5", denial),
            ("call_6", denial),
        ]);
        let c = judge_readers(&good);
        assert!(c.iter().all(|c| c.status == Status::Pass), "{c:#?}");
        let ran_quote = results(&[("call_4", "\"quoted\"")]);
        let c = judge_readers(&ran_quote);
        assert_eq!(c[3].status, Status::Fail, "an unreadable script must ask");
        assert_eq!(c[0].status, Status::Fail, "a missing plain result fails");
    }

    #[test]
    fn the_git_check_answers_are_judged() {
        let bad = "Error: KGRAF_GIT_BASH_PATH names C:\\x\\bash.exe, which is not a file";
        assert_eq!(
            judge_git_check("a", "a", Some(1), bad, GitExpect::BadOverride).status,
            Status::Pass
        );
        let asked = format!("{bad}\n[y/N] ");
        assert_eq!(
            judge_git_check("a", "a", Some(1), &asked, GitExpect::BadOverride).status,
            Status::Fail
        );
        assert_eq!(
            judge_git_check("a", "a", Some(0), "routes: ollama", GitExpect::Starts).status,
            Status::Pass
        );
        assert_eq!(
            judge_git_check(
                "a",
                "a",
                Some(1),
                "winget is not available on this machine.",
                GitExpect::NoWinget
            )
            .status,
            Status::Pass
        );
    }

    #[test]
    fn the_child_environment_drops_credentials_proxies_and_user_overrides() {
        let base = vec![
            ("PATH".to_owned(), "C:\\Windows".to_owned()),
            ("OPENAI_API_KEY".to_owned(), "sk-x".to_owned()),
            (
                "KGRAF_PERMISSIONS_PATH".to_owned(),
                "C:\\mine.toml".to_owned(),
            ),
            ("HTTPS_PROXY".to_owned(), "http://p".to_owned()),
            ("GIT_DIR".to_owned(), "x".to_owned()),
            ("TEMP".to_owned(), "C:\\Temp".to_owned()),
        ];
        let env = child_env(base, &[("Temp".to_owned(), "W:\\t".to_owned())]);
        let names: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, vec!["PATH", "Temp"]);
        assert_eq!(env[1].1, "W:\\t");
    }

    #[test]
    fn winget_folders_leave_the_path() {
        let p = "C:\\Windows;C:\\Users\\a\\AppData\\Local\\Microsoft\\WindowsApps;C:\\Git\\cmd";
        assert_eq!(path_without_winget(p), "C:\\Windows;C:\\Git\\cmd");
    }
}

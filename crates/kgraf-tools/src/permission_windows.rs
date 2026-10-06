//! The commands of a Windows shell's script (decision 0094, amended
//! 2026-10-06): `cmd /c` and `cmd /k`, and `powershell` or `pwsh` with
//! `-Command`, `-EncodedCommand` (base64 of UTF-16LE text, decoded here) or,
//! for `powershell`, a script given as plain arguments. Each command is
//! returned as an argv, so a rule, the `forbid` rules and a grant judge it
//! like any other; what kGraf cannot place is unreadable.
//!
//! Written new: no Rust crate parses cmd or PowerShell syntax for this use
//! (searched crates.io for cmd and PowerShell parsers on 2026-10-06; the
//! PowerShell ones bind the .NET parser). The readers only find command
//! words and refuse what they cannot place, as the POSIX script reader
//! does:
//!
//! - cmd: commands split on `&`, `&&`, `||` and `|` outside double quotes,
//!   `^` escaping the next character. Unreadable: `(` groups and `if`/`for`
//!   (which run commands in their clauses), a program word holding `%` or
//!   `!` (a variable), `call :label`, and `cmd` with no `/c` or `/k`.
//! - PowerShell: statements split on `;`, `|`, `&&`, `||`, newlines and
//!   braces outside quotes; a statement that starts with a variable or a
//!   literal is an expression, whose right side of `=` is read again.
//!   Unreadable: `Invoke-Expression`, `Invoke-Command`, `Start-Job`, `Add-Type`,
//!   `New-Object`, `Set-Alias`, `Import-Module`, `.Invoke`, .NET calls (`[`,
//!   `::`), subexpressions (`$(`, `@(`), the call or dot operator on a
//!   variable or an expression, dot-sourcing, backtick escapes, a program
//!   word holding `$`, `-File`, `-Command -` (standard input), `-NoExit`, and
//!   a text that does not decode.

use super::Embedded;

fn unreadable(program: &str, why: &str) -> Vec<Embedded> {
    vec![Embedded::Unreadable(format!("`{program}` {why}"))]
}

/// `cmd [/q] [/d] [/s] [/v:on] [/c | /k] STRING`.
pub(super) fn cmd(args: &[String]) -> Vec<Embedded> {
    let mut delayed = false;
    for (i, a) in args.iter().enumerate() {
        let lower = a.to_ascii_lowercase();
        if lower == "/c" || lower == "/k" || lower == "/r" {
            let script = args[i + 1..].join(" ");
            if script.trim().is_empty() {
                return unreadable(
                    "cmd",
                    "with an empty `/c` command runs nothing kGraf can read",
                );
            }
            // `/s` strips one pair of outer quotes; reading without it
            // treats the quoted text as one command, which is how cmd reads
            // most such lines too.
            let script = script.trim();
            let script = script
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .filter(|inner| {
                    !inner.contains('"') || args.iter().any(|a| a.eq_ignore_ascii_case("/s"))
                })
                .unwrap_or(script);
            return cmd_script(script, delayed);
        }
        if lower.starts_with("/v:on") {
            delayed = true;
            continue;
        }
        if lower.starts_with('/') {
            continue;
        }
        // A word that is not an option before `/c`: cmd runs it as a
        // command line.
        break;
    }
    unreadable(
        "cmd",
        "with no `/c` or `/k` command reads its commands from its input",
    )
}

/// The commands of a cmd script, or why it cannot be read.
fn cmd_script(script: &str, delayed: bool) -> Vec<Embedded> {
    let mut commands: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let chars: Vec<char> = script.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '"' => {
                quoted = !quoted;
                cur.push(c);
            }
            '^' if !quoted => {
                if let Some(&n) = chars.get(i + 1) {
                    cur.push('^');
                    cur.push(n);
                    i += 1;
                }
            }
            '&' | '|' if !quoted => {
                // `2>&1`, `>&2`: descriptor duplication separates nothing.
                if c == '&' && cur.ends_with('>') {
                    cur.push(c);
                } else {
                    commands.push(std::mem::take(&mut cur));
                    if chars.get(i + 1) == Some(&c) {
                        i += 1;
                    }
                }
            }
            '\n' | '\r' if !quoted => commands.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
        i += 1;
    }
    commands.push(cur);
    let mut out = Vec::new();
    for command in commands {
        let words = cmd_words(&command);
        let words = strip_cmd_redirections(words);
        let Some(first) = words.first() else {
            continue;
        };
        let program = first.trim_start_matches('@').to_owned();
        let lower = program.to_ascii_lowercase();
        if program.is_empty() || program.starts_with('(') || command.trim_start().starts_with('(') {
            return unreadable(
                "cmd",
                "runs a parenthesized group, which kGraf does not read",
            );
        }
        if matches!(lower.as_str(), "if" | "for") {
            return unreadable(
                "cmd",
                &format!("runs `{lower}`, whose clauses run commands kGraf does not read"),
            );
        }
        if program.contains('%') || (delayed && program.contains('!')) {
            return unreadable(
                "cmd",
                "runs a command named by a variable, so kGraf cannot tell what it runs",
            );
        }
        let mut argv = vec![program];
        argv.extend(words[1..].iter().cloned());
        match lower.as_str() {
            "call" => match argv.get(1) {
                Some(target) if target.starts_with(':') => {
                    return unreadable("cmd", "calls a label of a batch file kGraf does not read");
                }
                Some(_) => argv.remove(0),
                None => continue,
            },
            // `start ["title"] [/options] PROGRAM ARGS` runs PROGRAM.
            "start" => {
                let rest: Vec<String> = words[1..]
                    .iter()
                    .skip_while(|w| w.starts_with('/'))
                    .cloned()
                    .collect();
                let rest = match rest.first() {
                    Some(t) if command.contains(&format!("\"{t}\"")) && rest.len() > 1 => {
                        rest[1..].to_vec()
                    }
                    _ => rest,
                };
                let rest: Vec<String> = rest
                    .into_iter()
                    .skip_while(|w| w.starts_with('/'))
                    .collect();
                if rest.is_empty() {
                    return unreadable("cmd", "`start` with no program starts a new shell");
                }
                out.push(Embedded::Argv(argv));
                out.push(Embedded::Argv(rest));
                continue;
            }
            _ => String::new(),
        };
        out.push(Embedded::Argv(argv));
    }
    out
}

/// The words of one cmd command: split on whitespace outside double quotes,
/// quotes removed, `^` escapes resolved.
fn cmd_words(command: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut started = false;
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            '^' if !quoted => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                    started = true;
                }
            }
            ' ' | '\t' if !quoted => {
                if started {
                    words.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            _ => {
                cur.push(c);
                started = true;
            }
        }
    }
    if started {
        words.push(cur);
    }
    words
}

/// The words of a cmd command without its redirections (`>out`, `2>&1`,
/// `< in`, `>> log`).
fn strip_cmd_redirections(words: Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    let mut skip_next = false;
    for w in words {
        if skip_next {
            skip_next = false;
            continue;
        }
        let rest = w.trim_start_matches(|c: char| c.is_ascii_digit());
        if rest.starts_with('>') || rest.starts_with('<') {
            let target = rest.trim_start_matches(['>', '<', '&']);
            skip_next = target.is_empty() && !rest.ends_with('&') && !rest.contains('&');
            continue;
        }
        out.push(w);
    }
    out
}

/// What a PowerShell command line option is, for [`powershell`].
#[derive(Clone, Copy, PartialEq, Eq)]
enum PsOption {
    Command,
    Encoded,
    File,
    /// Takes a value in the next word.
    Valued,
    Flag,
    /// Leaves the shell open, reading input.
    Interactive,
}

/// How PowerShell reads an option's name: any unambiguous prefix of a
/// parameter, with the documented short forms (`-c`, `-e`, `-ec`, `-f`).
fn ps_option(name: &str) -> Option<PsOption> {
    let n = name.trim_start_matches(['-', '/']).to_ascii_lowercase();
    let prefix_of = |full: &str, min: usize| n.len() >= min && full.starts_with(n.as_str());
    Some(
        if n == "c" || prefix_of("command", 3) || prefix_of("commandwithargs", 8) || n == "cwa" {
            PsOption::Command
        } else if n == "e" || n == "ec" || prefix_of("encodedcommand", 2) && n != "ex" && n != "ep"
        {
            PsOption::Encoded
        } else if n == "f" || prefix_of("file", 2) {
            PsOption::File
        } else if n == "ex"
            || n == "ep"
            || prefix_of("executionpolicy", 2)
            || n == "w"
            || prefix_of("windowstyle", 2)
            || n == "if"
            || prefix_of("inputformat", 3)
            || n == "o"
            || n == "of"
            || prefix_of("outputformat", 2)
            || n == "v"
            || prefix_of("version", 2)
            || n == "config"
            || prefix_of("configurationname", 3)
            || prefix_of("psconsolefile", 3)
            || n == "wd"
            || prefix_of("workingdirectory", 3)
            || prefix_of("settingsfile", 3)
            || prefix_of("custompipename", 3)
            || n == "ea"
            || prefix_of("encodedarguments", 9)
        {
            PsOption::Valued
        } else if prefix_of("noexit", 3) || n == "i" || prefix_of("interactive", 3) {
            PsOption::Interactive
        } else if n == "nop"
            || prefix_of("noprofile", 3)
            || n == "noni"
            || prefix_of("noninteractive", 4)
            || n == "nol"
            || prefix_of("nologo", 3)
            || n == "mta"
            || n == "sta"
            || n == "l"
            || prefix_of("login", 2)
            || n == "?"
            || n == "h"
            || prefix_of("help", 2)
            || prefix_of("noprofileloadtime", 10)
        {
            PsOption::Flag
        } else {
            return None;
        },
    )
}

/// `powershell [OPTIONS] -Command TEXT...`, `-EncodedCommand B64`, or (for
/// `powershell`, not `pwsh`) the script as plain arguments.
pub(super) fn powershell(program: &str, args: &[String]) -> Vec<Embedded> {
    let mut i = 0;
    while let Some(a) = args.get(i) {
        if !(a.starts_with('-') || (a.starts_with('/') && a.len() > 1)) {
            break;
        }
        match ps_option(a) {
            Some(PsOption::Command) => {
                let text = args[i + 1..].join(" ");
                if text.trim() == "-" {
                    return unreadable(program, "reads its commands from its input");
                }
                if text.trim().is_empty() {
                    return Vec::new();
                }
                return ps_script(program, &text);
            }
            Some(PsOption::Encoded) => {
                let Some(b64) = args.get(i + 1) else {
                    return unreadable(program, "names no encoded command");
                };
                return match decode_utf16le_base64(b64) {
                    Some(text) => ps_script(program, &text),
                    None => unreadable(program, "has an encoded command that does not decode"),
                };
            }
            Some(PsOption::File) => {
                return unreadable(program, "runs a script file, which kGraf does not read");
            }
            Some(PsOption::Interactive) => {
                return unreadable(program, "stays open reading commands from its input");
            }
            Some(PsOption::Valued) => i += 2,
            Some(PsOption::Flag) => i += 1,
            None => {
                return unreadable(
                    program,
                    &format!(
                        "has the option `{}`, which kGraf cannot place",
                        a.escape_debug()
                    ),
                );
            }
        }
    }
    match args.get(i..) {
        // `pwsh FILE`: the first plain argument is a script file.
        Some(rest) if !rest.is_empty() && program == "pwsh" => {
            unreadable(program, "runs a script file, which kGraf does not read")
        }
        Some(rest) if !rest.is_empty() => ps_script(program, &rest.join(" ")),
        _ => unreadable(program, "with no command reads its commands from its input"),
    }
}

/// Base64 of UTF-16LE text, as `-EncodedCommand` takes it.
fn decode_utf16le_base64(text: &str) -> Option<String> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(text.trim())
        .ok()?;
    if bytes.len() % 2 != 0 {
        return None;
    }
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|p| u16::from_le_bytes([p[0], p[1]]))
        .collect();
    String::from_utf16(&units).ok()
}

/// Cmdlets and operators whose effect kGraf cannot read: they run text as
/// code, run code elsewhere, load code, or rename commands.
const PS_OPAQUE: &[&str] = &[
    "invoke-expression",
    "iex",
    "invoke-command",
    "icm",
    "invoke-item",
    "ii",
    "start-job",
    "sajb",
    "start-threadjob",
    "add-type",
    "new-object",
    "set-alias",
    "sal",
    "new-alias",
    "nal",
    "import-module",
    "ipmo",
    "invoke-webrequest",
    "iwr",
    "invoke-restmethod",
    "irm",
    "register-scheduledtask",
    "register-scheduledjob",
    "register-wmievent",
    "invoke-wmimethod",
    "invoke-cimmethod",
    "enter-pssession",
    "new-pssession",
    "start-transcript",
];

/// PowerShell keywords: a statement that starts with one runs no command by
/// its name; its clauses are read as statements of their own.
const PS_KEYWORDS: &[&str] = &[
    "if", "elseif", "else", "foreach", "for", "while", "do", "until", "switch", "try", "catch",
    "finally", "function", "filter", "param", "begin", "process", "end", "return", "throw",
    "break", "continue", "exit", "trap", "data", "class", "enum", "in",
];

/// The commands of a PowerShell script, or why it cannot be read.
fn ps_script(program: &str, text: &str) -> Vec<Embedded> {
    let lower = text.to_ascii_lowercase();
    for marker in ["$(", "@(", "::", "[", "`", ".invoke", "-encodedcommand"] {
        if lower.contains(marker) {
            return unreadable(
                program,
                &format!("runs a script with `{marker}`, which kGraf does not read"),
            );
        }
    }
    let mut out = Vec::new();
    for statement in ps_statements(text) {
        let words = ps_words(&statement);
        match ps_command(&words) {
            Ok(Some(argv)) => {
                let key = argv[0].to_ascii_lowercase();
                if PS_OPAQUE.contains(&key.as_str()) {
                    return unreadable(
                        program,
                        &format!("runs `{}`, whose effect kGraf cannot read", argv[0]),
                    );
                }
                if matches!(key.as_str(), "start-process" | "saps" | "start") {
                    match start_process(&argv) {
                        Ok(inner) => {
                            out.push(Embedded::Argv(argv));
                            out.push(Embedded::Argv(inner));
                        }
                        Err(why) => return unreadable(program, &why),
                    }
                    continue;
                }
                out.push(Embedded::Argv(argv));
            }
            Ok(None) => {}
            Err(why) => return unreadable(program, &why),
        }
    }
    out
}

/// A PowerShell script split into statements on `;`, `|`, `&&`, `||`,
/// newlines, braces and parentheses outside quotes.
fn ps_statements(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let (mut single, mut double) = (false, false);
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\'' if !double => {
                single = !single;
                cur.push(c);
            }
            '"' if !single => {
                double = !double;
                cur.push(c);
            }
            ';' | '|' | '\n' | '\r' | '{' | '}' | '(' | ')' if !single && !double => {
                out.push(std::mem::take(&mut cur));
                if c == '|' && chars.get(i + 1) == Some(&'|') {
                    i += 1;
                }
            }
            '&' if !single && !double && chars.get(i + 1) == Some(&'&') => {
                out.push(std::mem::take(&mut cur));
                i += 1;
            }
            _ => cur.push(c),
        }
        i += 1;
    }
    out.push(cur);
    out.into_iter()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect()
}

/// A word of a PowerShell statement: its text without quotes, and whether
/// it was quoted, and whether it expands (`$` outside single quotes).
struct PsWord {
    text: String,
    quoted: bool,
    expands: bool,
}

fn ps_words(statement: &str) -> Vec<PsWord> {
    let mut out = Vec::new();
    let mut cur = PsWord {
        text: String::new(),
        quoted: false,
        expands: false,
    };
    let mut started = false;
    let (mut single, mut double) = (false, false);
    for c in statement.chars() {
        match c {
            '\'' if !double => {
                single = !single;
                cur.quoted = true;
                started = true;
            }
            '"' if !single => {
                double = !double;
                cur.quoted = true;
                started = true;
            }
            ' ' | '\t' if !single && !double => {
                if started {
                    out.push(std::mem::replace(
                        &mut cur,
                        PsWord {
                            text: String::new(),
                            quoted: false,
                            expands: false,
                        },
                    ));
                    started = false;
                }
            }
            _ => {
                if c == '$' && !single {
                    cur.expands = true;
                }
                cur.text.push(c);
                started = true;
            }
        }
    }
    if started {
        out.push(cur);
    }
    out
}

/// The command a statement runs: `Ok(Some(argv))`, `Ok(None)` for an
/// expression or a keyword, `Err` when kGraf cannot tell.
fn ps_command(words: &[PsWord]) -> Result<Option<Vec<String>>, String> {
    let Some(first) = words.first() else {
        return Ok(None);
    };
    let text = first.text.as_str();
    // The call operator: `& 'C:\tools\curl.exe' ARGS`.
    if text == "&" {
        return match words.get(1) {
            Some(w) if !w.expands && !w.text.starts_with('$') => {
                Ok(Some(words[1..].iter().map(|w| w.text.clone()).collect()))
            }
            _ => Err("calls a command named by a variable or an expression".into()),
        };
    }
    // Dot-sourcing runs a script file in the current scope.
    if text == "." && words.len() > 1 {
        return Err("dot-sources a script file, which kGraf does not read".into());
    }
    // An expression: a variable, a literal or a member access. An
    // assignment's right side is a statement of its own.
    if text.starts_with('$') && !first.quoted {
        if let Some(eq) = words.iter().position(|w| {
            !w.quoted && matches!(w.text.as_str(), "=" | "+=" | "-=" | "*=" | "/=" | "??=")
        }) {
            return ps_command(&words[eq + 1..]);
        }
        return Ok(None);
    }
    if first.quoted || text.parse::<f64>().is_ok() || text.starts_with('.') || text.starts_with('-')
    {
        return Ok(None);
    }
    if PS_KEYWORDS.contains(&text.to_ascii_lowercase().as_str()) {
        return Ok(None);
    }
    if first.expands {
        return Err(format!(
            "runs a command named by a variable (`{}`), so kGraf cannot tell what it runs",
            text.escape_debug()
        ));
    }
    Ok(Some(words.iter().map(|w| w.text.clone()).collect()))
}

/// The program `Start-Process` starts: `-FilePath X` or its first plain
/// argument, with the words of `-ArgumentList`.
fn start_process(argv: &[String]) -> Result<Vec<String>, String> {
    let mut file: Option<&str> = None;
    let mut list: Vec<String> = Vec::new();
    let mut i = 1;
    while let Some(a) = argv.get(i) {
        let lower = a.to_ascii_lowercase();
        if lower.starts_with("-f") && "-filepath".starts_with(lower.as_str()) {
            file = argv.get(i + 1).map(String::as_str);
            i += 2;
        } else if lower.starts_with("-a") && "-argumentlist".starts_with(lower.as_str()) {
            if let Some(v) = argv.get(i + 1) {
                list.extend(
                    v.split(',')
                        .map(|p| p.trim().trim_matches(['\'', '"']).to_owned())
                        .filter(|p| !p.is_empty()),
                );
            }
            i += 2;
        } else if lower.starts_with('-') {
            // `-Verb RunAs`, `-WorkingDirectory D`, `-Wait`, `-NoNewWindow`.
            i += if matches!(
                lower.as_str(),
                "-wait" | "-nonewwindow" | "-passthru" | "-usenewenvironment" | "-loaduserprofile"
            ) {
                1
            } else {
                2
            };
        } else {
            if file.is_none() {
                file = Some(a);
            } else {
                list.push(a.clone());
            }
            i += 1;
        }
    }
    match file {
        Some(f) if !f.contains('$') && !f.is_empty() => {
            let mut out = vec![f.to_owned()];
            out.extend(list);
            Ok(out)
        }
        Some(_) => Err("starts a process named by a variable".into()),
        None => Err("starts a process kGraf cannot name".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_owned()).collect()
    }

    fn argvs(found: &[Embedded]) -> Vec<Vec<String>> {
        found
            .iter()
            .filter_map(|e| match e {
                Embedded::Argv(a) => Some(a.clone()),
                _ => None,
            })
            .collect()
    }

    fn unreadable_found(found: &[Embedded]) -> bool {
        found.iter().any(|e| matches!(e, Embedded::Unreadable(_)))
    }

    #[test]
    fn cmd_scripts_are_split_into_commands() {
        assert_eq!(
            argvs(&cmd(&argv(&["/c", "dir & curl x"]))),
            vec![argv(&["dir"]), argv(&["curl", "x"])]
        );
        assert_eq!(
            argvs(&cmd(&argv(&[
                "/C",
                "echo a && curl.exe \"x y\" 2>&1 | findstr a"
            ]))),
            vec![
                argv(&["echo", "a"]),
                argv(&["curl.exe", "x y"]),
                argv(&["findstr", "a"])
            ]
        );
        assert_eq!(
            argvs(&cmd(&argv(&["/k", "cu^rl x"]))),
            vec![argv(&["curl", "x"])]
        );
        assert_eq!(
            argvs(&cmd(&argv(&["/c", "start \"t\" /b curl x"]))),
            vec![
                argv(&["start", "t", "/b", "curl", "x"]),
                argv(&["curl", "x"])
            ]
        );
        assert!(unreadable_found(&cmd(&argv(&["/c", "%X% a"]))));
        assert!(unreadable_found(&cmd(&argv(&["/c", "(curl x)"]))));
        assert!(unreadable_found(&cmd(&argv(&[
            "/c",
            "for %i in (a) do curl %i"
        ]))));
        assert!(unreadable_found(&cmd(&argv(&[]))));
    }

    #[test]
    fn powershell_scripts_are_split_into_commands() {
        let ps = |words: &[&str]| powershell("powershell", &argv(words));
        assert_eq!(
            argvs(&ps(&[
                "-NoProfile",
                "-Command",
                "Get-Item x; curl.exe x | Out-Null"
            ])),
            vec![
                argv(&["Get-Item", "x"]),
                argv(&["curl.exe", "x"]),
                argv(&["Out-Null"])
            ]
        );
        assert_eq!(
            argvs(&ps(&["-c", "$r = curl x"])),
            vec![argv(&["curl", "x"])]
        );
        assert_eq!(
            argvs(&ps(&["-c", "Get-ChildItem | ForEach-Object { curl x }"])),
            vec![
                argv(&["Get-ChildItem"]),
                argv(&["ForEach-Object"]),
                argv(&["curl", "x"])
            ]
        );
        assert_eq!(
            argvs(&ps(&["-c", "& 'C:\\tools\\curl.exe' x"])),
            vec![argv(&["C:\\tools\\curl.exe", "x"])]
        );
        assert_eq!(
            argvs(&ps(&["-c", "Start-Process curl -ArgumentList 'x','y'"])),
            vec![
                argv(&["Start-Process", "curl", "-ArgumentList", "x,y"]),
                argv(&["curl", "x", "y"])
            ]
        );
        // `curl x` as UTF-16LE base64.
        let encoded = {
            use base64::Engine as _;
            let bytes: Vec<u8> = "curl x"
                .encode_utf16()
                .flat_map(|u| u.to_le_bytes())
                .collect();
            base64::engine::general_purpose::STANDARD.encode(bytes)
        };
        assert_eq!(argvs(&ps(&["-enc", &encoded])), vec![argv(&["curl", "x"])]);
        assert_eq!(
            argvs(&powershell("pwsh", &argv(&["-EncodedCommand", &encoded]))),
            vec![argv(&["curl", "x"])]
        );
        assert_eq!(argvs(&ps(&["curl", "x"])), vec![argv(&["curl", "x"])]);
        for opaque in [
            "iex 'curl x'",
            "Invoke-Expression $s",
            "$c = 'curl'; & $c x",
            "[Diagnostics.Process]::Start('curl')",
            "$(curl x)",
            ". ./x.ps1",
            "& $env:X x",
        ] {
            assert!(unreadable_found(&ps(&["-c", opaque])), "{opaque}");
        }
        assert!(unreadable_found(&ps(&["-File", "x.ps1"])));
        assert!(unreadable_found(&ps(&["-enc", "!!notbase64"])));
        assert!(unreadable_found(&ps(&["-Command", "-"])));
        assert!(unreadable_found(&ps(&[])));
        assert!(unreadable_found(&powershell("pwsh", &argv(&["x.ps1"]))));
    }
}

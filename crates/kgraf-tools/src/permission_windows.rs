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
//!
//! Both interpreters read their command line by their own rules, not the
//! MSVC rules the Windows backend quotes it for, and their parsers do more
//! than these readers model (decision 0094, amended 2026-10-07). So a script
//! is read as text only when the text checked is certainly the text the
//! interpreter runs; otherwise it is unreadable, with the best-effort reading
//! kept beside the reason so a deny rule and the `forbid` rules still see
//! it:
//!
//! - cmd: a double quote in any argument, `^`, `%`, `!`, a parenthesis, a
//!   line break or control character, a character outside ASCII, a switch
//!   before `/c` that is not `/X` or `/X:VALUE`, an argument with a space
//!   beside other arguments, a lone quoted script with no operator and no
//!   `/s` (cmd keeps its quotes when a file of that name exists), a quoted
//!   script ending in `\`, and a program word cmd would end at `,;=/` or
//!   read as a built-in (`rd\x`, `echo.`).
//! - PowerShell: a quote (ASCII or typographic), a backtick, `$`, `@`,
//!   braces, parentheses, `#`, `<`, a control character other than a tab or
//!   a line break, a character outside ASCII, `&` joined to other text, and
//!   an argument with a space that ends in `\`; for `-EncodedCommand`, the
//!   same rules on the decoded text.
//!
//! Plain scripts of words, spaces, the operators the readers split on and
//! cmd redirections read as before.

use super::Embedded;

fn unreadable(program: &str, why: &str) -> Vec<Embedded> {
    vec![Embedded::Unreadable(format!("`{program}` {why}"))]
}

/// Combine a reading with the reason the interpreter may read the script
/// differently from kGraf: the unreadable reason first, then the best-effort
/// reading, so a deny rule and the `forbid` rules still see what the script
/// would most likely run while the call stays unreadable (asked under a
/// shell rule, never granted).
fn with_unmodeled(program: &str, unmodeled: Option<String>, read: Vec<Embedded>) -> Vec<Embedded> {
    match unmodeled {
        None => read,
        Some(why) => {
            let mut out = unreadable(program, &why);
            out.extend(read);
            out
        }
    }
}

/// `cmd [/q] [/d] [/s] [/v:on] [/c | /k] STRING`.
pub(super) fn cmd(args: &[String]) -> Vec<Embedded> {
    let script = match cmd_script_text(args) {
        Ok(script) => script,
        Err(found) => return found,
    };
    // The best-effort reading of an unmodeled script. `/s` strips one pair
    // of outer quotes; reading without it treats the quoted text as one
    // command, which is how cmd reads most such lines too.
    let text = script.text.trim();
    let text = text
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .filter(|inner| !inner.contains('"') || script.strip)
        .unwrap_or(text);
    with_unmodeled("cmd", script.unmodeled, cmd_script(text, script.delayed))
}

/// A `cmd` script as the reader checks it.
struct CmdScript {
    /// The arguments after `/c` joined with spaces, untrimmed: the text
    /// cmd runs when [`CmdScript::unmodeled`] is `None`.
    text: String,
    /// `/v:on`: `!NAME!` expands.
    delayed: bool,
    /// `/s`: cmd always strips one pair of outer quotes.
    strip: bool,
    /// Why cmd may read its command line differently from `text`.
    unmodeled: Option<String>,
}

/// The script of `cmd ... /c SCRIPT`, or the reading when there is none.
///
/// cmd does not split its command line by the MSVC rules the Windows
/// backend quotes it for: it takes the text after `/c`, applies its own
/// quote rule (`cmd /?`), then its own escapes, variables and delimiters.
/// So a script is checked as text only where that text is certainly what
/// cmd runs: no character cmd treats specially beyond the operators the
/// reader splits on and the redirections it strips, and no quote on the
/// command line except one pair cmd is certain to strip. Anything else is
/// unmodeled.
fn cmd_script_text(args: &[String]) -> Result<CmdScript, Vec<Embedded>> {
    let mut delayed = false;
    let mut strip = false;
    let mut unmodeled = args.iter().find_map(|a| cmd_unmodeled_char(a));
    for (i, a) in args.iter().enumerate() {
        let lower = a.to_ascii_lowercase();
        if lower == "/c" || lower == "/k" || lower == "/r" {
            let tail = &args[i + 1..];
            let text = tail.join(" ");
            if text.trim().is_empty() {
                return Err(unreadable(
                    "cmd",
                    "with an empty `/c` command runs nothing kGraf can read",
                ));
            }
            unmodeled = unmodeled.or_else(|| cmd_unmodeled_quoting(tail, strip));
            return Ok(CmdScript {
                text,
                delayed,
                strip,
                unmodeled,
            });
        }
        if lower.starts_with('/') {
            if lower.starts_with("/v:on") {
                delayed = true;
            }
            if lower == "/s" {
                strip = true;
            }
            if !is_plain_cmd_switch(&lower) {
                unmodeled = unmodeled.or_else(|| {
                    Some(format!(
                        "has the switch `{}`, and kGraf does not model how cmd reads it",
                        a.escape_debug()
                    ))
                });
            }
            continue;
        }
        // A word that is not an option before `/c`: cmd runs it as a
        // command line.
        break;
    }
    Err(unreadable(
        "cmd",
        "with no `/c` or `/k` command reads its commands from its input",
    ))
}

/// A switch of the form `/X` or `/X:VALUE` (one letter, a value of letters
/// and digits): `/q`, `/d`, `/s`, `/v:on`, `/e:off`, `/t:0a`. These hold no
/// second `/` that cmd could read as `/c`.
fn is_plain_cmd_switch(lower: &str) -> bool {
    let Some(rest) = lower.strip_prefix('/') else {
        return false;
    };
    let mut chars = rest.chars();
    let letter = chars.next().is_some_and(|c| c.is_ascii_alphabetic());
    let value = chars.as_str();
    letter
        && (value.is_empty()
            || value
                .strip_prefix(':')
                .is_some_and(|v| !v.is_empty() && v.chars().all(|c| c.is_ascii_alphanumeric())))
}

/// Why cmd may read an argument holding this character differently from
/// the reader: a quote (cmd's own quote rule, and the quotes the backend's
/// MSVC quoting adds), `^` (escape), `%` and `!` (variables), parentheses
/// (groups and `if`/`for` clauses), a line break or another control
/// character, and any character outside ASCII (cmd's code page and its
/// own delimiters).
fn cmd_unmodeled_char(arg: &str) -> Option<String> {
    let c = arg.chars().find(|&c| {
        matches!(c, '"' | '^' | '%' | '!' | '(' | ')')
            || (c.is_control() && c != '\t')
            || !c.is_ascii()
    })?;
    Some(if c.is_control() {
        "holds a line break or a control character, which cmd reads by rules kGraf does not model"
            .to_owned()
    } else if c.is_ascii() {
        format!("holds `{c}`, which cmd reads by rules kGraf does not model")
    } else {
        "holds a character outside ASCII, which cmd reads by rules kGraf does not model".to_owned()
    })
}

/// Why the quotes the backend adds around an argument with a space would
/// change the text cmd runs. cmd strips the first and the last quote of the
/// text after `/c`, except that it keeps both when (with no `/s`) they are
/// the only two, enclose a space and no `&<>()@^|`, and name an existing
/// executable file: a state of the disk the reader cannot see.
fn cmd_unmodeled_quoting(tail: &[String], strip: bool) -> Option<String> {
    let quoted = |a: &String| a.is_empty() || a.contains([' ', '\t']);
    if !tail.iter().any(quoted) {
        return None;
    }
    if tail.len() > 1 {
        return Some(
            "has an argument with a space beside other arguments, so cmd sees the quotes \
             around it and runs a different text than kGraf reads"
                .to_owned(),
        );
    }
    let only = &tail[0];
    if only.ends_with('\\') {
        return Some(
            "ends with a backslash inside quotes, which the command line doubles and cmd keeps"
                .to_owned(),
        );
    }
    if !strip && !only.contains(['&', '<', '>', '(', ')', '@', '^', '|']) {
        return Some(
            "is one quoted string with no operator, which cmd runs as a program name when a \
             file of that name exists; give the words as separate arguments or add `/s`"
                .to_owned(),
        );
    }
    None
}

/// cmd's built-in commands. cmd recognizes one when its name is followed
/// by any of `.\:+[]` as well as by a space (`cd\`, `echo.`), so a word
/// such as `rd\x` runs `rd`.
const CMD_BUILTINS: &[&str] = &[
    "assoc", "break", "call", "cd", "chdir", "cls", "color", "copy", "date", "del", "dir", "dpath",
    "echo", "endlocal", "erase", "exit", "for", "ftype", "goto", "if", "keys", "md", "mkdir",
    "mklink", "move", "path", "pause", "popd", "prompt", "pushd", "rd", "rem", "ren", "rename",
    "rmdir", "set", "setlocal", "shift", "start", "time", "title", "type", "ver", "verify", "vol",
];

/// Why cmd may run a different program than the word the reader names:
/// cmd also ends a command name at `,`, `;`, `=` and `/`, and finds a
/// built-in before `.\:+[]`.
fn cmd_program_unmodeled(program: &str) -> Option<String> {
    if program.contains([',', ';', '=', '/']) {
        return Some(format!(
            "runs `{}`, a name cmd ends at `,`, `;`, `=` or `/`",
            program.escape_debug()
        ));
    }
    let at = program.find(['.', '\\', ':', '+', '[', ']'])?;
    let prefix = program[..at].to_ascii_lowercase();
    (at > 0 && CMD_BUILTINS.contains(&prefix.as_str())).then(|| {
        format!(
            "runs `{}`, which cmd reads as its built-in `{prefix}`",
            program.escape_debug()
        )
    })
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
        if let Some(why) = cmd_program_unmodeled(&program) {
            out.push(Embedded::Unreadable(format!("`cmd` {why}")));
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
    match ps_script_text(program, args) {
        Ok(script) => with_unmodeled(program, script.unmodeled, ps_script(program, &script.text)),
        Err(found) => found,
    }
}

/// A PowerShell script as the reader checks it.
struct PsScript {
    /// The arguments after `-Command` (or the plain arguments) joined with
    /// spaces, as PowerShell joins them, or the decoded `-EncodedCommand`.
    text: String,
    /// Why PowerShell may read the script differently from `text`.
    unmodeled: Option<String>,
}

/// The script of a `powershell` or `pwsh` call, or the reading when there
/// is none.
///
/// PowerShell's own parser handles quotes, escapes, variables, script
/// blocks and expressions that the reader only approximates, and its host
/// splits the command line with its own runtime. So a script is checked as
/// text only where it holds none of those (see [`ps_unmodeled_text`]) and
/// every argument survives the backend's quoting unchanged (no quote, no
/// backslash before an added closing quote). Anything else is unmodeled.
fn ps_script_text(program: &str, args: &[String]) -> Result<PsScript, Vec<Embedded>> {
    let args_unmodeled = args.iter().find_map(|a| ps_unmodeled_arg(a));
    let script = |text: String| {
        let unmodeled = args_unmodeled.clone().or_else(|| ps_unmodeled_text(&text));
        PsScript { text, unmodeled }
    };
    let mut i = 0;
    while let Some(a) = args.get(i) {
        if !(a.starts_with('-') || (a.starts_with('/') && a.len() > 1)) {
            break;
        }
        match ps_option(a) {
            Some(PsOption::Command) => {
                let text = args[i + 1..].join(" ");
                if text.trim() == "-" {
                    return Err(unreadable(program, "reads its commands from its input"));
                }
                if text.trim().is_empty() {
                    return Err(Vec::new());
                }
                return Ok(script(text));
            }
            Some(PsOption::Encoded) => {
                let Some(b64) = args.get(i + 1) else {
                    return Err(unreadable(program, "names no encoded command"));
                };
                return match decode_utf16le_base64(b64) {
                    Some(text) => Ok(script(text)),
                    None => Err(unreadable(
                        program,
                        "has an encoded command that does not decode",
                    )),
                };
            }
            Some(PsOption::File) => {
                return Err(unreadable(
                    program,
                    "runs a script file, which kGraf does not read",
                ));
            }
            Some(PsOption::Interactive) => {
                return Err(unreadable(
                    program,
                    "stays open reading commands from its input",
                ));
            }
            Some(PsOption::Valued) => i += 2,
            Some(PsOption::Flag) => i += 1,
            None => {
                return Err(unreadable(
                    program,
                    &format!(
                        "has the option `{}`, which kGraf cannot place",
                        a.escape_debug()
                    ),
                ));
            }
        }
    }
    match args.get(i..) {
        // `pwsh FILE`: the first plain argument is a script file.
        Some(rest) if !rest.is_empty() && program == "pwsh" => Err(unreadable(
            program,
            "runs a script file, which kGraf does not read",
        )),
        Some(rest) if !rest.is_empty() => Ok(script(rest.join(" "))),
        _ => Err(unreadable(
            program,
            "with no command reads its commands from its input",
        )),
    }
}

/// The character in `text` PowerShell reads by rules the reader does not
/// model: a quote (ASCII or the typographic quotes PowerShell also
/// accepts), a backtick (escape), `$` (variables, subexpressions), `@`
/// (splatting, arrays, here-strings), braces (script blocks), parentheses
/// (expressions), `#` (comments), `<` (reserved), a control character
/// other than a tab or a line break, and any character outside ASCII
/// (PowerShell reads typographic dashes as `-` and more spaces than the
/// reader splits on).
fn ps_unmodeled_char(text: &str) -> Option<String> {
    let c = text.chars().find(|&c| {
        matches!(
            c,
            '\'' | '"' | '`' | '$' | '@' | '{' | '}' | '(' | ')' | '#' | '<'
        ) || (c.is_control() && !matches!(c, '\t' | '\n' | '\r'))
            || !c.is_ascii()
    })?;
    Some(if c.is_control() {
        "holds a control character, which PowerShell reads by rules kGraf does not model".to_owned()
    } else if c.is_ascii() {
        format!("holds `{c}`, which PowerShell reads by rules kGraf does not model")
    } else {
        "holds a character outside ASCII, which PowerShell reads by rules kGraf does not model"
            .to_owned()
    })
}

/// Why PowerShell may receive an argument differently from how it was
/// given: an unmodeled character, or a backslash before the closing quote
/// the backend adds around an argument with a space (written doubled for
/// the MSVC rules, which a host's runtime may not share).
fn ps_unmodeled_arg(arg: &str) -> Option<String> {
    if let Some(why) = ps_unmodeled_char(arg) {
        return Some(why);
    }
    (arg.contains([' ', '\t']) && arg.ends_with('\\')).then(|| {
        "has an argument that ends with a backslash inside quotes, which PowerShell's host \
         may read differently"
            .to_owned()
    })
}

/// Why PowerShell may read a script text differently from the reader: an
/// unmodeled character ([`ps_unmodeled_char`]), or `&` that is neither a
/// word of its own (the call operator) nor part of `&&`: `&curl` calls
/// `curl`, where the reader would name a program `&curl`.
fn ps_unmodeled_text(text: &str) -> Option<String> {
    if let Some(why) = ps_unmodeled_char(text) {
        return Some(why);
    }
    text.split_whitespace()
        .any(|w| w != "&" && w.replace("&&", "").contains('&'))
        .then(|| {
            "holds `&` joined to other text, which PowerShell reads by rules kGraf does not \
             model"
                .to_owned()
        })
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
    // A word that starts with `.` is a member access or a number, except a
    // relative path (`.\tool.exe`, `../bin/tool`), which runs a program.
    let relative_path = ["./", ".\\", "../", "..\\"]
        .iter()
        .any(|p| text.starts_with(p));
    if first.quoted
        || text.parse::<f64>().is_ok()
        || (text.starts_with('.') && !relative_path)
        || text.starts_with('-')
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

    fn encoded(script: &str) -> String {
        use base64::Engine as _;
        let bytes: Vec<u8> = script
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    /// One case per cmd rule: each script is unreadable, and the reason
    /// comes first.
    #[test]
    fn cmd_scripts_cmd_may_read_differently_are_unreadable() {
        // (the reason's own words, the call)
        let cases: &[(&str, &[&str])] = &[
            ("holds `\"`", &["/c", "echo", "\"a\""]),
            ("holds `^`", &["/c", "dir", "a^b"]),
            ("holds `%`", &["/c", "echo", "x%y"]),
            ("holds `!`", &["/c", "echo", "a!b"]),
            ("holds `(`", &["/c", "echo", "a(b"]),
            ("holds `)`", &["/c", "echo", "a)b"]),
            ("line break", &["/c", "dir\necho a"]),
            ("line break", &["/c", "dir\recho a"]),
            ("control character", &["/c", "dir\u{b}a"]),
            ("outside ASCII", &["/c", "echo", "caf\u{e9}"]),
            ("has the switch `/q/d`", &["/q/d", "/c", "dir"]),
            ("beside other arguments", &["/c", "dir", "a b"]),
            ("one quoted string with no operator", &["/c", "dir /b"]),
            ("ends with a backslash", &["/c", "dir C:\\a & dir b\\"]),
            ("a name cmd ends at", &["/c", "dir,a"]),
            ("a name cmd ends at", &["/c", "dir;a"]),
            ("a name cmd ends at", &["/c", "dir=a"]),
            ("a name cmd ends at", &["/c", "tools/x.exe"]),
            ("its built-in `rd`", &["/c", "rd\\a"]),
            ("its built-in `echo`", &["/c", "echo.a"]),
        ];
        for (reason, words) in cases {
            let found = cmd(&argv(words));
            assert!(
                matches!(found.first(), Some(Embedded::Unreadable(why)) if why.contains(reason)),
                "{reason}: {words:?}: {found:?}"
            );
        }
    }

    /// Ordinary cmd scripts read as before: no unreadable entry and the same
    /// commands.
    #[test]
    fn plain_cmd_scripts_stay_readable_with_the_same_split() {
        for (words, want) in plain_cmd_cases() {
            let found = cmd(&argv(words));
            assert!(!unreadable_found(&found), "{words:?}: {found:?}");
            let want: Vec<Vec<String>> = want.iter().map(|c| argv(c)).collect();
            assert_eq!(argvs(&found), want, "{words:?}");
        }
    }

    type Case = (&'static [&'static str], &'static [&'static [&'static str]]);

    fn plain_cmd_cases() -> Vec<Case> {
        vec![
            (&["/c", "dir"], &[&["dir"]]),
            (
                &["/c", "cargo", "test", "&&", "echo", "done"],
                &[&["cargo", "test"], &["echo", "done"]],
            ),
            (
                &["/c", "cargo test && echo done"],
                &[&["cargo", "test"], &["echo", "done"]],
            ),
            (&["/c", "dir & curl x"], &[&["dir"], &["curl", "x"]]),
            (
                &["/k", "dir || echo none | findstr n"],
                &[&["dir"], &["echo", "none"], &["findstr", "n"]],
            ),
            (
                &["/c", "echo a 2>&1 | findstr a"],
                &[&["echo", "a"], &["findstr", "a"]],
            ),
            (&["/c", "dir > out.txt"], &[&["dir"]]),
            (&["/s", "/c", "dir /b"], &[&["dir", "/b"]]),
            (
                &["/d", "/q", "/v:off", "/c", "cargo", "build"],
                &[&["cargo", "build"]],
            ),
            (
                &["/c", "C:\\tools\\curl.exe", "x"],
                &[&["C:\\tools\\curl.exe", "x"]],
            ),
            (&["/c", "@echo", "off"], &[&["echo", "off"]]),
        ]
    }

    /// One case per PowerShell rule: each script is unreadable, and the
    /// reason comes first.
    #[test]
    fn powershell_scripts_powershell_may_read_differently_are_unreadable() {
        let enc_dollar = encoded("$x = curl x");
        let enc_quote = encoded("Write-Output 'a; b'");
        // (the reason's own words, the call)
        let cases: Vec<(&str, Vec<&str>)> = vec![
            ("holds `'`", vec!["-c", "Get-Item 'a'"]),
            ("holds `\"`", vec!["-c", "Get-Item", "\"a\""]),
            // A semicolon inside quotes: the quote is the reason.
            ("holds `'`", vec!["-c", "Write-Output 'a; b'"]),
            // Typographic quotes and dashes.
            ("outside ASCII", vec!["-c", "Get-Item \u{2018}a\u{2019}"]),
            ("outside ASCII", vec!["-c", "Get-Item \u{2013}Force"]),
            ("holds ```", vec!["-c", "Get-Item a`b"]),
            ("holds `$`", vec!["-c", "Get-Item", "$x"]),
            ("holds `@`", vec!["-c", "Get-Item @a"]),
            ("holds `{`", vec!["-c", "Get-Item a{"]),
            ("holds `}`", vec!["-c", "Get-Item a}"]),
            ("holds `(`", vec!["-c", "Get-Item a("]),
            ("holds `)`", vec!["-c", "Get-Item a)"]),
            ("holds `#`", vec!["-c", "Get-Item a # b"]),
            ("holds `<`", vec!["-c", "Get-Item < a"]),
            ("control character", vec!["-c", "Get-Item\u{b}a"]),
            ("`&` joined to other text", vec!["-c", "&tool x"]),
            ("ends with a backslash", vec!["-c", "dir C:\\a b\\"]),
            // An option value is an argument too.
            ("holds `'`", vec!["-ExecutionPolicy", "'x'", "-c", "dir"]),
            // The script as plain arguments.
            ("holds `'`", vec!["Get-Item", "'a'"]),
            // The decoded text of `-EncodedCommand`.
            ("holds `$`", vec!["-EncodedCommand", &enc_dollar]),
            ("holds `'`", vec!["-enc", &enc_quote]),
        ];
        for (reason, words) in &cases {
            let found = powershell("powershell", &argv(words));
            assert!(
                matches!(found.first(), Some(Embedded::Unreadable(why)) if why.contains(reason)),
                "{reason}: {words:?}: {found:?}"
            );
        }
    }

    fn plain_ps_cases() -> Vec<(Vec<String>, Vec<Vec<String>>)> {
        let to = |c: &[&[&str]]| c.iter().map(|w| argv(w)).collect::<Vec<_>>();
        vec![
            (
                argv(&["-c", "Get-Item x; curl.exe x | Out-Null"]),
                to(&[&["Get-Item", "x"], &["curl.exe", "x"], &["Out-Null"]]),
            ),
            (
                argv(&[
                    "-NoProfile",
                    "-Command",
                    "Get-ChildItem",
                    "|",
                    "Select-Object",
                    "-First",
                    "1",
                ]),
                to(&[&["Get-ChildItem"], &["Select-Object", "-First", "1"]]),
            ),
            (
                argv(&["-c", "cargo test && echo done"]),
                to(&[&["cargo", "test"], &["echo", "done"]]),
            ),
            (
                argv(&["-c", "& C:\\tools\\curl.exe x"]),
                to(&[&["C:\\tools\\curl.exe", "x"]]),
            ),
            (argv(&["Get-Item", "x"]), to(&[&["Get-Item", "x"]])),
            (
                argv(&["-EncodedCommand", &encoded("curl x")]),
                to(&[&["curl", "x"]]),
            ),
            (
                argv(&["-enc", &encoded("Get-Item a\r\ncurl x")]),
                to(&[&["Get-Item", "a"], &["curl", "x"]]),
            ),
        ]
    }

    /// Ordinary PowerShell scripts read as before: no unreadable entry and
    /// the same commands.
    #[test]
    fn plain_powershell_scripts_stay_readable_with_the_same_split() {
        for (words, want) in plain_ps_cases() {
            let found = powershell("powershell", &words);
            assert!(!unreadable_found(&found), "{words:?}: {found:?}");
            assert_eq!(argvs(&found), want, "{words:?}");
        }
    }

    /// A PowerShell statement that starts with a relative path runs that
    /// program; it is not a member access.
    #[test]
    fn a_powershell_relative_path_is_read_as_a_program() {
        for path in [".\\tool.exe", "./tool", "..\\bin\\tool.exe"] {
            let found = powershell("powershell", &argv(&["-c", &format!("{path} x")]));
            assert_eq!(argvs(&found), vec![argv(&[path, "x"])], "{path}");
        }
    }

    // ---- the text checked is the text run ---------------------------------

    /// The text after `/c` that cmd runs from `line`, by the rule `cmd /?`
    /// documents: it keeps the quotes when (with no `/s`) they are the only
    /// two, enclose whitespace and no `&<>()@^|`, and name an executable
    /// (assumed here, the worst case); otherwise it strips a leading quote
    /// and the last quote. `None` when the line has no `/c`.
    fn cmd_visible(line: &str) -> Option<String> {
        let rest = line.strip_prefix('"')?;
        let mut rest = &rest[rest.find('"')? + 1..];
        let mut strip = false;
        loop {
            rest = rest.trim_start_matches([' ', '\t']);
            let end = rest.find([' ', '\t']).unwrap_or(rest.len());
            let token = rest[..end].to_ascii_lowercase();
            if token.is_empty() {
                return None;
            }
            rest = &rest[end..];
            if matches!(token.as_str(), "/c" | "/k" | "/r") {
                break;
            }
            strip |= token == "/s";
        }
        let script = rest.trim_start_matches([' ', '\t']);
        let kept = !strip
            && script.matches('"').count() == 2
            && script.starts_with('"')
            && script[1..].split('"').next().is_some_and(|inner| {
                !inner.contains(['&', '<', '>', '(', ')', '@', '^', '|'])
                    && inner.contains([' ', '\t'])
            });
        if kept || !script.starts_with('"') {
            return Some(script.to_owned());
        }
        let inner = &script[1..];
        Some(match inner.rfind('"') {
            Some(last) => format!("{}{}", &inner[..last], &inner[last + 1..]),
            None => inner.to_owned(),
        })
    }

    /// `line` split by the MSVC runtime rules (2008 and later), the
    /// program word read up to its closing quote.
    fn msvc_split(line: &str) -> Vec<String> {
        let c: Vec<char> = line.chars().collect();
        let mut out = Vec::new();
        let mut i = 0;
        if c.first() == Some(&'"') {
            let end = c[1..]
                .iter()
                .position(|&x| x == '"')
                .map_or(c.len(), |p| p + 1);
            out.push(c[1..end].iter().collect());
            i = end + 1;
        }
        loop {
            while i < c.len() && matches!(c[i], ' ' | '\t') {
                i += 1;
            }
            if i >= c.len() {
                return out;
            }
            let (mut cur, mut quoted) = (String::new(), false);
            while i < c.len() {
                match c[i] {
                    '\\' => {
                        let run = c[i..].iter().take_while(|&&x| x == '\\').count();
                        i += run;
                        if c.get(i) == Some(&'"') {
                            cur.extend(std::iter::repeat_n('\\', run / 2));
                            if run % 2 == 1 {
                                cur.push('"');
                                i += 1;
                            }
                        } else {
                            cur.extend(std::iter::repeat_n('\\', run));
                        }
                    }
                    '"' if quoted && c.get(i + 1) == Some(&'"') => {
                        cur.push('"');
                        i += 2;
                    }
                    '"' => {
                        quoted = !quoted;
                        i += 1;
                    }
                    ' ' | '\t' if !quoted => break,
                    x => {
                        cur.push(x);
                        i += 1;
                    }
                }
            }
            out.push(cur);
        }
    }

    /// `line` split by the plainest rule a host could use: a quote toggles
    /// grouping and a space or a tab ends a word outside it; a backslash is
    /// literal.
    fn toggle_split(line: &str) -> Vec<String> {
        let (mut out, mut cur, mut quoted, mut started) = (Vec::new(), String::new(), false, false);
        for ch in line.chars() {
            match ch {
                '"' => {
                    quoted = !quoted;
                    started = true;
                }
                ' ' | '\t' if !quoted => {
                    if started {
                        out.push(std::mem::take(&mut cur));
                        started = false;
                    }
                }
                x => {
                    cur.push(x);
                    started = true;
                }
            }
        }
        if started {
            out.push(cur);
        }
        out
    }

    /// Check that an accepted cmd call's checked text is the text cmd runs
    /// from the backend's command line. `Some(false)` on a mismatch, `None`
    /// when the reader does not accept the call.
    fn cmd_text_matches(words: &[String]) -> Option<bool> {
        let script = cmd_script_text(words).ok()?;
        if script.unmodeled.is_some() || unreadable_found(&cmd(words)) {
            return None;
        }
        let line = kgraf_sandbox::windows_command_line(r"C:\Windows\System32\cmd.exe", words);
        let visible = cmd_visible(&line)?;
        Some(visible.trim_start() == script.text.trim_start())
    }

    /// Check that an accepted PowerShell call's arguments reach PowerShell
    /// unchanged from the backend's command line under both the MSVC rules
    /// and the plain toggle rule, so the joined text PowerShell runs is the
    /// text checked whatever runtime its host uses.
    fn ps_text_matches(words: &[String]) -> Option<bool> {
        let script = ps_script_text("powershell", words).ok()?;
        if script.unmodeled.is_some() || unreadable_found(&powershell("powershell", words)) {
            return None;
        }
        let line = kgraf_sandbox::windows_command_line(
            r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
            words,
        );
        let msvc = msvc_split(&line);
        let toggle = toggle_split(&line);
        Some(msvc[1..] == *words && toggle[1..] == *words)
    }

    #[test]
    fn every_accepted_plain_script_is_the_text_the_interpreter_runs() {
        for (words, _) in plain_cmd_cases() {
            assert_eq!(cmd_text_matches(&argv(words)), Some(true), "{words:?}");
        }
        for (words, _) in plain_ps_cases() {
            assert_eq!(ps_text_matches(&words), Some(true), "{words:?}");
        }
    }

    /// The same property over generated argument lists: every call the
    /// reader accepts, drawn from the characters each interpreter treats
    /// specially, runs the text checked. A fixed seed keeps it repeatable.
    #[test]
    fn generated_accepted_scripts_are_the_text_the_interpreter_runs() {
        const ALPHABET: &[char] = &[
            'a', 'b', 'x', ' ', ' ', '\t', '"', '\\', '&', '|', '^', '%', '!', '(', ')', '<', '>',
            '@', ';', ',', '=', '/', '$', '\'', '`', '{', '}', '#', '-', '.', ':', '\n',
        ];
        let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = |n: usize| {
            // xorshift64: deterministic, no dependency.
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % n as u64) as usize
        };
        let (mut cmd_accepted, mut ps_accepted) = (0, 0);
        for _ in 0..40_000 {
            let mut script: Vec<String> = (0..1 + next(3))
                .map(|_| {
                    (0..next(7))
                        .map(|_| ALPHABET[next(ALPHABET.len())])
                        .collect()
                })
                .collect();
            if script.iter().all(|w| w.trim().is_empty()) {
                script.insert(0, "dir".to_owned());
            }
            let mut words: Vec<String> = [&[][..], &["/s"], &["/d", "/q"], &["/v:on/s"]][next(4)]
                .iter()
                .map(|s| (*s).to_owned())
                .collect();
            words.push("/c".to_owned());
            words.extend(script.iter().cloned());
            if let Some(ok) = cmd_text_matches(&words) {
                assert!(ok, "cmd {words:?}");
                cmd_accepted += 1;
            }
            let mut words = argv(&["-NoProfile", "-c"]);
            words.extend(script);
            if let Some(ok) = ps_text_matches(&words) {
                assert!(ok, "powershell {words:?}");
                ps_accepted += 1;
            }
        }
        // The generator reaches the accepted forms, not only refusals.
        assert!(
            cmd_accepted > 1_000 && ps_accepted > 1_000,
            "{cmd_accepted} {ps_accepted}"
        );
    }

    /// `start` runs the program after its options; a quoted first word is a
    /// title only when a program follows it.
    #[test]
    fn cmd_start_drops_a_quoted_title_only_when_a_program_follows() {
        let run = |script: &str| cmd(&argv(&["/c", script]));
        assert_eq!(
            argvs(&run("start curl")),
            vec![argv(&["start", "curl"]), argv(&["curl"])]
        );
        assert_eq!(
            argvs(&run("start curl x")),
            vec![argv(&["start", "curl", "x"]), argv(&["curl", "x"])]
        );
        assert_eq!(
            argvs(&run("start /b curl x")),
            vec![argv(&["start", "/b", "curl", "x"]), argv(&["curl", "x"])]
        );
        // A lone quoted word is the program, not a title.
        assert_eq!(
            argvs(&run("start \"t\"")),
            vec![argv(&["start", "t"]), argv(&["t"])]
        );
        assert_eq!(
            argvs(&run("start \"t\" curl x")),
            vec![argv(&["start", "t", "curl", "x"]), argv(&["curl", "x"])]
        );
        assert!(unreadable_found(&run("start")));
        assert!(unreadable_found(&run("start /b")));
    }

    /// `call :label` runs a batch file's label, which is not read; `call
    /// PROGRAM` runs the program.
    #[test]
    fn cmd_call_runs_its_program_and_refuses_a_label() {
        let run = |script: &str| cmd(&argv(&["/c", script]));
        let label = run("call :end");
        assert!(unreadable_found(&label));
        assert!(argvs(&label).is_empty(), "a label is no program: {label:?}");
        assert_eq!(argvs(&run("call curl x")), vec![argv(&["curl", "x"])]);
        assert!(argvs(&run("call")).is_empty());
    }

    /// Quotes keep a `^` as text; outside quotes it escapes the next character.
    #[test]
    fn cmd_words_keep_a_caret_inside_quotes() {
        assert_eq!(
            cmd_words("echo \"a^b\" c^ d e^^f"),
            argv(&["echo", "a^b", "c d", "e^f"])
        );
    }

    /// A redirection and its target leave the command's words; a target
    /// written after the operator is its own word, one joined to it is not.
    #[test]
    fn cmd_redirections_take_only_their_own_target() {
        let run = |script: &str| argvs(&cmd(&argv(&["/c", script])));
        for script in [
            "dir >out more",
            "dir > out more",
            "dir >> log more",
            "dir 2>&1 more",
            "dir >&2 more",
            "dir <in more",
            "dir < in more",
            "dir 2> err more",
        ] {
            assert_eq!(run(script), vec![argv(&["dir", "more"])], "{script}");
        }
        assert_eq!(run("dir more >out"), vec![argv(&["dir", "more"])]);
    }

    /// Every PowerShell option name, in the spellings PowerShell accepts
    /// (any unambiguous prefix, either dash or a slash, any case), reads as
    /// its kind; an ambiguous or unknown name reads as none.
    #[test]
    fn each_powershell_option_name_has_its_kind() {
        use PsOption::*;
        for (kind, names) in [
            (
                Command,
                &[
                    "c",
                    "com",
                    "comm",
                    "command",
                    "commandw",
                    "commandwithargs",
                    "cwa",
                ][..],
            ),
            (Encoded, &["e", "ec", "en", "enc", "encodedcommand"][..]),
            (File, &["f", "fi", "file"][..]),
            (
                Valued,
                &[
                    "ex",
                    "ep",
                    "exe",
                    "executionpolicy",
                    "w",
                    "wi",
                    "windowstyle",
                    "if",
                    "inp",
                    "inputformat",
                    "o",
                    "of",
                    "ou",
                    "outputformat",
                    "v",
                    "ve",
                    "version",
                    "config",
                    "con",
                    "configurationname",
                    "psc",
                    "psconsolefile",
                    "wd",
                    "wor",
                    "workingdirectory",
                    "set",
                    "settingsfile",
                    "cus",
                    "custompipename",
                    "ea",
                    "encodedar",
                    "encodedarguments",
                ][..],
            ),
            (
                Interactive,
                &["noe", "noexit", "i", "int", "interactive"][..],
            ),
            (
                Flag,
                &[
                    "nop",
                    "nopr",
                    "noprofile",
                    "noni",
                    "nonint",
                    "noninteractive",
                    "nol",
                    "nolo",
                    "nologo",
                    "mta",
                    "sta",
                    "l",
                    "lo",
                    "login",
                    "?",
                    "h",
                    "he",
                    "help",
                    "noprofilel",
                    "noprofileloadtime",
                ][..],
            ),
        ] {
            for name in names {
                for spelled in [
                    format!("-{name}"),
                    format!("/{name}"),
                    format!("-{}", name.to_ascii_uppercase()),
                ] {
                    assert!(
                        ps_option(&spelled) == Some(kind),
                        "{spelled} is not the expected kind"
                    );
                }
            }
        }
        for name in ["-", "-zz", "-co", "-in", "-n", "-no", "-encodeda", "-noni1"] {
            assert!(ps_option(name).is_none(), "{name}");
        }
    }
}

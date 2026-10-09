//! Every process creation in kGraf takes the spawn lock (see
//! [`super::spawn_locked`]). This test reads the source of every crate in
//! the workspace and finds each call of `.spawn()`, `.output()` or
//! `.status()` outside test code. Each must go through the lock's helpers
//! (`child_run::run`, `spawn`, `output`, `status`, or a closure passed to
//! `spawn_locked`), or be listed in [`ALLOWED`] with the number of such calls
//! in that file and why they cannot run beside a sandboxed command's start on
//! Windows. A new direct spawn fails the test until it is routed or listed;
//! a listed call that goes away fails it too, so the list stays exact.
//!
//! Test code is left out: files named `*tests.rs`, the `tests`, `benches`
//! and `examples` folders, and inline modules under a `#[cfg(...test...)]`
//! attribute. Comments and the contents of string and character literals
//! are blanked before the search, so text that only names a call does not
//! count.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Direct calls that do not take the spawn lock: (path below `crates/`,
/// calls in that file, why).
const ALLOWED: &[(&str, usize, &str)] = &[
    (
        "kgraf-core/src/child_run.rs",
        1,
        "`run` takes the spawn lock itself, with the caller's bound",
    ),
    (
        "kgraf-core/src/flock.rs",
        1,
        "test-support only (`Forkers`, Unix), for tests of fork-time descriptor copies",
    ),
    (
        "kgraf-sandbox/src/sandbox/mod.rs",
        3,
        "the sandboxed command's own spawn, output and status on Unix (`#[cfg(not(windows))]`); \
         on Windows they go to `windows::spawn`, which takes the lock (`InheritWindow`)",
    ),
    (
        "kgraf-sandbox/src/egress.rs",
        1,
        "Unix only: `SandboxedCommand::spawn`",
    ),
    (
        "kgraf-sandbox/src/build_dir.rs",
        1,
        "`cp` on macOS and Linux only; returns before the spawn on Windows",
    ),
    (
        "kgraf-tools/src/shell.rs",
        2,
        "Unix: tokio spawn of the sandboxed command's Unix parts (`run_sandboxed`, \
         `#[cfg(unix)]`); Windows: `SandboxedCommand::spawn`, which takes the lock",
    ),
    (
        "kgraf-orchestrator/src/check_runner.rs",
        2,
        "Unix: tokio spawn of the sandboxed check (`#[cfg(unix)]`); Windows: \
         `SandboxedCommand::spawn`, which takes the lock",
    ),
    (
        "kgraf-cli/src/sandbox_cmd.rs",
        1,
        "`SandboxedCommand::spawn` (selfcheck), which takes the lock on Windows",
    ),
    (
        "kgraf-host/src/gates.rs",
        1,
        "`SandboxedCommand::spawn` (a gate check), which takes the lock on Windows",
    ),
    (
        "kgraf-host/src/deps/install.rs",
        1,
        "`SandboxedCommand::spawn` (a dependency install), which takes the lock on Windows",
    ),
    (
        "kgraf-host/src/subagent_e2e.rs",
        1,
        "test code: included only from a `#[cfg(test)]` module of workspaces.rs",
    ),
    (
        "kgraf-bench-briefs/src/check.rs",
        1,
        "`SandboxedCommand::spawn` (a benchmark check), which takes the lock on Windows",
    ),
];

/// Crates not read, and why.
const SKIPPED_CRATES: &[(&str, &str)] = &[(
    "kgraf-wintest",
    "the Windows self-test runner: a separate program that starts test binaries and kgraf; \
     it runs no sandboxed command in its own process",
)];

/// Receivers whose `.status()` is an HTTP status, not a process.
const NOT_A_PROCESS: &[&str] = &[
    "response", "resp", "http", "inner", "e", "self", "proj", "reply", "err",
];

/// `source` with comments and the contents of string and character literals
/// replaced by spaces, newlines kept, so lines and braces still line up.
fn blank_literals(source: &str) -> String {
    let c: Vec<char> = source.chars().collect();
    let mut out = String::with_capacity(source.len());
    let blank = |ch: char| if ch == '\n' { '\n' } else { ' ' };
    let mut i = 0;
    while i < c.len() {
        let rest = |k: usize| c.get(i + k).copied();
        match c[i] {
            '/' if rest(1) == Some('/') => {
                while i < c.len() && c[i] != '\n' {
                    out.push(' ');
                    i += 1;
                }
            }
            '/' if rest(1) == Some('*') => {
                let mut depth = 0;
                while i < c.len() {
                    if c[i] == '/' && c.get(i + 1) == Some(&'*') {
                        depth += 1;
                        out.push_str("  ");
                        i += 2;
                    } else if c[i] == '*' && c.get(i + 1) == Some(&'/') {
                        depth -= 1;
                        out.push_str("  ");
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        out.push(blank(c[i]));
                        i += 1;
                    }
                }
            }
            // Raw strings: r"..", r#".."#, br"..", with any number of `#`.
            'r' | 'b'
                if {
                    let start = if c[i] == 'b' && rest(1) == Some('r') {
                        2
                    } else {
                        1
                    };
                    let prev_ident = i > 0 && (c[i - 1].is_alphanumeric() || c[i - 1] == '_');
                    let mut k = i + start;
                    while c.get(k) == Some(&'#') {
                        k += 1;
                    }
                    !prev_ident && (c[i] == 'r' || rest(1) == Some('r')) && c.get(k) == Some(&'"')
                } =>
            {
                let start = if c[i] == 'b' { 2 } else { 1 };
                for _ in 0..start {
                    out.push(c[i]);
                    i += 1;
                }
                let mut hashes = 0;
                while c[i] == '#' {
                    out.push('#');
                    hashes += 1;
                    i += 1;
                }
                out.push('"');
                i += 1;
                while i < c.len() {
                    if c[i] == '"' && (1..=hashes).all(|h| c.get(i + h) == Some(&'#')) {
                        out.push('"');
                        i += 1;
                        for _ in 0..hashes {
                            out.push('#');
                            i += 1;
                        }
                        break;
                    }
                    out.push(blank(c[i]));
                    i += 1;
                }
            }
            '"' => {
                out.push('"');
                i += 1;
                while i < c.len() && c[i] != '"' {
                    if c[i] == '\\' {
                        out.push(' ');
                        i += 1;
                    }
                    if i < c.len() {
                        out.push(blank(c[i]));
                        i += 1;
                    }
                }
                out.push('"');
                i += 1;
            }
            '\'' => {
                // A character literal ('x', '\n', '\'', '\u{7f}') or a
                // lifetime ('a), which is code.
                let end = if rest(1) == Some('\\') {
                    (i + 2..c.len()).find(|&k| c[k] == '\'' && k > i + 2)
                } else if rest(2) == Some('\'') {
                    Some(i + 2)
                } else {
                    None
                };
                match end {
                    Some(end) => {
                        out.push('\'');
                        for _ in i + 1..end {
                            out.push(' ');
                        }
                        out.push('\'');
                        i = end + 1;
                    }
                    None => {
                        out.push('\'');
                        i += 1;
                    }
                }
            }
            ch => {
                out.push(ch);
                i += 1;
            }
        }
    }
    out
}

/// Whether a blanked line is a `cfg` attribute that includes `test`.
fn is_test_cfg(line: &str) -> bool {
    let t = line.trim();
    t.starts_with("#[cfg(")
        && !t.contains("not(test")
        && t.split(|ch: char| !ch.is_alphanumeric() && ch != '_')
            .any(|w| w == "test")
}

/// Whether a blanked line opens an inline module.
fn opens_inline_mod(line: &str) -> bool {
    let t = line.trim();
    let t = t
        .strip_prefix("pub(crate) ")
        .or_else(|| t.strip_prefix("pub(super) "))
        .or_else(|| t.strip_prefix("pub "))
        .unwrap_or(t);
    t.starts_with("mod ") && t.contains('{')
}

/// The lines of blanked source outside test-only inline modules, with their
/// line numbers (from 1).
fn live_lines(blanked: &str) -> Vec<(usize, &str)> {
    let lines: Vec<&str> = blanked.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if is_test_cfg(lines[i]) {
            // Further attributes may sit between the cfg and the module.
            let mut j = i + 1;
            while j < lines.len() && lines[j].trim_start().starts_with("#[") {
                j += 1;
            }
            if j < lines.len() && opens_inline_mod(lines[j]) {
                let mut depth: i64 = 0;
                while j < lines.len() {
                    depth += lines[j].matches('{').count() as i64;
                    depth -= lines[j].matches('}').count() as i64;
                    j += 1;
                    if depth <= 0 {
                        break;
                    }
                }
                i = j;
                continue;
            }
        }
        out.push((i + 1, lines[i]));
        i += 1;
    }
    out
}

/// The byte ranges of blanked source inside the parentheses of a
/// `spawn_locked(...)` call.
fn locked_ranges(blanked: &str) -> Vec<std::ops::Range<usize>> {
    const CALL: &str = "spawn_locked(";
    let bytes = blanked.as_bytes();
    let mut out = Vec::new();
    for (at, _) in blanked.match_indices(CALL) {
        let open = at + CALL.len();
        let mut depth = 1;
        let mut k = open;
        while k < bytes.len() && depth > 0 {
            match bytes[k] {
                b'(' => depth += 1,
                b')' => depth -= 1,
                _ => {}
            }
            k += 1;
        }
        out.push(open..k);
    }
    out
}

/// The direct spawn calls in `source` outside test code: (line, text).
fn direct_spawns(source: &str) -> Vec<(usize, String)> {
    let blanked = blank_literals(source);
    let locked = locked_ranges(&blanked);
    // Byte offset of the start of each line (`str::lines` splits on `\n`;
    // a `\r` before it stays at the line's end).
    let starts: Vec<usize> = std::iter::once(0)
        .chain(blanked.match_indices('\n').map(|(i, _)| i + 1))
        .collect();
    let mut found = Vec::new();
    for (n, line) in live_lines(&blanked) {
        for call in [".spawn()", ".output()", ".status()"] {
            for (at, _) in line.match_indices(call) {
                let receiver: String = line[..at]
                    .trim_end()
                    .chars()
                    .rev()
                    .take_while(|ch| ch.is_alphanumeric() || *ch == '_')
                    .collect::<Vec<char>>()
                    .into_iter()
                    .rev()
                    .collect();
                if call == ".status()" && NOT_A_PROCESS.contains(&receiver.as_str()) {
                    continue;
                }
                // A call inside a closure passed to `spawn_locked`.
                let offset = starts[n - 1] + at;
                if locked.iter().any(|r| r.contains(&offset)) {
                    continue;
                }
                found.push((n, line.trim().to_owned()));
            }
        }
    }
    found
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            if !matches!(name.as_str(), "target" | "tests" | "benches" | "examples") {
                rust_files(&path, out);
            }
        } else if name.ends_with(".rs") && !name.ends_with("tests.rs") {
            out.push(path);
        }
    }
}

#[test]
fn every_spawn_takes_the_spawn_lock_or_is_listed_with_its_reason() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the crates folder")
        .to_path_buf();
    let mut found: BTreeMap<String, Vec<(usize, String)>> = BTreeMap::new();
    let mut scanned = 0;
    for entry in std::fs::read_dir(&crates).expect("read crates").flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if SKIPPED_CRATES.iter().any(|(c, _)| *c == name) {
            continue;
        }
        let mut files = Vec::new();
        rust_files(&entry.path().join("src"), &mut files);
        for file in files {
            let source = std::fs::read_to_string(&file).expect("read source");
            scanned += 1;
            let calls = direct_spawns(&source);
            if !calls.is_empty() {
                let rel = file
                    .strip_prefix(&crates)
                    .expect("below crates")
                    .to_string_lossy()
                    .replace('\\', "/");
                found.insert(rel, calls);
            }
        }
    }
    assert!(scanned > 100, "only {scanned} files read below {crates:?}");
    let mut problems = Vec::new();
    for (file, calls) in &found {
        let allowed = ALLOWED
            .iter()
            .find(|(f, _, _)| f == file)
            .map_or(0, |(_, n, _)| *n);
        if calls.len() != allowed {
            problems.push(format!(
                "{file}: {} direct spawn call(s), {allowed} listed: {calls:?}",
                calls.len()
            ));
        }
    }
    for (file, n, _) in ALLOWED {
        // A listed file that no longer exists (a crate left out of a copy of
        // the tree) is skipped; one that exists must still hold its calls.
        if crates.join(file).exists() && !found.contains_key(*file) {
            problems.push(format!("{file}: listed with {n} call(s), none found"));
        }
    }
    assert!(
        problems.is_empty(),
        "route each spawn through kgraf_core::child_run (spawn, output, status, \
         spawn_locked or run), or list it in ALLOWED with why it cannot overlap a \
         sandboxed command's start:\n{}",
        problems.join("\n")
    );
}

#[test]
fn the_scan_sees_calls_in_code_and_not_in_tests_comments_or_strings() {
    let source = r##"
fn real(cmd: &mut Command) {
    let a = cmd.spawn();
    let b = Command::new("git")
        .output();
    let c = response.status();
    // cmd.output() in a comment
    let d = "cmd.status() in a string";
    let e = r#"x.spawn() in a raw string, with a quote " inside"#;
    let f = '"';
    let g = kgraf_core::child_run::spawn_locked(|| {
        tokio_cmd.spawn()
    });
    let h = c.status();
}

#[cfg(test)]
mod tests {
    fn t() {
        let s = "{";
        Command::new("x").output();
    }
}

fn after_tests(cmd: &mut Command) {
    cmd.status();
}
"##;
    let got: Vec<usize> = direct_spawns(source).into_iter().map(|(n, _)| n).collect();
    assert_eq!(got, vec![3, 5, 14, 26], "{:?}", direct_spawns(source));
}

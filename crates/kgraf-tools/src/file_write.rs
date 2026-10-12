//! Recognizes a shell call whose only effect is writing one file inside the
//! worktree, so a refusal of it can point at `apply_patch`, which writes that
//! file with no command permission.
//!
//! The rule is conservative. A call is a pure file write only when it is:
//!
//! - `sh -c` or `bash -c` with a script that is one of
//!   `cat > PATH << DELIM` (the redirection and the heredoc in either order)
//!   followed by the body and the closing `DELIM` line, or one line of
//!   `cat > PATH`, `printf ... > PATH` or `echo ... > PATH`; or
//! - `tee PATH`;
//!
//! and `PATH` is a plain relative path with no `..`, or an absolute path under
//! the worktree root. Any other shell syntax (a second command, a pipe, an
//! expansion, an append, text after the closing line) makes it not a pure
//! write, and the call is treated as before.

use serde_json::Value;
use std::path::{Component, Path};

/// What a pure file-write call writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileWrite {
    /// The path relative to the worktree, with `/` separators.
    pub path: String,
    /// Whether a file is there now (a patch must delete it before it adds it).
    pub exists: bool,
}

/// The file `input` (a shell call's input) writes, if the call is a pure
/// file write under `root` (see the module docs).
pub fn pure_file_write(input: &Value, root: &Path) -> Option<FileWrite> {
    let argv = crate::shell::command_argv(input)?;
    let path = match argv.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["tee", path] => path.to_owned(),
        [shell, "-c", script] if matches!(basename(shell), "sh" | "bash") => script_target(script)?,
        _ => return None,
    };
    let rel = relative_in(&path, root)?;
    let exists = root.join(&rel).is_file();
    Some(FileWrite { path: rel, exists })
}

/// The text that tells a worker refused a command that only writes a file to
/// write it with `apply_patch`, with the whole-file patch for that path.
pub fn file_write_hint(write: &FileWrite) -> String {
    format!(
        "\nThat command only writes `{path}`. Write it with apply_patch instead, which needs no \
         shell:\n{skeleton}",
        path = write.path,
        skeleton = crate::apply_patch::whole_file_skeleton(&write.path, write.exists)
    )
}

fn basename(program: &str) -> &str {
    program.rsplit('/').next().unwrap_or(program)
}

/// `path` as a worktree-relative path, or `None` if it is not plain or leaves
/// the worktree.
fn relative_in(path: &str, root: &Path) -> Option<String> {
    if path.is_empty()
        || !path
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '/' | '-' | '+' | '@'))
    {
        return None;
    }
    let given = Path::new(path);
    let rel = if given.is_absolute() {
        let canonical = kgraf_core::plain_path::canonicalize(root).ok();
        given
            .strip_prefix(root)
            .ok()
            .or_else(|| {
                canonical
                    .as_deref()
                    .and_then(|c| given.strip_prefix(c).ok())
            })?
            .to_path_buf()
    } else {
        given.to_path_buf()
    };
    let mut parts = Vec::new();
    for component in rel.components() {
        match component {
            Component::Normal(p) => parts.push(p.to_str()?.to_owned()),
            Component::CurDir => {}
            _ => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

#[derive(Debug, PartialEq)]
enum Tok {
    Word(String),
    /// `>`
    Out,
    /// `<<` or `<<-` (the flag) and its delimiter.
    Here {
        strip_tabs: bool,
        delim: String,
    },
}

/// One word at `*i`: plain characters and quoted segments. `None` on any
/// shell syntax a pure write does not use (expansions, separators, globs).
fn word(chars: &[char], i: &mut usize) -> Option<String> {
    let mut text = String::new();
    let mut any = false;
    while *i < chars.len() {
        let c = chars[*i];
        match c {
            '\'' => {
                let end = chars[*i + 1..].iter().position(|&q| q == '\'')? + *i + 1;
                text.extend(&chars[*i + 1..end]);
                *i = end + 1;
            }
            '"' => {
                let end = chars[*i + 1..].iter().position(|&q| q == '"')? + *i + 1;
                let inner = &chars[*i + 1..end];
                if inner.iter().any(|q| matches!(q, '$' | '`' | '\\')) {
                    return None;
                }
                text.extend(inner);
                *i = end + 1;
            }
            c if c.is_whitespace() || matches!(c, '<' | '>') => break,
            ';' | '&' | '|' | '(' | ')' | '`' | '$' | '\\' | '{' | '}' | '*' | '?' | '[' | '#'
            | '!' | '~' => return None,
            c => {
                text.push(c);
                *i += 1;
            }
        }
        any = true;
    }
    any.then_some(text)
}

/// The tokens of one line, or `None` on any shell syntax beyond words, quotes,
/// one `>` and a heredoc operator.
fn tokens(line: &str) -> Option<Vec<Tok>> {
    let chars: Vec<char> = line.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '>' {
            if matches!(chars.get(i + 1), Some('>' | '&')) {
                return None;
            }
            out.push(Tok::Out);
            i += 1;
        } else if c == '<' {
            if chars.get(i + 1) != Some(&'<') || chars.get(i + 2) == Some(&'<') {
                return None;
            }
            i += 2;
            let strip_tabs = chars.get(i) == Some(&'-');
            if strip_tabs {
                i += 1;
            }
            while chars.get(i).is_some_and(|c| c.is_whitespace()) {
                i += 1;
            }
            let delim = word(&chars, &mut i)?;
            out.push(Tok::Here { strip_tabs, delim });
        } else {
            out.push(Tok::Word(word(&chars, &mut i)?));
        }
    }
    Some(out)
}

/// The path a pure-write script writes, or `None`.
fn script_target(script: &str) -> Option<String> {
    let (first, rest) = match script.split_once('\n') {
        Some((first, rest)) => (first, Some(rest)),
        None => (script, None),
    };
    let toks = tokens(first)?;
    let here = toks.iter().find_map(|t| match t {
        Tok::Here { strip_tabs, delim } => Some((*strip_tabs, delim.as_str())),
        _ => None,
    });
    let outs: Vec<usize> = toks
        .iter()
        .enumerate()
        .filter_map(|(i, t)| (*t == Tok::Out).then_some(i))
        .collect();
    let [out] = outs[..] else { return None };
    let Some(Tok::Word(path)) = toks.get(out + 1) else {
        return None;
    };
    let Some(Tok::Word(program)) = toks.first() else {
        return None;
    };
    match here {
        Some((strip_tabs, delim)) => {
            // `cat`, the redirection, the heredoc: nothing else on the line.
            let words = toks.iter().filter(|t| matches!(t, Tok::Word(_))).count();
            if program != "cat" || words != 2 || delim.is_empty() {
                return None;
            }
            let mut lines = rest?.split('\n');
            let closed = lines.by_ref().any(|l| {
                let l = if strip_tabs {
                    l.trim_start_matches('\t')
                } else {
                    l
                };
                l == delim
            });
            // Nothing but blank lines after the closing line.
            (closed && lines.all(|l| l.trim().is_empty())).then(|| path.clone())
        }
        None => {
            // One line: `cat > PATH`, `printf ... > PATH`, `echo ... > PATH`.
            if rest.is_some_and(|r| !r.trim().is_empty()) || out + 2 != toks.len() {
                return None;
            }
            let ok = match program.as_str() {
                "cat" => out == 1,
                "printf" | "echo" => toks[..out].iter().all(|t| matches!(t, Tok::Word(_))),
                _ => false,
            };
            ok.then(|| path.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sh(script: &str) -> Value {
        json!({"command": ["sh", "-c", script]})
    }

    fn write_of(script: &str, root: &Path) -> Option<String> {
        pure_file_write(&sh(script), root).map(|w| w.path)
    }

    #[test]
    fn a_heredoc_to_a_path_in_the_worktree_is_a_pure_write() {
        let dir = tempfile::tempdir().expect("dir");
        let s = "cat > loyalty_points/src/lib.rs << 'EOF'\nfn a() {}\nEOF\n";
        assert_eq!(
            write_of(s, dir.path()).as_deref(),
            Some("loyalty_points/src/lib.rs")
        );
        let s = "cat << EOF > a.txt\nline $HOME\nEOF";
        assert_eq!(write_of(s, dir.path()).as_deref(), Some("a.txt"));
    }

    #[test]
    fn single_line_writes_and_tee_are_pure_writes() {
        let dir = tempfile::tempdir().expect("dir");
        for s in [
            "cat > a.txt",
            "echo hi > a.txt",
            "printf '%s\\n' \"x y\" > a.txt",
        ] {
            assert_eq!(write_of(s, dir.path()).as_deref(), Some("a.txt"), "{s}");
        }
        let tee = pure_file_write(&json!({"command": ["tee", "out/a.txt"]}), dir.path());
        assert_eq!(tee.map(|w| w.path).as_deref(), Some("out/a.txt"));
    }

    #[test]
    fn a_path_outside_the_worktree_is_not_a_pure_write() {
        let dir = tempfile::tempdir().expect("dir");
        for s in [
            "cat > /etc/hosts << EOF\nx\nEOF",
            "cat > ../a.txt << EOF\nx\nEOF",
            "echo hi > /tmp/a.txt",
        ] {
            assert_eq!(write_of(s, dir.path()), None, "{s}");
        }
        let abs = format!("cat > {}/a.txt << EOF\nx\nEOF", dir.path().display());
        assert_eq!(write_of(&abs, dir.path()).as_deref(), Some("a.txt"));
    }

    #[test]
    fn a_script_that_also_runs_something_is_not_a_pure_write() {
        let dir = tempfile::tempdir().expect("dir");
        for s in [
            "cat > a << EOF\nx\nEOF; make",
            "cat > a << EOF\nx\nEOF\nmake",
            "cat > a << EOF\nx\n",
            "echo hi > a && make",
            "echo hi > a; make",
            "echo $(id) > a",
            "echo hi | tee a",
            "echo hi >> a",
            "cat > a << EOF; make\nx\nEOF",
            "mkdir -p out",
        ] {
            assert_eq!(write_of(s, dir.path()), None, "{s}");
        }
    }

    #[test]
    fn the_hint_deletes_only_a_file_that_exists() {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::write(dir.path().join("a.txt"), "old").expect("write");
        let s = "cat > a.txt << EOF\nx\nEOF";
        let old = pure_file_write(&sh(s), dir.path()).expect("write");
        assert!(old.exists);
        assert!(file_write_hint(&old).contains("*** Delete File: a.txt"));
        let new = pure_file_write(&sh(&s.replace("a.txt", "b.txt")), dir.path()).expect("write");
        let hint = file_write_hint(&new);
        assert!(hint.contains("*** Add File: b.txt") && !hint.contains("Delete File"));
    }
}

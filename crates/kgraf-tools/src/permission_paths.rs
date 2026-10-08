//! Where a path a command names points, for the permission check: one
//! lexical resolution of a word against the command's directory, and one
//! containment test against the worktree, for Unix and Windows path rules.
//!
//! The check runs before the command, so it cannot ask the file system; it
//! reads the word as the platform's path rules read it. On Windows those
//! rules have more forms than "absolute or relative", and each form is
//! decided here (the safe direction: when kGraf cannot be sure a path is
//! inside the worktree, it is outside, and the caller asks):
//!
//! | Form | Example | Decision |
//! |---|---|---|
//! | drive-absolute, either separator | `C:\p\x`, `C:/p/x`, `C:\p/x` | resolved; `\` and `/` are one separator |
//! | drive-relative, same drive as the directory | `C:x` | resolved against the directory |
//! | drive-relative, other or unknown drive | `D:x` | unknown: Windows keeps one directory per drive, which kGraf cannot see |
//! | rooted with `\` | `\x` | resolved on the directory's drive or share |
//! | rooted with `/` | `/tmp/x` | unknown: the Git Bash worker shell maps it through its mount table, other programs take it on the current drive |
//! | UNC, either separator | `\\srv\share\x`, `//srv/share/x` | resolved under its share; a share is never a drive |
//! | verbatim disk or UNC | `\\?\C:\x`, `\\?\UNC\srv\share\x` | resolved, with no name changed; `.`, `..`, `/`, trailing dots or spaces in it are unknown |
//! | other verbatim, device namespace | `\\?\Volume{..}\x`, `\\.\C:\x`, `\\.\pipe\x` | unknown |
//! | `..` above the drive or share root | `C:\..\x` | above the root |
//! | `..` out of the worktree | `C:\p\..\other` | resolved, so outside |
//! | name with trailing dots or spaces | `a.txt.`, `a.txt ` | the dots and spaces dropped, as Windows does; a name of only dots or spaces is unknown |
//! | reserved device name, any extension | `NUL`, `con.txt`, `COM1`, `LPT¹` | unknown |
//! | alternate data stream | `file:stream`, `x::$DATA` | unknown |
//! | character Windows refuses in a name | `<>"\|?*`, a control character | unknown |
//! | case variant of the worktree | `c:\P\x` for `C:\p` | inside: ASCII letters compare without case |
//! | non-ASCII case variant | `C:\Ü\x` for `C:\ü` | outside: Unicode case rules differ between volumes |
//! | 8.3 short name | `C:\PROGRA~1\x` | compared as written: a short name of the worktree is outside, a short name below it stays below it |
//! | junction or link inside the worktree | `C:\p\link\x` | inside here; the read that follows opens it through `cap-std`, confined to the worktree |
//!
//! A directory with per-directory case sensitivity (`fsutil file
//! setCaseSensitiveInfo`) breaks the ASCII rule above; kGraf does not make one.
//!
//! The rules are a pure function of the strings and a [`Flavor`], so the
//! Windows rules run under test on every host. A verbatim path is read
//! through `kgraf_core::plain_path::plain_form`, and device names come from
//! `kgraf_core::plain_path::is_reserved`, so the two modules apply one rule.
//! `typed-path` (already in the lockfile) splits a Windows prefix but applies
//! none of the name rules above and knows no directory, so the rest is stated
//! here.

/// Which platform's path rules read a word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Flavor {
    Unix,
    Windows,
}

impl Flavor {
    /// The rules of the platform kGraf runs on.
    pub(crate) const HOST: Self = if cfg!(windows) {
        Self::Windows
    } else {
        Self::Unix
    };

    fn is_sep(self, c: char) -> bool {
        c == '/' || (self == Self::Windows && c == '\\')
    }
}

/// What a full path hangs from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Anchor {
    /// The Unix root `/`.
    Root,
    /// A Windows drive, its letter in upper case.
    Drive(char),
    /// A Windows share, `\\server\share`.
    Share { server: String, share: String },
}

/// A full path with `.` and `..` resolved: an anchor and its names, as
/// written (Windows trailing dots and spaces dropped).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Lexical {
    pub(crate) anchor: Anchor,
    pub(crate) names: Vec<String>,
}

/// Where a word points.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Resolved {
    Path(Lexical),
    /// `..` climbed above the anchor, which is kept.
    AboveRoot(Anchor),
    /// kGraf cannot place it; the reason, for a message.
    Unknown(&'static str),
}

/// How a word relates to the directory it is read in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Shape {
    /// Taken from the directory (`x`, `./x`, `C:x` on Windows).
    Relative,
    /// Rooted on the directory's drive or share (`\x` on Windows).
    Rooted,
    /// A full path on its own (`/x` on Unix, `C:\x`, `\\srv\share\x`).
    Absolute,
    /// A form kGraf does not place (a device path, `/x` on Windows).
    Unplaced,
}

/// The shape of `word` under `flavor`'s rules.
pub(crate) fn shape(flavor: Flavor, word: &str) -> Shape {
    match flavor {
        Flavor::Unix => {
            if word.starts_with('/') {
                Shape::Absolute
            } else {
                Shape::Relative
            }
        }
        Flavor::Windows => {
            let b = word.as_bytes();
            let sep = |i: usize| b.get(i).is_some_and(|c| *c == b'\\' || *c == b'/');
            if word.starts_with(r"\\?\") {
                // Placed only when it has a plain form that names the same
                // file (a drive or a share path).
                return if kgraf_core::plain_path::plain_form(word).is_some() {
                    Shape::Absolute
                } else {
                    Shape::Unplaced
                };
            }
            if sep(0) && sep(1) {
                // `\\.\`, `\\?\` with a forward slash, `//./`: the device
                // namespace.
                return if matches!(b.get(2), Some(b'.' | b'?')) && (sep(3) || b.len() == 3) {
                    Shape::Unplaced
                } else {
                    Shape::Absolute
                };
            }
            if b.first() == Some(&b'\\') {
                return Shape::Rooted;
            }
            if b.first() == Some(&b'/') {
                return Shape::Unplaced;
            }
            if drive_absolute(word) {
                Shape::Absolute
            } else {
                Shape::Relative
            }
        }
    }
}

/// Whether `word` names a file rather than a package or a program to look
/// up: it starts with `.`, or is a path of any anchored shape (`/x`; on
/// Windows also `\x`, `C:\x`, `C:x`, `\\srv\share\x`).
pub(crate) fn names_a_file(flavor: Flavor, word: &str) -> bool {
    word.starts_with('.')
        || word.starts_with('/')
        || (flavor == Flavor::Windows && (word.starts_with('\\') || drive_letter(word).is_some()))
}

/// Whether `word` contains a directory separator, so a program it names is
/// a path rather than a name looked up on PATH.
pub(crate) fn has_separator(flavor: Flavor, word: &str) -> bool {
    word.chars().any(|c| flavor.is_sep(c))
        || (flavor == Flavor::Windows && drive_letter(word).is_some())
}

/// Whether `word`, on Windows, is a path rooted with one `/` that Git Bash
/// maps into its own tree (`/usr/bin/env`, `/bin/sh`): not a drive path
/// (`/c/x`, the drive C:), which can name any file. Such a path is never in
/// a worktree unless the worktree lies in Git Bash's tree or a folder its
/// mount table maps (`/tmp`).
pub(crate) fn git_bash_tree_path(flavor: Flavor, word: &str) -> bool {
    if flavor != Flavor::Windows {
        return false;
    }
    let Some(rest) = word.strip_prefix('/') else {
        return false;
    };
    if rest.starts_with(['/', '\\']) {
        return false;
    }
    let first = rest.split(['/', '\\']).next().unwrap_or_default();
    !(first.len() == 1 && first.chars().all(|c| c.is_ascii_alphabetic()))
}

/// `word` read against `base`, the directory a command runs in (`None`
/// when kGraf cannot tell).
pub(crate) fn resolve(flavor: Flavor, base: Option<&Lexical>, word: &str) -> Resolved {
    if word.contains('\0') {
        return Resolved::Unknown("a path with a NUL character");
    }
    match flavor {
        Flavor::Unix => {
            let start = if word.starts_with('/') {
                Lexical {
                    anchor: Anchor::Root,
                    names: Vec::new(),
                }
            } else {
                match base {
                    Some(b) => b.clone(),
                    None => return Resolved::Unknown(NO_DIRECTORY),
                }
            };
            push_names(flavor, start, word)
        }
        Flavor::Windows => resolve_windows(base, word),
    }
}

/// The reason for a relative word read with no known directory.
pub(crate) const NO_DIRECTORY: &str = "a relative path, and kGraf cannot tell its directory";

/// A full directory path (a worktree, a home, a command's directory) as
/// [`Lexical`]; `None` when it is not full, not Unicode, or not placeable.
pub(crate) fn full(flavor: Flavor, path: &std::path::Path) -> Option<Lexical> {
    match resolve(flavor, None, path.to_str()?) {
        Resolved::Path(l) => Some(l),
        Resolved::AboveRoot(_) | Resolved::Unknown(_) => None,
    }
}

/// The names of `path` below `root` when `path` is `root` or inside it.
/// Windows compares a drive letter, a share and ASCII letters without case.
pub(crate) fn within<'a>(
    flavor: Flavor,
    path: &'a Lexical,
    root: &Lexical,
) -> Option<&'a [String]> {
    let same_anchor = match (&path.anchor, &root.anchor) {
        (Anchor::Root, Anchor::Root) => true,
        (Anchor::Drive(a), Anchor::Drive(b)) => a == b,
        (
            Anchor::Share {
                server: s1,
                share: h1,
            },
            Anchor::Share {
                server: s2,
                share: h2,
            },
        ) => same_name(flavor, s1, s2) && same_name(flavor, h1, h2),
        _ => false,
    };
    if !same_anchor || path.names.len() < root.names.len() {
        return None;
    }
    path.names
        .iter()
        .zip(&root.names)
        .all(|(a, b)| same_name(flavor, a, b))
        .then(|| &path.names[root.names.len()..])
}

/// Whether two names are one name under `flavor`'s comparison.
pub(crate) fn same_name(flavor: Flavor, a: &str, b: &str) -> bool {
    match flavor {
        Flavor::Unix => a == b,
        Flavor::Windows => a.eq_ignore_ascii_case(b),
    }
}

/// The drive letter of `C:...`, upper case.
fn drive_letter(word: &str) -> Option<char> {
    let mut chars = word.chars();
    let letter = chars.next().filter(char::is_ascii_alphabetic)?;
    (chars.next() == Some(':')).then(|| letter.to_ascii_uppercase())
}

/// `C:\` or `C:/`.
fn drive_absolute(word: &str) -> bool {
    drive_letter(word).is_some() && matches!(word.as_bytes().get(2), Some(b'\\' | b'/'))
}

fn resolve_windows(base: Option<&Lexical>, word: &str) -> Resolved {
    let flavor = Flavor::Windows;
    match shape(flavor, word) {
        Shape::Unplaced => Resolved::Unknown(if word.starts_with('/') && !word.starts_with("//") {
            "a path rooted with `/`, which the Git Bash shell and Windows programs place \
                 differently"
        } else {
            "a Windows device or volume path"
        }),
        // Windows passes a `\\?\` path on with no name changed, so it is
        // placed only through the plain form that names the same file.
        Shape::Absolute if word.starts_with(r"\\?\") => {
            match kgraf_core::plain_path::plain_form(word) {
                Some(plain) if !plain.starts_with(r"\\?\") => resolve_windows(base, &plain),
                _ => Resolved::Unknown(
                    "a verbatim path whose names Windows would read differently in another form",
                ),
            }
        }
        Shape::Absolute if drive_absolute(word) => {
            let anchor = Anchor::Drive(drive_letter(word).unwrap_or('C'));
            push_names(
                flavor,
                Lexical {
                    anchor,
                    names: Vec::new(),
                },
                &word[2..],
            )
        }
        Shape::Absolute => {
            // `\\server\share\rest`, either separator.
            let mut parts = word[2..].splitn(3, ['\\', '/']);
            let (Some(server), Some(share)) = (parts.next(), parts.next()) else {
                return Resolved::Unknown("a share path with no share name");
            };
            if server.is_empty() || share.is_empty() || !plain_name(server) || !plain_name(share) {
                return Resolved::Unknown("a share path with no share name");
            }
            let anchor = Anchor::Share {
                server: server.to_owned(),
                share: share.to_owned(),
            };
            push_names(
                flavor,
                Lexical {
                    anchor,
                    names: Vec::new(),
                },
                parts.next().unwrap_or_default(),
            )
        }
        Shape::Rooted => match base {
            Some(b) => push_names(
                flavor,
                Lexical {
                    anchor: b.anchor.clone(),
                    names: Vec::new(),
                },
                word,
            ),
            None => Resolved::Unknown(NO_DIRECTORY),
        },
        Shape::Relative => {
            let Some(b) = base else {
                return Resolved::Unknown(NO_DIRECTORY);
            };
            match drive_letter(word) {
                // `C:x` is taken from the current directory of drive C:,
                // which is the command's directory only when that is on C:.
                Some(d) if b.anchor == Anchor::Drive(d) => {
                    push_names(flavor, b.clone(), &word[2..])
                }
                Some(_) => Resolved::Unknown(
                    "a drive-relative path on a drive other than the command's directory",
                ),
                None => push_names(flavor, b.clone(), word),
            }
        }
    }
}

/// `start` with the names of `rest` added, `.` and `..` resolved.
fn push_names(flavor: Flavor, mut start: Lexical, rest: &str) -> Resolved {
    for raw in rest.split(|c| flavor.is_sep(c)) {
        match raw {
            "" | "." => {}
            ".." => {
                if start.names.pop().is_none() {
                    return Resolved::AboveRoot(start.anchor);
                }
            }
            n if flavor == Flavor::Windows => {
                // Windows drops trailing dots and spaces from a name.
                let trimmed = n.trim_end_matches(['.', ' ']);
                if trimmed.is_empty() {
                    return Resolved::Unknown("a name of only dots or spaces");
                }
                if let Err(why) = windows_name(trimmed) {
                    return Resolved::Unknown(why);
                }
                start.names.push(trimmed.to_owned());
            }
            n => start.names.push(n.to_owned()),
        }
    }
    Resolved::Path(start)
}

/// A server or share name with nothing Windows would read as more.
fn plain_name(n: &str) -> bool {
    n != "." && n != ".." && windows_name(n).is_ok()
}

/// Why a Windows name does not name an ordinary file, if it does not.
fn windows_name(n: &str) -> Result<(), &'static str> {
    if n.contains(':') {
        return Err("a name with `:`, an alternate data stream");
    }
    if n.chars()
        .any(|c| c.is_control() || matches!(c, '<' | '>' | '"' | '|' | '?' | '*'))
    {
        return Err("a name with a character Windows does not allow");
    }
    if kgraf_core::plain_path::is_reserved(n) {
        return Err("a Windows device name");
    }
    Ok(())
}

#[cfg(test)]
#[path = "permission_paths_tests.rs"]
mod tests;

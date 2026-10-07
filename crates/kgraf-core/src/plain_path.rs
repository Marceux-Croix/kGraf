//! Canonical paths in the plain form that other programs read.
//!
//! On Windows, `std::fs::canonicalize` returns a verbatim path:
//! `\\?\C:\Users\x` or `\\?\UNC\server\share\x`. Git for Windows never accepts
//! that form ("could not create leading directories of '//?/C:/...'"), and
//! neither do `cmd`, PowerShell scripts or most other programs. A verbatim
//! path also does not compare equal to the same path in plain form, so a
//! canonical path and a path from `tempfile` or the environment did not match.
//!
//! [`canonicalize`] is the one canonicalization in this workspace (the root
//! `clippy.toml` disallows the standard one). It removes the verbatim prefix
//! when the plain form names the same file, and returns an error when it does
//! not. So a successful result is never verbatim, and two canonical paths are
//! always in one form when kGraf compares them (sandbox roots, containment
//! checks, hidden folders, policy matches).
//!
//! The rule is a port of the `dunce` crate's `simplified` (CC0, MIT-0 or
//! Apache-2.0). It is written as a pure string function so that its tests run
//! on every platform with Windows-shaped inputs. One change from `dunce`: the
//! length of the path does not keep the verbatim form. Rust's own file calls
//! add the verbatim prefix again for a long plain path, and git reads a long
//! plain path when `core.longpaths` is set, but it never reads a verbatim one.
//! A path over `MAX_PATH` in plain form is therefore still the better form to
//! give another program.

use std::borrow::Cow;
use std::io;
use std::path::{Path, PathBuf};

/// The prefix of a verbatim Windows path.
const VERBATIM: &str = r"\\?\";

/// The longest file name NTFS stores, in UTF-16 code units.
const MAX_COMPONENT: usize = 255;

/// Device names that Win32 resolves to a device in any folder, with or
/// without an extension. `COM0`, `LPT0` and the superscript digits are
/// included: Windows reserves some of them, and a name kept in verbatim form
/// by mistake costs only a refusal, never a wrong file.
const RESERVED: &[&str] = &[
    "CON",
    "PRN",
    "AUX",
    "NUL",
    "CONIN$",
    "CONOUT$",
    "COM0",
    "COM1",
    "COM2",
    "COM3",
    "COM4",
    "COM5",
    "COM6",
    "COM7",
    "COM8",
    "COM9",
    "COM\u{b9}",
    "COM\u{b2}",
    "COM\u{b3}",
    "LPT0",
    "LPT1",
    "LPT2",
    "LPT3",
    "LPT4",
    "LPT5",
    "LPT6",
    "LPT7",
    "LPT8",
    "LPT9",
    "LPT\u{b9}",
    "LPT\u{b2}",
    "LPT\u{b3}",
];

/// Whether `path` starts with the verbatim prefix `\\?\`.
pub fn is_verbatim(path: &str) -> bool {
    path.starts_with(VERBATIM)
}

/// The plain form of the verbatim Windows path `path`, when Win32 reads that
/// plain form as the same path: `\\?\C:\x` gives `C:\x` and
/// `\\?\UNC\server\share\x` gives `\\server\share\x`.
///
/// `None` when `path` is not verbatim, or when its plain form would name a
/// different file: another verbatim kind (`\\?\Volume{...}`, `\\?\GLOBALROOT`),
/// a drive with no root folder (`\\?\C:` is the volume, not `C:\`), or a
/// component Win32 rewrites or refuses in plain form (a trailing dot or
/// space, a reserved device name such as `CON` or `nul.txt`, `.` or `..`, an
/// empty component, `/`, `:` or another character a file name cannot hold, or
/// a name over 255 UTF-16 units).
pub fn plain_form(path: &str) -> Option<String> {
    let rest = path.strip_prefix(VERBATIM)?;
    let (mut plain, tail) = if let Some(unc) = rest.strip_prefix(r"UNC\") {
        let mut parts = unc.splitn(3, '\\');
        let server = parts.next()?;
        let share = parts.next()?;
        if !valid_component(server) || !valid_component(share) {
            return None;
        }
        (format!(r"\\{server}\{share}"), parts.next())
    } else {
        let bytes = rest.as_bytes();
        let drive = bytes.first().is_some_and(u8::is_ascii_alphabetic)
            && bytes.get(1) == Some(&b':')
            && bytes.get(2) == Some(&b'\\');
        if !drive {
            return None;
        }
        // `rest[..2]` is ASCII (checked above), so the slice is on a char
        // boundary.
        (rest[..2].to_string(), Some(&rest[3..]))
    };
    // A drive root keeps its separator: `C:` alone is the current folder of
    // drive C, not its root.
    let Some(tail) = tail else {
        return Some(plain);
    };
    plain.push('\\');
    if tail.is_empty() {
        return Some(plain);
    }
    // One trailing separator is kept as written; an empty component anywhere
    // else has no plain form.
    let body = tail.strip_suffix('\\').unwrap_or(tail);
    if !body.split('\\').all(valid_component) {
        return None;
    }
    plain.push_str(tail);
    Some(plain)
}

/// Whether Win32 reads `name` in a plain path as the same file name.
fn valid_component(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name.encode_utf16().count() <= MAX_COMPONENT
        && !name.ends_with(['.', ' '])
        && !name
            .chars()
            .any(|c| c < ' ' || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'))
        && !is_reserved(name)
}

/// Whether `name` is a device name: the part before the first dot, without
/// trailing spaces, matched without case (`con`, `NUL.txt`, `Aux .rs`).
fn is_reserved(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).trim_end_matches(' ');
    RESERVED
        .iter()
        .any(|r| stem.to_uppercase() == r.to_uppercase())
}

/// `path` in plain form when it is a verbatim Windows path that has one, else
/// `path` unchanged. For a path kGraf did not canonicalize itself (a
/// `read_link` target, a value read back from a file) before it goes to
/// another program or into a comparison with a canonical path. On other
/// platforms this is the identity: `\\?\` is an ordinary file name there.
pub fn simplify(path: &Path) -> Cow<'_, Path> {
    if cfg!(windows)
        && let Some(plain) = path.to_str().and_then(plain_form)
    {
        return Cow::Owned(PathBuf::from(plain));
    }
    Cow::Borrowed(path)
}

/// `std::fs::canonicalize`, then on Windows the plain form of the result.
///
/// On Windows this fails with [`io::ErrorKind::InvalidInput`] when the
/// canonical path has no plain form (see [`plain_form`]) or is not valid
/// Unicode. Such a path cannot go to git, and a verbatim result would not
/// compare equal to the plain canonical paths around it, so a containment
/// check could miss it. A caller that falls back to the path as written on an
/// error keeps working, in the form every other path is in.
pub fn canonicalize(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    #[allow(clippy::disallowed_methods)]
    let real = std::fs::canonicalize(path.as_ref())?;
    if cfg!(windows) {
        plain_canonical(real)
    } else {
        Ok(real)
    }
}

/// The plain form of the canonical path `real`, or the error [`canonicalize`]
/// documents. A path that is not verbatim is returned as it is.
fn plain_canonical(real: PathBuf) -> io::Result<PathBuf> {
    let Some(text) = real.to_str() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "the canonical path {} is not valid Unicode, so it has no plain form other \
                 programs can read",
                real.display()
            ),
        ));
    };
    if !is_verbatim(text) {
        return Ok(real);
    }
    match plain_form(text) {
        Some(plain) => Ok(PathBuf::from(plain)),
        None => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "the canonical path {text} has no plain form that names the same file (a \
                 reserved device name, a trailing dot or space, or a volume with no drive \
                 letter); git and other programs cannot read it"
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_verbatim_drive_path_becomes_plain() {
        assert_eq!(
            plain_form(r"\\?\C:\Users\me\AppData\Local\Temp\.tmpAb12\ws\w").as_deref(),
            Some(r"C:\Users\me\AppData\Local\Temp\.tmpAb12\ws\w")
        );
        assert_eq!(plain_form(r"\\?\d:\x").as_deref(), Some(r"d:\x"));
    }

    #[test]
    fn a_drive_root_keeps_its_separator() {
        assert_eq!(plain_form(r"\\?\C:\").as_deref(), Some(r"C:\"));
    }

    #[test]
    fn a_drive_with_no_root_folder_is_the_volume_and_stays() {
        assert_eq!(plain_form(r"\\?\C:"), None);
        assert_eq!(plain_form(r"\\?\C:x"), None);
    }

    #[test]
    fn a_trailing_separator_is_kept() {
        assert_eq!(plain_form(r"\\?\C:\a\").as_deref(), Some(r"C:\a\"));
    }

    #[test]
    fn a_verbatim_unc_path_becomes_a_plain_unc_path() {
        assert_eq!(
            plain_form(r"\\?\UNC\server\share\dir\f.txt").as_deref(),
            Some(r"\\server\share\dir\f.txt")
        );
        assert_eq!(
            plain_form(r"\\?\UNC\server\share").as_deref(),
            Some(r"\\server\share")
        );
        assert_eq!(
            plain_form(r"\\?\UNC\server\share\").as_deref(),
            Some(r"\\server\share\")
        );
    }

    #[test]
    fn a_unc_path_without_a_share_stays() {
        assert_eq!(plain_form(r"\\?\UNC\server"), None);
        assert_eq!(plain_form(r"\\?\UNC\\share"), None);
    }

    #[test]
    fn other_verbatim_kinds_stay() {
        assert_eq!(
            plain_form(r"\\?\Volume{0c1b2a3d-0000-0000-0000-100000000000}\x"),
            None
        );
        assert_eq!(plain_form(r"\\?\GLOBALROOT\Device\HarddiskVolume1\x"), None);
        assert_eq!(plain_form(r"\\?\1:\x"), None);
    }

    #[test]
    fn a_path_that_is_not_verbatim_has_no_plain_form_to_give() {
        assert_eq!(plain_form(r"C:\x"), None);
        assert_eq!(plain_form(r"\\server\share\x"), None);
        assert_eq!(plain_form(r"\\.\pipe\x"), None);
        assert_eq!(plain_form("/tmp/x"), None);
    }

    #[test]
    fn a_trailing_dot_or_space_stays_verbatim() {
        // Win32 drops them from a plain path: `C:\a.\b` would open `C:\a\b`.
        assert_eq!(plain_form(r"\\?\C:\a.\b"), None);
        assert_eq!(plain_form(r"\\?\C:\a\b "), None);
        assert_eq!(plain_form(r"\\?\C:\a\.git."), None);
    }

    #[test]
    fn a_reserved_device_name_stays_verbatim() {
        for name in [
            "CON",
            "con",
            "nul.txt",
            "Aux .rs",
            "COM1",
            "lpt9.tar.gz",
            "CONIN$",
            "COM\u{b9}",
        ] {
            let path = format!(r"\\?\C:\repo\{name}");
            assert_eq!(plain_form(&path), None, "{name}");
        }
        // Not device names: a longer stem, or a dot before the name.
        for name in ["CONFIG", "console.log", "nulls", "COM10", ".con", "my con"] {
            let path = format!(r"\\?\C:\repo\{name}");
            assert_eq!(
                plain_form(&path),
                Some(format!(r"C:\repo\{name}")),
                "{name}"
            );
        }
    }

    #[test]
    fn dot_components_and_empty_components_stay_verbatim() {
        assert_eq!(plain_form(r"\\?\C:\a\..\b"), None);
        assert_eq!(plain_form(r"\\?\C:\a\.\b"), None);
        assert_eq!(plain_form(r"\\?\C:\a\\b"), None);
    }

    #[test]
    fn characters_a_plain_name_cannot_hold_stay_verbatim() {
        // `/` is a literal character in a verbatim path and a separator in a
        // plain one; `:` opens an alternate data stream.
        for c in ['/', ':', '<', '>', '"', '|', '?', '*', '\u{1}'] {
            let path = format!(r"\\?\C:\a\b{c}c");
            assert_eq!(plain_form(&path), None, "{c:?}");
        }
    }

    #[test]
    fn a_name_over_255_units_stays_verbatim() {
        let ok = "a".repeat(255);
        let long = "a".repeat(256);
        assert!(plain_form(&format!(r"\\?\C:\{ok}")).is_some());
        assert_eq!(plain_form(&format!(r"\\?\C:\{long}")), None);
    }

    #[test]
    fn a_path_over_max_path_still_becomes_plain() {
        // Git reads a long plain path with `core.longpaths`, never a verbatim one.
        let deep = vec!["segment"; 60].join(r"\");
        let path = format!(r"\\?\C:\{deep}");
        assert!(path.len() > 260);
        assert_eq!(plain_form(&path), Some(format!(r"C:\{deep}")));
    }

    #[test]
    fn non_ascii_names_become_plain() {
        assert_eq!(
            plain_form("\\\\?\\C:\\Users\\J\u{f6}rg\\\u{6587}\u{6863}").as_deref(),
            Some("C:\\Users\\J\u{f6}rg\\\u{6587}\u{6863}")
        );
    }

    #[test]
    fn a_canonical_verbatim_path_without_a_plain_form_is_an_error() {
        let err = plain_canonical(PathBuf::from(r"\\?\C:\repo\nul")).expect_err("no plain form");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(
            plain_canonical(PathBuf::from(r"\\?\C:\repo\src")).expect("plain"),
            PathBuf::from(r"C:\repo\src")
        );
        assert_eq!(
            plain_canonical(PathBuf::from("/tmp/x")).expect("as is"),
            PathBuf::from("/tmp/x")
        );
    }

    #[test]
    fn canonicalize_resolves_a_real_path() {
        let dir = tempfile::tempdir().expect("dir");
        let got = canonicalize(dir.path()).expect("canonical");
        assert!(got.is_absolute());
        assert!(!is_verbatim(&got.to_string_lossy()), "{}", got.display());
        assert_eq!(canonicalize(&got).expect("again"), got);
    }

    #[cfg(not(windows))]
    #[test]
    fn simplify_is_the_identity_off_windows() {
        let p = Path::new(r"\\?\C:\x");
        assert_eq!(simplify(p), Cow::Borrowed(p));
    }

    #[cfg(windows)]
    #[test]
    fn simplify_makes_a_verbatim_path_plain_on_windows() {
        assert_eq!(simplify(Path::new(r"\\?\C:\x")), Path::new(r"C:\x"));
        assert_eq!(simplify(Path::new(r"\\?\C:\nul")), Path::new(r"\\?\C:\nul"));
    }

    /// A worktree made from a canonical temp path: the path that failed with
    /// "could not create leading directories of '//?/C:/...'" before the
    /// verbatim prefix was removed.
    #[cfg(windows)]
    #[test]
    fn git_makes_a_worktree_at_a_canonical_temp_path() {
        use std::process::Command;
        let dir = tempfile::tempdir().expect("dir");
        let root = canonicalize(dir.path()).expect("canonical");
        assert!(!is_verbatim(&root.to_string_lossy()), "{}", root.display());
        let repo = root.join("repo");
        std::fs::create_dir(&repo).expect("repo dir");
        let git = |cwd: &Path, args: &[&std::ffi::OsStr]| {
            let out = Command::new("git")
                .current_dir(cwd)
                .args(args)
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .output()
                .expect("git runs");
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        let os = |s: &'static str| std::ffi::OsStr::new(s);
        git(&repo, &[os("init"), os("-q")]);
        git(
            &repo,
            &[
                os("-c"),
                os("user.name=t"),
                os("-c"),
                os("user.email=t@t"),
                os("commit"),
                os("-q"),
                os("--allow-empty"),
                os("-m"),
                os("init"),
            ],
        );
        let worktree = canonicalize(&root).expect("root").join("ws").join("w");
        git(
            &repo,
            &[
                os("worktree"),
                os("add"),
                os("--detach"),
                worktree.as_os_str(),
                os("HEAD"),
            ],
        );
        assert!(worktree.join(".git").is_file());
    }
}

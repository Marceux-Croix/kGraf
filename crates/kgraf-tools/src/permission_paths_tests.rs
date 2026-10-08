//! The path forms of each platform, as pure string tests: the Windows rules
//! run on every host.

use super::*;
use crate::permissions::{
    HardLimit, ReadContext, Unplaced, critical_deletion_of, locate, read_call_in,
};
use std::path::{Path, PathBuf};

/// Where a word lands relative to a worktree.
#[derive(Debug, PartialEq, Eq)]
enum Lands {
    /// Inside, at this worktree-relative path (`/`-joined).
    In(&'static str),
    Out,
    Unknown,
}

fn lands(flavor: Flavor, root: &str, cwd: &str, word: &str) -> Lands {
    match locate(flavor, Path::new(root), Path::new(cwd), word) {
        Ok(rel) => {
            let joined: Vec<String> = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            Lands::In(Box::leak(joined.join("/").into_boxed_str()))
        }
        Err(Unplaced::Outside) => Lands::Out,
        Err(Unplaced::Unknown(_)) => Lands::Unknown,
    }
}

fn check(flavor: Flavor, root: &str, cwd: &str, table: &[(&str, Lands)]) {
    let mut wrong = Vec::new();
    for (word, want) in table {
        let got = lands(flavor, root, cwd, word);
        if &got != want {
            wrong.push(format!("{word:?}: want {want:?}, got {got:?}"));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

#[test]
fn windows_path_forms_land_inside_outside_or_unknown() {
    use Lands::*;
    check(
        Flavor::Windows,
        r"C:\proj",
        r"C:\proj\sub",
        &[
            // Relative, either separator, `.` and `..`.
            ("x.js", In("sub/x.js")),
            (r".\x.js", In("sub/x.js")),
            ("./x.js", In("sub/x.js")),
            (r"..\x.js", In("x.js")),
            ("../x.js", In("x.js")),
            (r"..\..\x.js", Out),
            (r"a\\b", In("sub/a/b")),
            // Drive-absolute, mixed separators, case variants.
            (r"C:\proj/pre.js", In("pre.js")),
            ("C:/proj/pre.js", In("pre.js")),
            (r"c:\PROJ\Pre.js", In("Pre.js")),
            (r"C:\proj", In("")),
            (r"C:\proj\..\other\x", Out),
            (r"C:\projx\y", Out),
            (r"C:\..\x", Out),
            (r"D:\proj\x", Out),
            // Rooted on the directory's drive.
            (r"\proj\x", In("x")),
            (r"\other\x", Out),
            // Rooted with `/`: Git Bash and Windows place it differently.
            ("/tmp/x", Unknown),
            ("/c/proj/x", Unknown),
            ("/", Unknown),
            // Drive-relative.
            ("C:x", In("sub/x")),
            (r"c:..\x", In("x")),
            ("C:", In("sub")),
            ("D:x", Unknown),
            // Shares.
            (r"\\server\share\x", Out),
            ("//server/share/proj/x", Out),
            (r"\\server", Unknown),
            (r"\\server\", Unknown),
            // Verbatim.
            (r"\\?\C:\proj\x", In("x")),
            (r"\\?\c:\proj\x", In("x")),
            (r"\\?\C:\proj\..\x", Unknown),
            (r"\\?\C:\proj\.\x", Unknown),
            (r"\\?\C:\proj\x.", Unknown),
            (r"\\?\C:\proj\x ", Unknown),
            (r"\\?\C:/proj/x", Unknown),
            (r"\\?\C:\proj\nul", Unknown),
            (r"\\?\UNC\srv\share\x", Out),
            (r"\\?\Volume{1b2c}\proj\x", Unknown),
            (r"\\?\GLOBALROOT\Device\x", Unknown),
            // The device namespace.
            (r"\\.\C:\proj\x", Unknown),
            (r"\\.\pipe\x", Unknown),
            ("//./pipe/x", Unknown),
            (r"\\.", Unknown),
            ("//?/C:/proj/x", Unknown),
            // Reserved device names, with or without an extension.
            ("NUL", Unknown),
            ("nul.bin", Unknown),
            ("con.txt", Unknown),
            ("CON", Unknown),
            ("PRN", Unknown),
            ("aux.js", Unknown),
            ("COM1", Unknown),
            ("com9.x", Unknown),
            ("LPT1", Unknown),
            ("lpt\u{00B9}.x", Unknown),
            ("CONIN$", Unknown),
            ("conout$", Unknown),
            ("NUL .txt", Unknown),
            (r"x\aux\y", Unknown),
            // Names that only resemble device names.
            ("nullable.js", In("sub/nullable.js")),
            ("COM10", In("sub/COM10")),
            ("console.js", In("sub/console.js")),
            ("LPT", In("sub/LPT")),
            // Alternate data streams.
            ("file:stream", Unknown),
            ("x.js::$DATA", Unknown),
            (r"C:\proj\x:s", Unknown),
            // Trailing dots and spaces are dropped, as Windows does.
            ("a.txt.", In("sub/a.txt")),
            ("a.txt  ", In("sub/a.txt")),
            ("a.txt. .", In("sub/a.txt")),
            (r"dir.\x", In("sub/dir/x")),
            (r"C:\proj.\x", In("x")),
            ("...", Unknown),
            (".. ", Unknown),
            (" ", Unknown),
            // Characters Windows refuses in a name.
            ("a<b", Unknown),
            ("a>b", Unknown),
            ("a|b", Unknown),
            ("a\"b", Unknown),
            ("a?b", Unknown),
            ("a*b", Unknown),
            ("a\u{1}b", Unknown),
            ("a\0b", Unknown),
            // 8.3 short names compare as written.
            (r"C:\PROJ~1\x", Out),
            (r"C:\proj\SUB~1\x", In("SUB~1/x")),
            // A junction inside the worktree is inside here; the read that
            // follows opens it confined to the worktree.
            (r"C:\proj\link\x", In("link/x")),
        ],
    );
}

#[test]
fn windows_case_folds_only_ascii_letters() {
    use Lands::*;
    check(
        Flavor::Windows,
        "C:\\\u{00DC}ber",
        "C:\\\u{00DC}ber",
        &[
            ("C:\\\u{00DC}BER\\x", In("x")),
            ("C:\\\u{00FC}ber\\x", Out),
            ("c:\\\u{00DC}ber\\x", In("x")),
        ],
    );
    // The Kelvin sign lowercases to `k`; NTFS does not fold it.
    check(
        Flavor::Windows,
        r"C:\kit",
        r"C:\kit",
        &[("C:\\\u{212A}it\\x", Out), (r"C:\KIT\x", In("x"))],
    );
}

#[test]
fn a_worktree_given_in_another_form_is_compared_by_place() {
    use Lands::*;
    // A root in lower case, or verbatim, or with a trailing separator.
    check(
        Flavor::Windows,
        r"c:\Proj",
        r"c:\Proj",
        &[(r"C:\PROJ\x", In("x"))],
    );
    check(
        Flavor::Windows,
        r"\\?\C:\proj",
        r"\\?\C:\proj",
        &[(r"C:\proj\x", In("x")), ("x", In("x"))],
    );
    check(
        Flavor::Windows,
        r"C:\proj\",
        r"C:\proj",
        &[(r"C:\proj\x", In("x"))],
    );
    // A share worktree: `\x` is rooted on the share.
    check(
        Flavor::Windows,
        r"\\srv\share\proj",
        r"\\srv\share\proj",
        &[
            (r"\proj\x", In("x")),
            (r"\x", Out),
            (r"\\SRV\Share\proj\x", In("x")),
            ("//srv/share/proj/x", In("x")),
            (r"\\?\UNC\srv\share\proj\x", In("x")),
            (r"\\srv\other\proj\x", Out),
            (r"C:\proj\x", Out),
            ("C:x", Unknown),
            (r"..\..\x", Out),
        ],
    );
    // A root kGraf cannot place makes every name unknown.
    check(Flavor::Windows, r"proj", r"proj", &[("x", Unknown)]);
    check(
        Flavor::Windows,
        r"\\.\pipe\p",
        r"\\.\pipe\p",
        &[("x", Unknown)],
    );
}

#[test]
fn unix_path_forms_keep_their_meaning() {
    use Lands::*;
    check(
        Flavor::Unix,
        "/proj",
        "/proj/sub",
        &[
            ("x.js", In("sub/x.js")),
            ("../x.js", In("x.js")),
            ("../../x.js", Out),
            ("/proj/pre.js", In("pre.js")),
            ("/proj//./pre.js", In("pre.js")),
            ("/PROJ/pre.js", Out),
            ("/projx/y", Out),
            ("/../x", Out),
            ("/tmp/x", Out),
            // Windows forms are ordinary names on Unix.
            (r"C:\proj\x", In(r"sub/C:\proj\x")),
            (r"..\x", In(r"sub/..\x")),
            ("NUL", In("sub/NUL")),
            ("a.txt.", In("sub/a.txt.")),
            ("file:stream", In("sub/file:stream")),
            ("a\0b", Unknown),
        ],
    );
}

#[test]
fn shapes_and_file_names_follow_the_flavor() {
    let w = Flavor::Windows;
    let u = Flavor::Unix;
    for (word, shape_w, shape_u) in [
        ("x", Shape::Relative, Shape::Relative),
        ("C:x", Shape::Relative, Shape::Relative),
        (r"C:\x", Shape::Absolute, Shape::Relative),
        ("C:/x", Shape::Absolute, Shape::Relative),
        (r"\x", Shape::Rooted, Shape::Relative),
        ("/x", Shape::Unplaced, Shape::Absolute),
        (r"\\s\h\x", Shape::Absolute, Shape::Relative),
        ("//s/h/x", Shape::Absolute, Shape::Absolute),
        (r"\\?\C:\x", Shape::Absolute, Shape::Relative),
        (r"\\?\UNC\s\h", Shape::Absolute, Shape::Relative),
        (r"\\?\Volume{1}\x", Shape::Unplaced, Shape::Relative),
        (r"\\.\C:\x", Shape::Unplaced, Shape::Relative),
    ] {
        assert_eq!(shape(w, word), shape_w, "windows {word:?}");
        assert_eq!(shape(u, word), shape_u, "unix {word:?}");
    }
    for (word, file_w, file_u) in [
        ("./x", true, true),
        (r".\x", true, true),
        ("/x", true, true),
        (r"\x", true, false),
        (r"C:\x\pre.js", true, false),
        ("C:x", true, false),
        (r"\\srv\share\x", true, false),
        ("pkg", false, false),
        ("@scope/pkg", false, false),
        ("node:fs", false, false),
    ] {
        assert_eq!(names_a_file(w, word), file_w, "windows {word:?}");
        assert_eq!(names_a_file(u, word), file_u, "unix {word:?}");
    }
    for (word, sep_w, sep_u) in [
        ("make", false, false),
        ("bin/sh", true, true),
        (r"bin\sh", true, false),
        (r".\make", true, false),
        ("C:make", true, false),
    ] {
        assert_eq!(has_separator(w, word), sep_w, "windows {word:?}");
        assert_eq!(has_separator(u, word), sep_u, "unix {word:?}");
    }
    for (word, tree) in [
        ("/usr/bin/env", true),
        ("/bin/sh", true),
        ("/", true),
        ("/c/proj/x", false),
        ("/C", false),
        (r"/d\x", false),
        ("//srv/share", false),
        ("C:/x", false),
        ("x/y", false),
    ] {
        assert_eq!(git_bash_tree_path(w, word), tree, "{word:?}");
        assert!(!git_bash_tree_path(u, word), "{word:?}");
    }
}

/// The deletion limit for `words` under `flavor`, with the worktree
/// `workspace` and the home `home`.
fn deletion(flavor: Flavor, home: &str, workspace: &str, words: &[&str]) -> Option<HardLimit> {
    let argv: Vec<String> = words.iter().map(|w| (*w).to_owned()).collect();
    let reading = read_call_in(&argv, &ReadContext::default());
    critical_deletion_of(
        flavor,
        &reading,
        Some(&PathBuf::from(home)),
        Some(&PathBuf::from(workspace)),
    )
}

#[derive(Debug, PartialEq, Eq)]
enum Limit {
    Deny,
    Ask,
    Pass,
}

fn limit_of(l: Option<HardLimit>) -> Limit {
    match l {
        Some(HardLimit::Deny(_)) => Limit::Deny,
        Some(HardLimit::Ask(_)) => Limit::Ask,
        None => Limit::Pass,
    }
}

#[test]
fn windows_deletions_of_the_worktree_its_git_and_system_paths_are_caught() {
    use Limit::*;
    let table: &[(&[&str], Limit)] = &[
        // The worktree itself, in every spelling.
        (&["rm", "-rf", r"C:\work\ws"], Ask),
        (&["rm", "-rf", "c:/WORK/WS"], Ask),
        (&["rm", "-rf", r"C:\work\ws."], Ask),
        (&["rm", "-rf", r"\\?\C:\work\ws"], Ask),
        (&["rm", "-rf", r"\work\ws"], Ask),
        (&["rm", "-rf", "."], Ask),
        (&["rm", "-rf", "C:"], Ask),
        (&["rm", "-rf", r"C:\work\ws\sub\.."], Ask),
        // The folder above it, a top-level folder.
        (&["rm", "-rf", r"C:\work"], Deny),
        // Its `.git`, in every spelling.
        (&["rm", "-rf", r"C:\work\ws\.git"], Ask),
        (&["rm", "-rf", r"C:\WORK\WS\.GIT."], Ask),
        (&["rm", "-rf", ".git"], Ask),
        (&["rm", "-rf", r".git\hooks"], Ask),
        (&["rm", "-rf", "GIT~1"], Ask),
        (&["rm", "-f", r"C:\work\ws\.git\index"], Ask),
        // Climbing out of it (`..` is `C:\work`, a top-level folder).
        (&["rm", "-rf", ".."], Deny),
        (&["rm", "-rf", r"..\other"], Ask),
        (&["rm", "-rf", "C:../other"], Ask),
        // System paths: a drive root, a top-level folder, the home.
        (&["rm", "-rf", r"C:\"], Deny),
        (&["rm", "-rf", r"C:\*"], Deny),
        (&["rm", "-rf", r"c:\users"], Deny),
        (&["rm", "-rf", r"C:\Users\me"], Deny),
        (&["rm", "-rf", r"C:\USERS\ME"], Deny),
        (&["rm", "-rf", r"C:\..\.."], Deny),
        (&["rm", "-rf", r"\\srv\share\x"], Deny),
        (&["rm", "-rf", "/"], Deny),
        (&["rm", "-rf", "/c"], Deny),
        (&["rm", "-rf", "/c/*"], Deny),
        (&["rm", "-rf", r"\\.\C:"], Deny),
        // Where kGraf cannot tell: asked about when recursive.
        (&["rm", "-rf", "/tmp/scratch/x"], Ask),
        (&["rm", "-rf", "/c/work/ws"], Ask),
        (&["rm", "-rf", "D:x"], Ask),
        (&["rm", "-rf", r"\\.\pipe\x"], Ask),
        (&["rm", "-rf", "x:stream"], Ask),
        (&["rm", "-f", "/tmp/scratch/file"], Pass),
        (&["rm", "-f", "nul"], Pass),
        // Ordinary deletions.
        (&["rm", "-rf", "target"], Pass),
        (&["rm", r"src\old.rs"], Pass),
        (&["rm", "-rf", r"C:\work\ws\target"], Pass),
        (&["rm", "-rf", r"c:\Work\WS\target"], Pass),
        (&["rm", "-rf", r"D:\data\x"], Pass),
        (&["rm", "-rf", r"\data\x"], Pass),
        (&["rm", "-rf", r"\\srv\share\x\y"], Pass),
        (&["rm", "-rf", ".github"], Pass),
        // A directory change is followed in Windows form.
        (&["sh", "-c", "cd C:/work/ws/sub && rm -rf .."], Ask),
        (&["sh", "-c", "cd C:/work/ws/sub && rm -rf ../target"], Pass),
        (&["sh", "-c", "cd C:/other && rm -rf x"], Ask),
        (&["sh", "-c", "cd /c/work/ws && rm -rf x"], Ask),
        (&["sh", "-c", "cd C:/work/ws/a && rm -rf ../../ws"], Ask),
    ];
    let mut wrong = Vec::new();
    for (words, want) in table {
        let got = limit_of(deletion(
            Flavor::Windows,
            r"C:\Users\me",
            r"C:\work\ws",
            words,
        ));
        if &got != want {
            wrong.push(format!("{words:?}: want {want:?}, got {got:?}"));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

#[test]
fn unix_deletions_keep_their_meaning_under_the_shared_rules() {
    use Limit::*;
    let table: &[(&[&str], Limit)] = &[
        (&["rm", "-rf", "/work/ws"], Ask),
        (&["rm", "-rf", "/work/ws/.git"], Ask),
        (&["rm", "-rf", ".git"], Ask),
        (&["rm", "-rf", ".."], Deny),
        (&["rm", "-rf", "/"], Deny),
        (&["rm", "-rf", "/home/me"], Deny),
        (&["rm", "-rf", "/home"], Deny),
        (&["rm", "-rf", "/../x"], Deny),
        (&["rm", "-f", "/tmp/scratch/file"], Pass),
        (&["rm", "-rf", "target"], Pass),
        (&["rm", "-rf", "/work/WS"], Pass),
        // Windows forms are names on Unix.
        (&["rm", "-rf", r"C:\ws"], Pass),
        (&["rm", "-rf", "nul"], Pass),
    ];
    let mut wrong = Vec::new();
    for (words, want) in table {
        let got = limit_of(deletion(Flavor::Unix, "/home/me", "/work/ws", words));
        if &got != want {
            wrong.push(format!("{words:?}: want {want:?}, got {got:?}"));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

//! The configuration reader behind git aliases: the size bound, the
//! comment and header rules, quoting, and the way a repository's `.git` is
//! found.

use super::*;

fn strs(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| (*w).to_owned()).collect()
}

fn repo(config: &str) -> (tempfile::TempDir, ReadContext) {
    let dir = tempfile::tempdir().expect("dir");
    std::fs::create_dir(dir.path().join(".git")).expect("mkdir");
    std::fs::write(dir.path().join(".git/config"), config).expect("write");
    let ctx = ReadContext::in_worktree(dir.path(), None);
    (dir, ctx)
}

fn run(ctx: &ReadContext, words: &[&str]) -> Vec<Embedded> {
    git(&strs(words), 0, ctx)
}

fn why(found: &[Embedded]) -> String {
    found
        .iter()
        .filter_map(|e| match e {
            Embedded::Unreadable(w) => Some(w.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

#[test]
fn the_subject_of_a_git_command_skips_the_global_options() {
    let n = |w: &[&str]| normalized(&strs(w), 0);
    assert_eq!(
        n(&["git", "push", "origin"]),
        Some(strs(&["git", "push", "origin"]))
    );
    assert_eq!(
        n(&["git", "-c", "a=b", "push"]),
        Some(strs(&["git", "push"]))
    );
    assert_eq!(n(&["git", "-C", ".", "push"]), Some(strs(&["git", "push"])));
    assert_eq!(
        n(&["git", "--no-pager", "-c", "a=b", "-C", "d", "push", "x"]),
        Some(strs(&["git", "push", "x"]))
    );
    assert_eq!(
        n(&["git", "--no-pager", "--git-dir", "d", "push"]),
        Some(strs(&["git", "push"]))
    );
    assert_eq!(
        n(&["git", "--no-pager", "push"]),
        Some(strs(&["git", "push"]))
    );
    assert_eq!(
        n(&["git", "--git-dir=d", "push"]),
        Some(strs(&["git", "push"]))
    );
    assert_eq!(n(&["git"]), None);
    assert_eq!(n(&["git", "--no-pager"]), None);
    assert_eq!(n(&["git", "-c", "a=b"]), None);
    assert_eq!(n(&["ls", "push"]), None);
    assert_eq!(
        normalized(&strs(&["env", "A=1", "git", "-c", "a=b", "push"]), 2),
        Some(strs(&["git", "push"]))
    );
}

#[test]
fn a_configuration_file_is_read_up_to_its_bound() {
    let dir = tempfile::tempdir().expect("dir");
    let at = |name: &str, size: usize| {
        let p = dir.path().join(name);
        std::fs::write(&p, vec![b'#'; size]).expect("write");
        p
    };
    let small = at("small", 5_000);
    assert!(matches!(read_config(&small), Some(Ok(t)) if t.len() == 5_000));
    let limit = MAX_CONFIG_BYTES as usize;
    let exact = at("exact", limit);
    assert!(matches!(read_config(&exact), Some(Ok(t)) if t.len() == limit));
    let over = at("over", limit + 1);
    match read_config(&over) {
        Some(Err(why)) => assert!(why.contains("too large"), "{why}"),
        other => panic!("{other:?}"),
    }
    assert!(read_config(&dir.path().join("absent")).is_none());
    match read_config(dir.path()) {
        Some(Err(why)) => assert!(why.contains("not a regular file"), "{why}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(MAX_CONFIG_BYTES, 262_144);
}

#[test]
fn an_alias_is_read_from_the_configuration_in_every_spelling() {
    let (_d, ctx) = repo(
        "[user]\n\tname = x\n[alias]\n\tst = status\n\tev = !curl x\n\tq = \"log --oneline\"\n\tempty =\n\t# note = !curl n\n\t; other = !curl o\n[alias] one = diff\n",
    );
    // A plain alias runs git with its words and the rest of the line.
    assert_eq!(
        run(&ctx, &["git", "st", "a"]),
        vec![Embedded::Argv(strs(&["git", "status", "a"]))]
    );
    assert_eq!(
        run(&ctx, &["git", "q"]),
        vec![Embedded::Argv(strs(&["git", "log", "--oneline"]))]
    );
    // `!CMD` runs a shell command with the quoted rest.
    assert_eq!(
        run(&ctx, &["git", "ev", "a b"]),
        vec![Embedded::Script("curl x 'a b'".into())]
    );
    // An alias may follow its header on the same line.
    assert_eq!(
        run(&ctx, &["git", "one"]),
        vec![Embedded::Argv(strs(&["git", "diff"]))]
    );
    // An empty value cannot be split into a command.
    let found = run(&ctx, &["git", "empty"]);
    assert!(why(&found).contains("cannot split"), "{}", why(&found));
    // A name that is no alias runs the program `git-NAME`.
    assert_eq!(
        run(&ctx, &["git", "nothing", "a"]),
        vec![Embedded::Argv(strs(&["git-nothing", "a"]))]
    );
    // A comment is no alias, even when its text looks like one.
    assert_eq!(
        run(&ctx, &["git", "# note"]),
        vec![Embedded::Argv(strs(&["git-# note"]))]
    );
    assert_eq!(
        run(&ctx, &["git", "; other"]),
        vec![Embedded::Argv(strs(&["git-; other"]))]
    );
    // A blank line is no alias of an empty name.
    assert_eq!(
        run(&ctx, &["git", ""]),
        vec![Embedded::Argv(strs(&["git-"]))]
    );
}

#[test]
fn a_configuration_file_is_read_as_git_reads_it() {
    let alias = |text: &str, name: &str| alias_in(text, name);
    let some = |v: &str| Some(v.to_owned());
    assert_eq!(
        alias("[alias]\nst = status\n", "st"),
        (some("status"), false)
    );
    assert_eq!(
        alias("[alias]\nST = status\n", "st"),
        (some("status"), false)
    );
    assert_eq!(alias("[alias]\nst = a\nst = b\n", "st"), (some("b"), false));
    assert_eq!(
        alias("[alias] st = status\n", "st"),
        (some("status"), false)
    );
    assert_eq!(alias("[alias]\nst\n", "st"), (some("true"), false));
    assert_eq!(alias("[other]\nst = status\n", "st"), (None, false));
    assert_eq!(
        alias("[alias \"x\"]\nst = status\n", "st"),
        (some("status"), false)
    );
    assert_eq!(alias("# [alias]\nst = status\n", "st"), (None, false));
    assert_eq!(alias("; [alias]\nst = status\n", "st"), (None, false));
    assert_eq!(
        alias("[alias]\n# st = a\n; st = b\nst = c\n", "st"),
        (some("c"), false)
    );
    assert_eq!(alias("[alias]\n\nst = a\n", ""), (None, false));
    assert_eq!(
        alias("[alias]\nst = a \\\n b\n", "st"),
        (some("a b"), false)
    );
    assert_eq!(alias("[alias]\nst = a \\", "st"), (some("a"), false));
    assert_eq!(alias("[alias]\nst = a # c\n", "st"), (some("a"), false));
    assert_eq!(alias("[include]\npath = x\n", "st"), (None, true));
    assert_eq!(alias("[includeIf \"x\"]\npath = x\n", "st"), (None, true));
    assert_eq!(
        alias("[alias]\nst = a\n[include]\npath = x\n", "st"),
        (some("a"), true)
    );
    assert_eq!(alias("", "st"), (None, false));
    // Another section after `[alias]` ends it.
    assert_eq!(alias("[alias]\n[core]\nst = x\n", "st"), (None, false));
}

#[test]
fn a_configuration_value_loses_its_quotes_and_comment_and_keeps_its_escapes() {
    let u = unquote_config_value;
    assert_eq!(u("plain"), "plain");
    assert_eq!(u("  spaced  "), "spaced");
    assert_eq!(u("\"quoted\""), "quoted");
    assert_eq!(u("a # comment"), "a");
    assert_eq!(u("a ; comment"), "a");
    assert_eq!(u("\"a # not\" # comment"), "a # not");
    assert_eq!(u("\"a ; not\" ; comment"), "a ; not");
    assert_eq!(u("\"a#b\""), "a#b");
    assert_eq!(u("a\\\"b"), "a\"b");
    assert_eq!(u("a\\\\b"), "a\\b");
    assert_eq!(u("a\\nb"), "a\nb");
    assert_eq!(u("a\\tb"), "a\tb");
    assert_eq!(u("a\\xb"), "axb");
    assert_eq!(u("a\\"), "a");
    assert_eq!(u("\"a\\\"#b\""), "a\"#b");
    assert_eq!(u(""), "");
    assert_eq!(u("#only"), "");
}

#[cfg(unix)]
#[test]
fn the_dot_git_of_a_worktree_is_a_folder_or_a_file_and_nothing_else() {
    // A `.git` that is neither (a named pipe) is no repository.
    let dir = tempfile::tempdir().expect("dir");
    let made = std::process::Command::new("mkfifo")
        .arg(dir.path().join(".git"))
        .status()
        .expect("mkfifo");
    assert!(made.success());
    let ctx = ReadContext::in_worktree(dir.path(), None);
    let err = config_files(&ctx).expect_err("no repository");
    assert!(err.contains("has no `.git`"), "{err}");
    // A `.git` file names the folder.
    let dir = tempfile::tempdir().expect("dir");
    std::fs::create_dir(dir.path().join("real")).expect("mkdir");
    std::fs::write(dir.path().join(".git"), "gitdir: real\n").expect("write");
    let ctx = ReadContext::in_worktree(dir.path(), None);
    let files = config_files(&ctx).expect("files");
    assert!(files.contains(&dir.path().join("real/config")), "{files:?}");
    assert!(
        files.contains(&dir.path().join("real/config.worktree")),
        "{files:?}"
    );
    // A `.git` file that names nothing.
    std::fs::write(dir.path().join(".git"), "nothing here\n").expect("write");
    let err = config_files(&ctx).expect_err("no gitdir");
    assert!(err.contains("names no repository"), "{err}");
    // A missing `.git`.
    let empty = tempfile::tempdir().expect("dir");
    let ctx = ReadContext::in_worktree(empty.path(), None);
    let err = config_files(&ctx).expect_err("none");
    assert!(err.contains("has no `.git`"), "{err}");
}

use super::*;

#[test]
fn version_gate() {
    assert_eq!(parse_version("git version 2.54.0"), Some((2, 54)));
    assert_eq!(
        parse_version("git version 2.39.3 (Apple Git-146)"),
        Some((2, 39))
    );
    assert_eq!(parse_version("git version 2.31.0.windows.1"), Some((2, 31)));
    assert!(parse_version("git version 2.30.9").is_some_and(|v| v < MIN_GIT));
    assert_eq!(parse_version("not git"), None);
}

#[test]
fn every_driver_kind_is_neutralized_by_name() {
    let listing = b"user.name=x\nMe\0\
filter.lfs.clean\ngit-lfs clean -- %f\0\
filter.a=b.smudge\nevil\0\
merge.ours.driver\ntrue\0\
diff.dot.ted.textconv\ncat\0\
hook.audit.command\nlog\0\
tar.tgz.command\ngzip\0\
core.hooksPath\n.githooks\0\
merge.conflictstyle\nzdiff3\0\
remote.origin.url\nx\0\
flag-only.sub.bool\0";
    let got = driver_overrides(listing).expect("parse");
    let has = |k: &str, v: &str| got.iter().any(|(gk, gv)| gk == k && gv == v);
    for (k, v) in [
        ("filter.lfs.clean", ""),
        ("filter.lfs.smudge", ""),
        ("filter.lfs.process", ""),
        ("filter.lfs.required", "false"),
        ("filter.a=b.clean", ""),
        ("merge.ours.driver", ""),
        ("diff.dot.ted.textconv", ""),
        ("diff.dot.ted.command", ""),
        ("hook.audit.enabled", "false"),
        ("tar.tgz.command", ""),
    ] {
        assert!(has(k, v), "{k}={v} missing from {got:?}");
    }
    assert!(
        !got.iter()
            .any(|(k, _)| k.starts_with("remote.") || k.starts_with("merge.conflictstyle")),
        "{got:?}"
    );
}

#[test]
fn leading_global_options_are_split_from_the_subcommand() {
    fn os(v: &[&'static str]) -> Vec<&'static OsStr> {
        v.iter().map(|s| OsStr::new(*s)).collect()
    }
    let (g, rest) = split_globals(&os(&[
        "-c",
        "core.hooksPath=/dev/null",
        "--git-dir",
        "/r/.git",
        "--no-optional-locks",
        "rev-parse",
        "-q",
    ]))
    .expect("split");
    assert_eq!(g.len(), 5);
    assert_eq!(rest, os(&["rev-parse", "-q"]));
    assert!(split_globals(&os(&["--exec-path=/tmp", "status"])).is_err());
    assert!(split_globals(&os(&["-c"])).is_err());
}

#[cfg(unix)]
mod mechanism {
    use super::*;
    use std::fs;
    use std::process::Output;

    /// Plain git, the test's own setup. Not hardened: it builds the hostile
    /// repository and, as the control, shows that the planted programs do run
    /// under ordinary git.
    fn plain(dir: &Path, args: &[&str]) -> Output {
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdin(Stdio::null())
            .output()
            .expect("git")
    }

    fn plain_ok(dir: &Path, args: &[&str]) -> String {
        let out = plain(dir, args);
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// kGraf's hardened git.
    fn kgit(dir: &Path, args: &[&str]) -> Output {
        let pre = dash_c(dir);
        command(&Invocation::in_dir(&pre), args)
            .expect("hardened command")
            .stdin(Stdio::null())
            .output()
            .expect("git")
    }

    fn kgit_ok(dir: &Path, args: &[&str]) -> Vec<u8> {
        let out = kgit(dir, args);
        assert!(
            out.status.success(),
            "kgraf git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
    }

    struct Hostile {
        _tmp: tempfile::TempDir,
        root: PathBuf,
        repo: PathBuf,
        marks: PathBuf,
    }

    impl Hostile {
        fn marks(&self) -> Vec<String> {
            let mut v: Vec<String> = fs::read_dir(&self.marks)
                .expect("marks")
                .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
                .collect();
            v.sort();
            v
        }

        fn clear(&self) {
            for e in fs::read_dir(&self.marks).expect("marks") {
                fs::remove_file(e.expect("entry").path()).expect("clear");
            }
        }
    }

    /// A repository where every way git can run a program on its own is
    /// armed with a marker: hooks in `.githooks` (with `core.hooksPath` set)
    /// and in `.git/hooks`, config-defined hooks, the file system monitor, a
    /// filter (required), a merge driver and a diff driver selected by
    /// `.gitattributes` and defined in `.git/config` (one of them through an
    /// included file), a filter whose name holds `=`, a submodule pointing at
    /// a local bare repository, and `submodule.recurse` on.
    fn hostile() -> Hostile {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = tmp.path().to_path_buf();
        let marks = root.join("marks");
        fs::create_dir_all(&marks).expect("marks");
        // `mk NAME [FILE]`: leave marker NAME, then pass the input through,
        // so a filter or textconv still produces output.
        let mk = root.join("mk");
        fs::write(
            &mk,
            format!(
                "#!/bin/sh\ntouch '{}'/\"$1\"\nshift\nif [ $# -gt 0 ] && [ -f \"$1\" ]; then cat \"$1\"; else cat; fi\n",
                marks.display()
            ),
        )
        .expect("mk");
        set_exec(&mk);
        let m = |name: &str| format!("'{}' {name}", mk.display());

        let bare = root.join("sub.git");
        plain_ok(&root, &["init", "-q", "--bare", "-b", "main", "sub.git"]);
        let subsrc = root.join("subsrc");
        fs::create_dir_all(&subsrc).expect("subsrc");
        plain_ok(&subsrc, &["init", "-q", "-b", "main"]);
        plain_ok(&subsrc, &["config", "user.name", "t"]);
        plain_ok(&subsrc, &["config", "user.email", "t@example.invalid"]);
        fs::write(subsrc.join("s.txt"), "s\n").expect("s");
        plain_ok(&subsrc, &["add", "."]);
        plain_ok(&subsrc, &["commit", "-q", "-m", "s"]);
        plain_ok(
            &subsrc,
            &["push", "-q", bare.to_str().expect("utf8"), "main"],
        );
        let sub_head = plain_ok(&subsrc, &["rev-parse", "HEAD"]);

        let repo = root.join("repo");
        fs::create_dir_all(&repo).expect("repo");
        plain_ok(&repo, &["init", "-q", "-b", "main"]);
        for (k, v) in [
            ("user.name", "t".to_owned()),
            ("user.email", "t@example.invalid".to_owned()),
            ("filter.ev.clean", m("clean")),
            ("filter.ev.smudge", m("smudge")),
            ("filter.ev.required", "true".to_owned()),
            ("filter.a=b.clean", m("eqclean")),
            ("filter.a=b.smudge", m("eqsmudge")),
            (
                "merge.mev.driver",
                format!("{} </dev/null; exit 0", m("mergedriver")),
            ),
            ("diff.dev.textconv", m("textconv")),
            ("diff.dev.command", m("diffcommand")),
            ("diff.external", m("external")),
            ("core.fsmonitor", m("fsmonitor")),
            ("hook.cfg.command", m("confighook")),
            ("hook.cfg.event", "pre-commit".to_owned()),
            ("submodule.recurse", "true".to_owned()),
            ("protocol.file.allow", "always".to_owned()),
            ("include.path", root.join("inc.cfg").display().to_string()),
        ] {
            plain_ok(&repo, &["config", k, &v]);
        }
        for event in ["post-commit", "post-checkout"] {
            plain_ok(&repo, &["config", "--add", "hook.cfg.event", event]);
        }
        fs::write(
            root.join("inc.cfg"),
            format!("[filter \"inc\"]\n\tclean = {}\n", m("includedclean")),
        )
        .expect("include");
        // Unwritten by plain git below: added after the fixture commits.
        let hooks = [
            "pre-commit",
            "post-commit",
            "post-merge",
            "post-checkout",
            "pre-merge-commit",
            "reference-transaction",
            "pre-auto-gc",
        ];

        fs::write(
            repo.join(".gitattributes"),
            "*.t filter=ev merge=mev diff=dev\n*.q filter=a=b\n*.i filter=inc\n",
        )
        .expect("attrs");
        fs::write(repo.join("a.t"), "x\r\nline2\r\n").expect("a.t");
        fs::write(repo.join("plain.txt"), "1\n2\n3\n4\n5\n").expect("plain");
        fs::write(
            repo.join(".gitmodules"),
            format!(
                "[submodule \"sub\"]\n\tpath = sub\n\turl = {}\n",
                bare.display()
            ),
        )
        .expect("gitmodules");
        // Committed through plain git with the drivers off, so the fixture
        // starts with no marker.
        let off: Vec<String> = [
            "filter.ev.clean=",
            "filter.ev.smudge=",
            "filter.ev.required=false",
            "hook.cfg.enabled=false",
        ]
        .iter()
        .flat_map(|s| ["-c".to_owned(), (*s).to_owned()])
        .collect();
        let mut add: Vec<&str> = off.iter().map(String::as_str).collect();
        add.extend(["add", "."]);
        plain_ok(&repo, &add);
        let cacheinfo = format!("160000,{sub_head},sub");
        plain_ok(&repo, &["update-index", "--add", "--cacheinfo", &cacheinfo]);
        let mut commit: Vec<&str> = off.iter().map(String::as_str).collect();
        commit.extend(["commit", "-q", "-m", "base"]);
        plain_ok(&repo, &commit);

        fs::create_dir_all(repo.join(".githooks")).expect("hooks dir");
        for hook in hooks {
            for dir in [repo.join(".githooks"), repo.join(".git/hooks")] {
                let path = dir.join(hook);
                fs::write(&path, format!("#!/bin/sh\n{} </dev/null\n", m(hook))).expect("hook");
                set_exec(&path);
            }
        }
        plain_ok(&repo, &["config", "core.hooksPath", ".githooks"]);
        let h = Hostile {
            _tmp: tmp,
            root,
            repo,
            marks,
        };
        h.clear();
        h
    }

    /// Regular files under any `objects` directory below `dir`.
    fn objects_under(dir: &Path) -> usize {
        fn walk(dir: &Path, inside: bool, n: &mut usize) {
            let Ok(entries) = fs::read_dir(dir) else {
                return;
            };
            for e in entries.flatten() {
                let path = e.path();
                let here = inside || e.file_name() == "objects";
                if path.is_dir() {
                    walk(&path, here, n);
                } else if inside {
                    *n += 1;
                }
            }
        }
        let mut n = 0;
        walk(dir, false, &mut n);
        n
    }

    fn set_exec(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    /// The control: under ordinary git the fixture's programs do run, so an
    /// empty marker list from kGraf's git means the overrides held, not that
    /// the fixture was inert.
    #[test]
    fn the_fixture_runs_programs_under_plain_git() {
        let h = hostile();
        fs::write(h.repo.join("a.t"), "changed\n").expect("edit");
        fs::write(h.repo.join("e.q"), "q\n").expect("q");
        fs::write(h.repo.join("n.i"), "i\n").expect("i");
        plain_ok(&h.repo, &["add", "."]);
        plain_ok(&h.repo, &["commit", "-q", "-m", "c"]);
        let marks = h.marks();
        for expected in [
            "clean",
            "eqclean",
            "includedclean",
            "pre-commit",
            "post-commit",
            "confighook",
            "fsmonitor",
        ] {
            assert!(marks.iter().any(|m| m == expected), "{expected}: {marks:?}");
        }
    }

    #[test]
    fn commit_checkout_merge_worktree_and_fsck_run_no_program() {
        let h = hostile();
        let repo = &h.repo;
        // Commit: filter-selected files, a filter named `a=b`, an included
        // filter, and plain text.
        fs::write(repo.join("a.t"), "x\r\nline2\r\nmore\r\n").expect("edit");
        fs::write(repo.join("e.q"), "q\n").expect("q");
        fs::write(repo.join("n.i"), "i\n").expect("i");
        kgit_ok(repo, &["add", "-A"]);
        kgit_ok(repo, &["commit", "-q", "-m", "kgraf commit"]);
        assert_eq!(
            kgit_ok(repo, &["cat-file", "blob", "HEAD:a.t"]),
            b"x\r\nline2\r\nmore\r\n",
            "the blob holds the worktree's bytes, unfiltered"
        );
        // Branch, checkout, a clean merge of plain text, and a merge of a
        // path whose merge driver is configured.
        kgit_ok(repo, &["checkout", "-q", "-b", "side"]);
        fs::write(repo.join("plain.txt"), "1\n2\n3\n4\nside\n").expect("side");
        fs::write(repo.join("a.t"), "side\r\n").expect("side t");
        kgit_ok(repo, &["commit", "-q", "-am", "side"]);
        kgit_ok(repo, &["checkout", "-q", "main"]);
        assert_eq!(
            fs::read(repo.join("a.t")).expect("read"),
            b"x\r\nline2\r\nmore\r\n"
        );
        fs::write(repo.join("plain.txt"), "main\n2\n3\n4\n5\n").expect("main");
        fs::write(repo.join("a.t"), "main\r\n").expect("main t");
        kgit_ok(repo, &["commit", "-q", "-am", "main"]);
        let tree = kgit(repo, &["merge-tree", "--write-tree", "-z", "main", "side"]);
        // a.t changed on both sides: its driver would have resolved it; with
        // the driver neutralized it is a conflict, never silently resolved.
        assert_eq!(tree.status.code(), Some(1), "{:?}", tree);
        let merged = kgit(repo, &["merge", "--no-ff", "-m", "m", "side"]);
        assert!(!merged.status.success(), "a.t conflicts");
        assert_eq!(
            fs::read(repo.join("plain.txt")).expect("merged"),
            b"main\n2\n3\n4\nside\n",
            "the text merge is byte-correct"
        );
        kgit_ok(repo, &["merge", "--abort"]);
        // A merge with no driver path in it commits.
        kgit_ok(repo, &["checkout", "-q", "-b", "side2", "main~1"]);
        fs::write(repo.join("plain.txt"), "1\n2\n3\n4\nside2\n").expect("side2");
        kgit_ok(repo, &["commit", "-q", "-am", "side2"]);
        kgit_ok(repo, &["checkout", "-q", "main"]);
        kgit_ok(repo, &["merge", "--no-ff", "-q", "-m", "m2", "side2"]);
        assert_eq!(
            kgit_ok(repo, &["cat-file", "blob", "HEAD:plain.txt"]),
            b"main\n2\n3\n4\nside2\n"
        );
        // Worktree add (a checkout through git's own child processes).
        let wt = h.root.join("wt");
        kgit_ok(
            repo,
            &["worktree", "add", "-q", wt.to_str().expect("utf8"), "side"],
        );
        assert_eq!(fs::read(wt.join("a.t")).expect("wt"), b"side\r\n");
        // Reads that go through diff drivers, the status and fsck.
        fs::write(repo.join("a.t"), "dirty\n").expect("dirty");
        kgit_ok(repo, &["diff"]);
        kgit_ok(repo, &["log", "-p", "-1"]);
        kgit_ok(repo, &["show", "HEAD~1"]);
        kgit_ok(repo, &["blame", "a.t"]);
        kgit_ok(repo, &["status", "--porcelain"]);
        kgit_ok(repo, &["fsck", "--connectivity-only"]);
        kgit_ok(
            repo,
            &["worktree", "remove", "--force", wt.to_str().expect("utf8")],
        );
        assert_eq!(h.marks(), Vec::<String>::new(), "a planted program ran");
        // The submodule was never fetched: nothing under `.git/modules` and
        // an empty `sub` directory.
        assert!(!repo.join(".git/modules").exists());
        let sub = repo.join("sub");
        assert!(
            !sub.exists() || fs::read_dir(&sub).expect("sub").next().is_none(),
            "the submodule was checked out"
        );
    }

    #[test]
    fn no_transport_runs_even_to_a_local_repository() {
        let h = hostile();
        let bare = h.root.join("sub.git");
        let out = kgit(
            &h.repo,
            &["fetch", "-q", bare.to_str().expect("utf8"), "main"],
        );
        assert!(!out.status.success(), "the fetch must be refused");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains("transport") || err.contains("protocol"),
            "{err}"
        );
        let init = kgit(&h.repo, &["submodule", "update", "--init"]);
        assert!(
            !init.status.success(),
            "the submodule clone must be refused"
        );
        let err = String::from_utf8_lossy(&init.stderr);
        assert!(
            err.contains("transport") || err.contains("protocol"),
            "{err}"
        );
        // `git submodule` may leave an empty directory behind; no object of
        // the submodule may have arrived.
        assert_eq!(objects_under(&h.repo.join(".git/modules")), 0);
        assert!(!h.repo.join("sub/s.txt").exists());
        assert_eq!(h.marks(), Vec::<String>::new(), "a planted program ran");
    }

    /// A config the listing cannot read fails the build of the command with
    /// git's own message, before anything runs.
    #[test]
    fn an_unreadable_config_fails_before_the_command() {
        let h = hostile();
        fs::write(h.repo.join(".git/config"), "[broken\n").expect("break");
        let pre = dash_c(&h.repo);
        match command(&Invocation::in_dir(&pre), &["status"]) {
            Err(HostGitError::Config { stderr, .. }) => {
                assert!(stderr.contains("config"), "{stderr}")
            }
            other => panic!("expected a config failure, got {other:?}"),
        }
    }
}

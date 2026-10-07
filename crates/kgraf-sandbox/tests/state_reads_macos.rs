//! `SandboxPolicy::deny_read`: kGraf's own state directory is unreadable
//! to a sandboxed command, while a writable root beneath it (a worker's
//! worktree under `.kgraf/workspaces`) stays readable. Without the deny,
//! macOS reads are open and `cat` prints the secret.

#![cfg(target_os = "macos")]

use kgraf_sandbox::{Network, SandboxPolicy, sandboxed_command};

const SECRET: &str = "Bearer FAKE-MCP-HEADER-SECRET";

#[test]
fn the_state_directory_is_unreadable_but_a_worktree_inside_it_is_not() {
    let repo = tempfile::tempdir().unwrap();
    let state = repo.path().join(".kgraf");
    let work = state.join("workspaces/w-1");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(state.join("worker.json"), SECRET).unwrap();
    std::fs::write(work.join("own.txt"), "mine").unwrap();
    let cat = |policy: &SandboxPolicy, path: std::path::PathBuf| {
        sandboxed_command(policy, "/bin/cat", &work)
            .unwrap()
            .arg(path)
            .output()
            .unwrap()
    };

    let open = SandboxPolicy::new(vec![work.clone()], Network::Deny);
    let control = cat(&open, state.join("worker.json"));
    assert_eq!(
        String::from_utf8_lossy(&control.stdout),
        SECRET,
        "control: without the deny the secret is readable"
    );

    let denied = open.clone().deny_read(state.clone());
    let out = cat(&denied, state.join("worker.json"));
    assert!(!out.status.success(), "{out:?}");
    assert!(!String::from_utf8_lossy(&out.stdout).contains("FAKE-MCP"));
    assert!(String::from_utf8_lossy(&out.stderr).contains("Operation not permitted"));

    let own = cat(&denied, work.join("own.txt"));
    assert!(own.status.success(), "{own:?}");
    assert_eq!(String::from_utf8_lossy(&own.stdout), "mine");
}

/// A configuration kept in a broad directory (`--config ~/worker.json`) is
/// denied as one file, not its directory: the file is unreadable, a sibling
/// (standing in for `~/.cargo`) stays readable, and a real `cargo build` of a
/// small crate succeeds under the same policy.
#[test]
fn a_file_level_deny_hides_one_file_and_a_build_still_works() {
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("worker.json");
    std::fs::write(&config, SECRET).unwrap();
    std::fs::create_dir_all(home.path().join(".cargo")).unwrap();
    std::fs::write(home.path().join(".cargo/config.toml"), "# sibling").unwrap();
    let work = tempfile::tempdir().unwrap();
    let krate = work.path().join("probe");
    std::fs::create_dir_all(krate.join("src")).unwrap();
    std::fs::write(
        krate.join("Cargo.toml"),
        "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(
        krate.join("src/main.rs"),
        "fn main() { println!(\"built\"); }\n",
    )
    .unwrap();
    let policy = SandboxPolicy::new(vec![work.path().to_path_buf()], Network::Deny)
        .deny_read(config.clone());
    let run = |program: &str, args: &[&str]| {
        let mut cmd = sandboxed_command(&policy, program, &krate).unwrap();
        cmd.args(args)
            .env("CARGO_TARGET_DIR", work.path().join("target"));
        cmd.output().unwrap()
    };

    let secret = run("/bin/cat", &[config.to_str().unwrap()]);
    assert!(!secret.status.success(), "{secret:?}");
    assert!(!String::from_utf8_lossy(&secret.stdout).contains("FAKE-MCP"));
    let sibling = run(
        "/bin/cat",
        &[home.path().join(".cargo/config.toml").to_str().unwrap()],
    );
    assert!(sibling.status.success(), "{sibling:?}");

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let built = run(&cargo, &["run", "--offline", "--quiet"]);
    assert!(
        built.status.success(),
        "cargo run under a file-level deny: {}",
        String::from_utf8_lossy(&built.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&built.stdout).trim(), "built");
}

/// The gate and verifier layout: repository in a path with spaces, the state
/// directory read-denied, the worktree and its scratch directory beneath it,
/// `CARGO_TARGET_DIR` in the scratch directory. A real `cargo build` must pass.
///
/// Before the fix cargo failed with "could not parse/generate dep info ...
/// Operation not permitted": it canonicalizes the package root and the target
/// directory, and realpath(3) lstat()s `.kgraf` and `.kgraf/worktrees`,
/// whose metadata the state deny covered. The state stays unreadable: its
/// files, its listing and a sibling worktree.
#[test]
fn a_build_in_a_worktree_under_the_denied_state_directory_passes() {
    let outer = tempfile::Builder::new()
        .prefix("Want To Develop ")
        .tempdir()
        .unwrap();
    let repo = outer.path().join("An App");
    let state = repo.join(".kgraf");
    let work = state.join("worktrees/run-w1-r0-a0");
    let scratch = state.join("worktrees/.scratch/run-w1-r0-a0");
    std::fs::create_dir_all(work.join("src")).unwrap();
    std::fs::create_dir_all(&scratch).unwrap();
    std::fs::write(state.join("worker.json"), SECRET).unwrap();
    std::fs::write(
        work.join("Cargo.toml"),
        "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(
        work.join("src/main.rs"),
        "fn main() { println!(\"built\"); }\n",
    )
    .unwrap();
    let policy = SandboxPolicy::new(vec![work.clone(), scratch.clone()], Network::Deny)
        .with_tmp_dir(scratch.clone())
        .deny_read(state.clone());
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut cmd = sandboxed_command(&policy, &cargo, &work).unwrap();
    cmd.args(["build", "--offline"])
        .env("CARGO_TARGET_DIR", scratch.join("target"));
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "sandboxed cargo build: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(scratch.join("target/debug/probe").is_file());

    let sibling = state.join("worktrees/run-w2-r0-a0");
    std::fs::create_dir_all(&sibling).unwrap();
    std::fs::write(sibling.join("theirs.txt"), "theirs").unwrap();
    let other_scratch = state.join("worktrees/.scratch/run-w2-r0-a0");
    std::fs::create_dir_all(&other_scratch).unwrap();
    // An extended attribute on the state directory: the metadata re-allow
    // covers stat() only, never file-read-xattr.
    let xattr = std::process::Command::new("/usr/bin/xattr")
        .args(["-w", "com.kgraf.probe", "XATTR-SECRET"])
        .arg(&state)
        .status()
        .unwrap();
    assert!(xattr.success());
    let run = |program: &str, path: std::path::PathBuf| {
        sandboxed_command(&policy, program, &work)
            .unwrap()
            .arg(path)
            .output()
            .unwrap()
    };
    for (program, path) in [
        ("/bin/cat", state.join("worker.json")),
        ("/bin/ls", state.clone()),
        ("/bin/ls", state.join("worktrees")),
        ("/bin/ls", sibling.clone()),
        ("/bin/cat", sibling.join("theirs.txt")),
        ("/usr/bin/stat", state.join("worker.json")),
        ("/usr/bin/stat", sibling.clone()),
        ("/usr/bin/stat", other_scratch.clone()),
        ("/bin/ls", other_scratch.clone()),
    ] {
        let out = run(program, path.clone());
        assert!(
            !out.status.success(),
            "{program} {}: {out:?}",
            path.display()
        );
        assert!(!String::from_utf8_lossy(&out.stdout).contains("FAKE-MCP"));
        assert!(!String::from_utf8_lossy(&out.stdout).contains("theirs"));
    }
    // `xattr -l` exits 0 even when it cannot read the attributes, so the
    // check is on the output: the value and the name never appear.
    let out = sandboxed_command(&policy, "/usr/bin/xattr", &work)
        .unwrap()
        .arg("-l")
        .arg(&state)
        .output()
        .unwrap();
    let printed = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!printed.contains("XATTR-SECRET"), "{printed}");
    assert!(!printed.contains("com.kgraf.probe"), "{printed}");
    let control = std::process::Command::new("/usr/bin/xattr")
        .args(["-l"])
        .arg(&state)
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&control.stdout).contains("XATTR-SECRET"),
        "control: outside the sandbox the attribute is readable"
    );

    // The existence oracle STRENGTH.gaps reports: a name under the state
    // directory that exists is refused, one that does not is missing.
    let exists = run("/usr/bin/stat", sibling.clone());
    let missing = run("/usr/bin/stat", state.join("worktrees/no-such-worktree"));
    assert!(String::from_utf8_lossy(&exists.stderr).contains("Operation not permitted"));
    assert!(
        String::from_utf8_lossy(&missing.stderr).contains("No such file"),
        "{missing:?}"
    );
}

/// A fake vendor key in a checkout-root `.env`. Never a real key: the test
/// builds its own throwaway checkout.
const FAKE_ENV_KEY: &str = "sk-FAKEVENDORKEY0123456789abcdefFAKE";

/// Decision 0084: with `with_ancestor_reads(checkout)`, the checkout a
/// worktree lives in is unreadable to a build, except the cargo files cargo
/// reads on its way up. A worker's `build.rs` tries to read the checkout's
/// `.env` and a sibling file: both fail, while the checkout's
/// `.cargo/config.toml` (its `[env]` reaches the build script) and its
/// `Cargo.toml` (read while cargo looks for a workspace) stay readable, and
/// the build passes. The control run without the opt-in reads the key.
#[test]
fn a_worktree_build_cannot_read_the_checkout_env_but_reads_its_cargo_files() {
    let outer = tempfile::Builder::new()
        .prefix("Want To Develop ")
        .tempdir()
        .unwrap();
    let repo = outer.path().join("An App");
    let state = repo.join(".kgraf");
    let work = state.join("worktrees/run-w1-r0-a0");
    let scratch = state.join("worktrees/.scratch/run-w1-r0-a0");
    std::fs::create_dir_all(work.join("src")).unwrap();
    std::fs::create_dir_all(&scratch).unwrap();
    std::fs::create_dir_all(repo.join(".cargo")).unwrap();
    // The checkout is itself a package: cargo parses its manifest while it
    // looks for a workspace, and that parse needs `src/main.rs` to exist.
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(
        repo.join(".env"),
        format!("VENDOR_API_KEY={FAKE_ENV_KEY}\n"),
    )
    .unwrap();
    std::fs::write(repo.join("notes.txt"), "SIBLING-NOTES").unwrap();
    std::fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname = \"checkout\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(
        repo.join(".cargo/config.toml"),
        "[env]\nPROBE_FROM_CHECKOUT = \"checkout-config-read\"\n",
    )
    .unwrap();
    std::fs::write(
        work.join("Cargo.toml"),
        "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\nbuild = \"build.rs\"\n",
    )
    .unwrap();
    std::fs::write(work.join("src/main.rs"), "fn main() {}\n").unwrap();
    let canonical = kgraf_core::plain_path::canonicalize(&repo).unwrap();
    let env_path = canonical.join(".env");
    let notes_path = canonical.join("notes.txt");
    std::fs::write(
        work.join("build.rs"),
        format!(
            "fn main() {{\n\
             for p in [{env:?}, {notes:?}] {{\n\
               match std::fs::read_to_string(p) {{\n\
                 Ok(t) => println!(\"cargo:warning=READ {{}}\", t.trim()),\n\
                 Err(e) => println!(\"cargo:warning=DENIED {{}}\", e),\n\
               }}\n\
             }}\n\
             println!(\"cargo:warning=CONFIG {{}}\", \
               std::env::var(\"PROBE_FROM_CHECKOUT\").unwrap_or_default());\n\
             }}\n",
            env = env_path.to_str().unwrap(),
            notes = notes_path.to_str().unwrap(),
        ),
    )
    .unwrap();
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let build = |policy: &SandboxPolicy, target: &str| {
        let mut cmd = sandboxed_command(policy, &cargo, &work).unwrap();
        cmd.args(["build", "--offline"])
            .env("CARGO_TARGET_DIR", scratch.join(target));
        cmd.output().unwrap()
    };
    let base = SandboxPolicy::new(vec![work.clone(), scratch.clone()], Network::Deny)
        .with_tmp_dir(scratch.clone())
        .deny_read(state.clone());

    let control = build(&base, "control");
    let control_err = String::from_utf8_lossy(&control.stderr);
    assert!(control.status.success(), "control build: {control_err}");
    assert!(
        control_err.contains(FAKE_ENV_KEY),
        "control: without the opt-in the checkout .env is readable: {control_err}"
    );

    let confined = base.with_ancestor_reads(repo.clone());
    let out = build(&confined, "confined");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "confined build: {stderr}");
    assert!(!stderr.contains(FAKE_ENV_KEY), "{stderr}");
    assert!(!stderr.contains("SIBLING-NOTES"), "{stderr}");
    assert_eq!(stderr.matches("DENIED").count(), 2, "{stderr}");
    assert!(stderr.contains("CONFIG checkout-config-read"), "{stderr}");
}

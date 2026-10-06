//! Installed dependencies reach the acceptance check through the real
//! verifier path (decision 0084, amended 2026-10-06), with real tools: Node's
//! own test runner with a package from the checkout's `node_modules`, and a
//! Python test importing a module from the checkout's `.venv`.
//!
//! Layout as the host makes it: the checkout in a path with spaces, its
//! worktrees under `<checkout>/.kgraf/worktrees`, `.kgraf` read-denied, and
//! the dependency trees ignored by git, so no worktree has them. Before the
//! fix each check failed: the checkout is unreadable in the sandbox (macOS
//! denies it as a whole), so `require('dep')` and `import fixturedep` found
//! nothing.
//!
//! How to run: `cargo test -p kgraf-host dependency_links` on macOS, with
//! `node` and `python3` on `PATH` (the developer machine's tools; no network,
//! no package manager runs). `pytest_from_the_checkout_venv_passes` is
//! ignored by default: it installs pytest into a fixture venv with
//! `uv pip install --offline`, which needs pytest in uv's local cache; run it
//! with `--ignored`.

use super::*;

const GITIGNORE: &str = ".kgraf/\nnode_modules/\n.venv/\n";

/// A checkout in a path with spaces, with `files` committed, worktrees under
/// its own `.kgraf`, and the read denies the host passes.
fn checkout(files: &[(&str, &str)]) -> (Repo, Vec<PathBuf>) {
    let dir = tempfile::Builder::new()
        .prefix("Want To Develop ")
        .tempdir()
        .expect("dir");
    let root = dir.path().join("An App");
    let base = root.join(".kgraf/worktrees");
    let mut all = vec![(".gitignore", GITIGNORE)];
    all.extend_from_slice(files);
    let r = repo_in(dir, root.clone(), &base, &all);
    let state = std::fs::canonicalize(root.join(".kgraf")).expect("state");
    (r, vec![state])
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, text).expect("write");
}

/// Verify one attempt that writes `file` with `text` under `command`.
async fn verify_with(
    r: &Repo,
    denies: &[PathBuf],
    command: &str,
    file: &str,
    text: &str,
    worker: &str,
) -> (Verdict, PathBuf) {
    let c = named(file, worker);
    let ws = prepared(r, &c, worker).await;
    write(&ws.join(file), text);
    let v = CommandVerifier {
        read_denies: denies.to_vec(),
        ..verifier(r, command)
    };
    (v.verify(&c, &ws, &result()).await.expect("ran"), ws)
}

const NODE_TEST: &str = "const test = require('node:test');\n\
const assert = require('node:assert');\n\
const dep = require('dep');\n\
test('the dependency answers', () => assert.strictEqual(dep.answer, 42));\n";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_test_with_a_package_from_the_checkout_node_modules_passes() {
    let (r, denies) = checkout(&[(
        "package.json",
        r#"{"name":"fx","private":true,"scripts":{"test":"node --test"},"dependencies":{"dep":"1.0.0"}}"#,
    )]);
    write(
        &r.root.join("node_modules/dep/package.json"),
        r#"{"name":"dep","version":"1.0.0","main":"index.js"}"#,
    );
    write(
        &r.root.join("node_modules/dep/index.js"),
        "module.exports = { answer: 42 };\n",
    );
    let (verdict, ws) = verify_with(
        &r,
        &denies,
        "npm test",
        "test/dep.test.js",
        NODE_TEST,
        "w-node",
    )
    .await;
    let Verdict::Pass {
        commit: Some(_),
        notices,
        ..
    } = &verdict
    else {
        panic!("expected a pass, got {verdict:?}");
    };
    // The link is kGraf's, not the worker's or the check's: no notice.
    assert!(
        !notices.iter().any(|n| n.contains("node_modules")),
        "{notices:?}"
    );
    // The checkout's install is untouched, and the merged tree has no link.
    assert_eq!(
        std::fs::read_to_string(r.root.join("node_modules/dep/index.js")).expect("read"),
        "module.exports = { answer: 42 };\n"
    );
    let Integration::Merged { .. } = r
        .workspaces
        .integrate(&named("test/dep.test.js", "w-node"), &ws)
        .await
        .expect("merge")
    else {
        panic!("merge");
    };
    let tree = branch_tree(&r);
    assert!(tree.lines().any(|p| p == "test/dep.test.js"), "{tree}");
    assert!(
        !tree.lines().any(|p| p.starts_with("node_modules")),
        "{tree}"
    );
    r.workspaces.release(&ws, true).await;
    assert!(
        r.root.join("node_modules/dep/index.js").is_file(),
        "removing the worktree removed the link, never the checkout's install"
    );
}

/// A write through the link is denied by the sandbox: the checkout's install
/// stays as the user made it, and the check fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_check_cannot_write_the_checkout_install_through_the_link() {
    let (r, denies) = checkout(&[(
        "package.json",
        r#"{"name":"fx","private":true,"dependencies":{"dep":"1.0.0"}}"#,
    )]);
    write(&r.root.join("node_modules/dep/index.js"), "original\n");
    let (verdict, _ws) = verify_with(
        &r,
        &denies,
        "echo changed > node_modules/dep/index.js",
        "a.txt",
        "a\n",
        "w-write",
    )
    .await;
    let Verdict::Fail { reason } = &verdict else {
        panic!("expected a failure, got {verdict:?}");
    };
    assert!(reason.contains("Operation not permitted"), "{reason}");
    assert_eq!(
        std::fs::read_to_string(r.root.join("node_modules/dep/index.js")).expect("read"),
        "original\n"
    );
}

const FAILING_NODE_TEST: &str = "const test = require('node:test');\n\
const assert = require('node:assert');\n\
test('fails', () => assert.strictEqual(1, 2));\n";

/// The worker's scratch folder is its own `TMPDIR`, so it can leave files
/// there that the check in its tree would read as tool settings: npm's user
/// config (`.npmrc`, whose `script-shell` picks the program that runs the
/// test script), a Gradle or .NET home, a Go build cache, Python bytecode.
/// Here a planted `.npmrc` makes `npm test` run `/usr/bin/true`, a forged
/// pass of a failing test. The check gets a fresh scratch folder of its own,
/// so it fails as the test does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_settings_the_worker_leaves_in_its_scratch_folder_never_reach_the_check() {
    let (r, denies) = checkout(&[(
        "package.json",
        r#"{"name":"fx","private":true,"scripts":{"test":"node --test"}}"#,
    )]);
    let c = named("test/a.test.js", "w-plant");
    let ws = prepared(&r, &c, "w-plant").await;
    write(&ws.join("test/a.test.js"), FAILING_NODE_TEST);
    let scratch = ws
        .parent()
        .expect("base")
        .join(".scratch")
        .join(ws.file_name().expect("name"));
    assert!(scratch.is_dir(), "{}", scratch.display());
    write(&scratch.join(".npmrc"), "script-shell=/usr/bin/true\n");
    write(
        &scratch.join("gradle-home/init.d/pass.gradle"),
        "// would disable every test task\n",
    );
    let v = CommandVerifier {
        read_denies: denies.clone(),
        ..verifier(&r, "npm test")
    };
    let verdict = v.verify(&c, &ws, &result()).await.expect("ran");
    let Verdict::Fail { reason } = &verdict else {
        panic!("a planted .npmrc forged the check: {verdict:?}");
    };
    assert!(reason.contains("fails"), "{reason}");
    // And the settings homes a check sees are not the ones planted, nor the
    // worker's scratch folder itself.
    let live = r.workspaces.get(&ws).expect("live");
    // Neither the temporary folder nor the cargo target folder is in the
    // worker's scratch folder.
    let probe = format!(
        "test ! -e \"$GRADLE_USER_HOME/init.d/pass.gradle\" && \
         ! grep -q script-shell \"$NPM_CONFIG_USERCONFIG\" && \
         test -d \"$TMPDIR\" && \
         case \"$TMPDIR $CARGO_TARGET_DIR\" in *'{}'*) exit 1;; esac",
        scratch.display()
    );
    let (ok, out) = run_confined(
        &live,
        &r.root,
        &denies,
        &probe,
        Duration::from_secs(30),
        4_000,
        None,
    )
    .expect("ran");
    assert!(ok, "{out}");
    // The check's own folder is gone once it ends.
    let left: Vec<_> = std::fs::read_dir(live.checks_dir())
        .expect("checks")
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

/// Dependencies the project declares and the checkout lacks: the failure
/// names the missing install and its command, not only Node's error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_install_is_named_in_the_failure() {
    let (r, denies) = checkout(&[
        (
            "package.json",
            r#"{"name":"fx","private":true,"scripts":{"test":"node --test"},"dependencies":{"dep":"1.0.0"}}"#,
        ),
        ("package-lock.json", "{}"),
    ]);
    let (verdict, _ws) = verify_with(
        &r,
        &denies,
        "npm test",
        "test/dep.test.js",
        NODE_TEST,
        "w-missing",
    )
    .await;
    let Verdict::Fail { reason } = &verdict else {
        panic!("expected a failure, got {verdict:?}");
    };
    assert!(
        reason.contains("[dependencies] npm dependencies are not installed")
            && reason.contains("Run `npm ci` in the project root"),
        "{reason}"
    );
}

/// The `site-packages` folder of the virtual environment at `venv`.
fn site_packages(venv: &Path) -> PathBuf {
    let lib = venv.join("lib");
    std::fs::read_dir(&lib)
        .expect("lib")
        .flatten()
        .map(|e| e.path().join("site-packages"))
        .find(|p| p.is_dir())
        .expect("site-packages")
}

/// A real virtual environment in the checkout, made by the host's
/// `python3 -m venv` (no pip, no network).
fn make_venv(root: &Path) {
    let out = std::process::Command::new("python3")
        .args(["-m", "venv", "--without-pip", ".venv"])
        .current_dir(root)
        .output()
        .expect("python3");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_python_test_importing_from_the_checkout_venv_passes() {
    let (r, denies) = checkout(&[("pyproject.toml", "[project]\nname = \"fx\"\n")]);
    make_venv(&r.root);
    write(
        &site_packages(&r.root.join(".venv")).join("fixturedep.py"),
        "ANSWER = 42\n",
    );
    let test = "import sys, unittest\n\
import fixturedep\n\
class T(unittest.TestCase):\n    \
def test_answer(self):\n        \
self.assertEqual(fixturedep.ANSWER, 42)\n    \
def test_runs_in_the_venv(self):\n        \
self.assertTrue(sys.prefix.endswith('.venv'), sys.prefix)\n";
    let (verdict, _ws) = verify_with(
        &r,
        &denies,
        "python3 -m unittest discover -s tests -v",
        "tests/test_dep.py",
        test,
        "w-venv",
    )
    .await;
    assert!(
        matches!(
            verdict,
            Verdict::Pass {
                commit: Some(_),
                ..
            }
        ),
        "expected a pass, got {verdict:?}"
    );
}

/// A virtual environment below the root, in the project the command works
/// in (`cd svc && ...`), is the one activated: its packages import, and
/// the interpreter runs in it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_virtual_environment_of_the_command_s_folder_is_activated() {
    let (r, denies) = checkout(&[("svc/pyproject.toml", "[project]\nname = \"svc\"\n")]);
    make_venv(&r.root.join("svc"));
    write(
        &site_packages(&r.root.join("svc/.venv")).join("fixturedep.py"),
        "ANSWER = 42\n",
    );
    let test = "import sys, unittest\n\
import fixturedep\n\
class T(unittest.TestCase):\n    \
def test_answer(self):\n        \
self.assertEqual(fixturedep.ANSWER, 42)\n    \
def test_runs_in_the_venv(self):\n        \
self.assertTrue(sys.prefix.endswith('svc/.venv'), sys.prefix)\n";
    let (verdict, _ws) = verify_with(
        &r,
        &denies,
        "cd svc && python3 -m unittest discover -s tests -v",
        "svc/tests/test_dep.py",
        test,
        "w-subvenv",
    )
    .await;
    assert!(
        matches!(
            verdict,
            Verdict::Pass {
                commit: Some(_),
                ..
            }
        ),
        "expected a pass, got {verdict:?}"
    );
}

/// A pnpm workspace, laid out by hand as pnpm documents it (pnpm is not
/// installed here): a third-party package in `node_modules/.pnpm` requires
/// the workspace package `@fx/util`, which pnpm links in its hidden
/// hoisting folder `.pnpm/node_modules`. Node resolves `dep` from its real
/// path; through links into the checkout that path was the checkout's
/// `.pnpm`, so `@fx/util` was the checkout's copy (unreadable in the
/// sandbox). The tree gets a clone of the install, and the worker's edit of
/// `packages/util` is what `dep` loads.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pnpm_package_requiring_a_workspace_package_gets_the_tree_s_copy() {
    let (r, denies) = checkout(&[
        (
            "package.json",
            r#"{"name":"fx","private":true,"scripts":{"test":"node --test"}}"#,
        ),
        ("pnpm-workspace.yaml", "packages:\n  - packages/*\n"),
        (
            "packages/util/package.json",
            r#"{"name":"@fx/util","version":"1.0.0","main":"index.js"}"#,
        ),
        ("packages/util/index.js", "module.exports = 'base';\n"),
    ]);
    let nm = r.root.join("node_modules");
    write(&nm.join(".modules.yaml"), "layoutVersion: 5\n");
    let store = nm.join(".pnpm/dep@1.0.0/node_modules/dep");
    write(
        &store.join("package.json"),
        r#"{"name":"dep","version":"1.0.0","main":"index.js"}"#,
    );
    write(
        &store.join("index.js"),
        "module.exports = require('@fx/util');\n",
    );
    let link = |target: &str, at: &Path| {
        std::fs::create_dir_all(at.parent().expect("parent")).expect("mkdir");
        std::os::unix::fs::symlink(target, at).expect("link");
    };
    link(
        "../../../../packages/util",
        &nm.join(".pnpm/node_modules/@fx/util"),
    );
    link(".pnpm/dep@1.0.0/node_modules/dep", &nm.join("dep"));
    link("../../packages/util", &nm.join("@fx/util"));

    let mut c = named("packages/util/index.js", "w-pnpm");
    c.writes.push(kgraf_orchestrator::WriteTarget {
        path: "test/dep.test.js".into(),
        node: None,
    });
    let ws = prepared(&r, &c, "w-pnpm").await;
    write(
        &ws.join("packages/util/index.js"),
        "module.exports = 'edited';\n",
    );
    write(
        &ws.join("test/dep.test.js"),
        "const test = require('node:test');\n\
const assert = require('node:assert');\n\
test('dep loads the tree copy of the workspace package', () => \
assert.strictEqual(require('dep'), 'edited'));\n",
    );
    let v = CommandVerifier {
        read_denies: denies,
        ..verifier(&r, "npm test")
    };
    let verdict = v.verify(&c, &ws, &result()).await.expect("ran");
    assert!(
        matches!(
            verdict,
            Verdict::Pass {
                commit: Some(_),
                ..
            }
        ),
        "expected a pass, got {verdict:?}"
    );
    // The checkout's install is as the user made it.
    assert!(
        std::fs::symlink_metadata(nm.join("dep"))
            .expect("dep")
            .file_type()
            .is_symlink()
    );
    assert!(!nm.join(kgraf_sandbox::MIRROR_MARK).exists());
}

/// An npm workspace: the checkout's `node_modules/@fx/web` is a link to
/// `../../packages/web`. The test requires `@fx/web` and passes only with
/// the tree's copy of the package, which the commit holds (the checkout's
/// working copy says otherwise, and is unreadable anyway).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_npm_workspace_package_resolves_to_the_tree_s_copy() {
    let (r, denies) = checkout(&[
        (
            "package.json",
            r#"{"name":"fx","private":true,"workspaces":["packages/*"],"scripts":{"test":"node --test"}}"#,
        ),
        (
            "packages/web/package.json",
            r#"{"name":"@fx/web","version":"1.0.0","main":"index.js"}"#,
        ),
        (
            "packages/web/index.js",
            "module.exports = { answer: 42 };\n",
        ),
    ]);
    // The user's uncommitted edit in the checkout: never what the tree runs.
    write(
        &r.root.join("packages/web/index.js"),
        "module.exports = { answer: 1 };\n",
    );
    write(
        &r.root.join("node_modules/dep/package.json"),
        r#"{"name":"dep","version":"1.0.0","main":"index.js"}"#,
    );
    write(
        &r.root.join("node_modules/dep/index.js"),
        "module.exports = { answer: 42 };\n",
    );
    std::fs::create_dir_all(r.root.join("node_modules/@fx")).expect("mkdir");
    std::os::unix::fs::symlink("../../packages/web", r.root.join("node_modules/@fx/web"))
        .expect("link");
    let test = "const test = require('node:test');\n\
const assert = require('node:assert');\n\
test('the workspace package is the tree copy', () => \
assert.strictEqual(require('@fx/web').answer, 42));\n\
test('the installed dependency answers', () => \
assert.strictEqual(require('dep').answer, 42));\n";
    let (verdict, ws) = verify_with(
        &r,
        &denies,
        "npm test",
        "test/web.test.js",
        test,
        "w-workspace",
    )
    .await;
    assert!(
        matches!(
            verdict,
            Verdict::Pass {
                commit: Some(_),
                ..
            }
        ),
        "expected a pass, got {verdict:?}"
    );
    assert!(
        std::fs::symlink_metadata(ws.join("node_modules"))
            .expect("node_modules")
            .is_dir()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs pytest in uv's local cache (uv pip install --offline)"]
async fn pytest_from_the_checkout_venv_passes() {
    let (r, denies) = checkout(&[(
        "pyproject.toml",
        "[project]\nname = \"fx\"\n[tool.pytest.ini_options]\naddopts = \"-q\"\n",
    )]);
    make_venv(&r.root);
    let out = std::process::Command::new("uv")
        .args([
            "pip",
            "install",
            "--offline",
            "--quiet",
            "--python",
            ".venv/bin/python",
            "pytest",
        ])
        .current_dir(&r.root)
        .output()
        .expect("uv");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    write(
        &site_packages(&r.root.join(".venv")).join("fixturedep.py"),
        "ANSWER = 42\n",
    );
    let (verdict, _ws) = verify_with(
        &r,
        &denies,
        "python3 -m pytest",
        "tests/test_dep.py",
        "import fixturedep\n\ndef test_answer():\n    assert fixturedep.ANSWER == 42\n",
        "w-pytest",
    )
    .await;
    assert!(
        matches!(
            verdict,
            Verdict::Pass {
                commit: Some(_),
                ..
            }
        ),
        "expected a pass, got {verdict:?}"
    );
}

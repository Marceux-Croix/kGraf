//! Package stores and installed dependency trees against the real macOS
//! Seatbelt backend (decision 0084, amended 2026-10-06), with a throwaway
//! HOME holding fake credentials. One test per binary, because it sets HOME
//! before any other thread exists.
//!
//! What it shows: the package stores in HOME are readable and the credential
//! files beside them are not; a checkout's `node_modules`, linked into a
//! tree, is readable there while the rest of the checkout stays denied; a
//! link inside it that points at a credential file is refused at its
//! target; and nothing is written through the link.

#![cfg(target_os = "macos")]

use std::path::Path;

use kgraf_sandbox::{Network, SandboxPolicy, link_dependency_trees, sandboxed_command};

const FAKE: &str = "FAKE-CREDENTIAL-FOR-TEST";

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, text).expect("write");
}

#[test]
fn stores_and_dependency_trees_are_readable_and_credentials_beside_them_are_not() {
    let home = tempfile::tempdir().expect("home");
    let h = kgraf_core::plain_path::canonicalize(home.path()).expect("canonical");
    // SAFETY: the only test of this binary, run before it starts a thread.
    unsafe { std::env::set_var("HOME", &h) };
    let stores = [
        ".m2/repository/org/x/x.jar",
        ".gradle/caches/modules-2/files/x.jar",
        ".gradle/wrapper/dists/gradle-8/bin/gradle",
        ".nuget/packages/x/1.0/x.nupkg",
        ".npm/_cacache/index-v5/x",
        ".cache/pip/wheels/x.whl",
        "go/pkg/mod/example.com/m@v1.0.0/m.go",
        ".cargo/registry/index/x",
    ];
    let credentials = [
        ".m2/settings.xml",
        ".m2/settings-security.xml",
        ".gradle/gradle.properties",
        ".gradle/init.d/repo.gradle",
        ".nuget/NuGet/NuGet.Config",
        ".config/NuGet/NuGet.Config",
        ".npmrc",
        ".pip/pip.conf",
        ".cargo/credentials.toml",
        ".ssh/id_test",
        // Logins of the other package managers and services a build or a
        // test may touch.
        ".docker/config.json",
        ".netrc",
        ".git-credentials",
        ".config/git/credentials",
        ".pypirc",
        ".config/pip/pip.conf",
        "Library/Application Support/pip/pip.conf",
        ".yarnrc",
        ".yarnrc.yml",
        ".gradle/init.gradle",
        ".gradle/init.gradle.kts",
        ".gradle/enterprise/keys.properties",
        ".gradle/develocity/keys.properties",
        "Library/Preferences/pnpm/rc",
        ".bunfig.toml",
        "Library/Application Support/pypoetry/auth.toml",
        ".local/share/uv/credentials/credentials.toml",
    ];
    for rel in stores {
        write(&h.join(rel), "ok");
    }
    for rel in credentials {
        write(&h.join(rel), FAKE);
    }

    let checkout = tempfile::tempdir().expect("checkout");
    let c = kgraf_core::plain_path::canonicalize(checkout.path()).expect("canonical");
    write(&c.join("package.json"), "{}");
    write(&c.join("node_modules/dep/index.js"), "dep");
    write(&c.join(".env"), FAKE);
    std::os::unix::fs::symlink(h.join(".ssh/id_test"), c.join("node_modules/evil")).expect("link");
    std::os::unix::fs::symlink(c.join(".env"), c.join("node_modules/dotenv-link")).expect("link");

    let tree = tempfile::tempdir().expect("tree");
    let t = kgraf_core::plain_path::canonicalize(tree.path()).expect("canonical");
    write(&t.join("package.json"), "{}");
    let links = link_dependency_trees(&t, &c);
    assert_eq!(links.reads, vec![c.join("node_modules")], "{links:?}");

    let policy = SandboxPolicy::new(vec![t.clone()], Network::Deny)
        .with_ancestor_reads(c.clone())
        .with_dependency_links(&links);
    let cat = |path: &Path| {
        sandboxed_command(&policy, "/bin/cat", &t)
            .expect("command")
            .arg(path)
            .output()
            .expect("ran")
    };
    for rel in stores {
        let out = cat(&h.join(rel));
        assert_eq!(String::from_utf8_lossy(&out.stdout), "ok", "{rel}: {out:?}");
    }
    // Every credential file read, named at once.
    let read: Vec<&str> = credentials
        .iter()
        .copied()
        .filter(|rel| {
            let out = cat(&h.join(rel));
            out.status.success() || String::from_utf8_lossy(&out.stdout).contains("FAKE")
        })
        .collect();
    assert!(read.is_empty(), "credential files read: {read:?}");
    let dep = cat(&t.join("node_modules/dep/index.js"));
    assert_eq!(String::from_utf8_lossy(&dep.stdout), "dep", "{dep:?}");
    for escape in ["node_modules/evil", "node_modules/dotenv-link"] {
        let out = cat(&t.join(escape));
        assert!(!out.status.success(), "{escape} was read: {out:?}");
        assert!(
            !String::from_utf8_lossy(&out.stdout).contains("FAKE"),
            "{escape}"
        );
    }
    let env = cat(&c.join(".env"));
    assert!(
        !env.status.success(),
        "the checkout outside its trees was read: {env:?}"
    );

    let wrote = sandboxed_command(&policy, "/bin/sh", &t)
        .expect("command")
        .args(["-c", "echo changed > node_modules/dep/index.js"])
        .output()
        .expect("ran");
    assert!(!wrote.status.success(), "{wrote:?}");
    assert!(
        String::from_utf8_lossy(&wrote.stderr).contains("Operation not permitted"),
        "{wrote:?}"
    );
    assert_eq!(
        std::fs::read_to_string(c.join("node_modules/dep/index.js")).expect("read"),
        "dep"
    );

    // The Gradle wrapper: the command's own Gradle home reaches the user's
    // unpacked distribution read-only, the wrapper's lock goes in the
    // command's home, and nothing is written in the user's.
    let dist = h.join(".gradle/wrapper/dists/gradle-8.5-bin/abc123");
    write(&dist.join("gradle-8.5/lib/gradle-launcher.jar"), "jar");
    write(&dist.join("gradle-8.5-bin.zip.ok"), "");
    write(
        &t.join("gradle/wrapper/gradle-wrapper.properties"),
        "distributionUrl=https\\://services.gradle.org/distributions/gradle-8.5-bin.zip\n",
    );
    std::fs::create_dir_all(t.join(".tmp")).expect("tmp");
    let with_tmp = SandboxPolicy::new(vec![t.clone()], Network::Deny).with_tmp_dir(t.join(".tmp"));
    let wrapper = sandboxed_command(&with_tmp, "/bin/sh", &t)
        .expect("command")
        .args([
            "-c",
            "d=\"$GRADLE_USER_HOME/wrapper/dists/gradle-8.5-bin/abc123\" && \
             test -f \"$d/gradle-8.5-bin.zip.ok\" && \
             cat \"$d/gradle-8.5/lib/gradle-launcher.jar\" && \
             : > \"$d/gradle-8.5-bin.zip.lck\"",
        ])
        .output()
        .expect("ran");
    assert!(wrapper.status.success(), "{wrapper:?}");
    assert_eq!(String::from_utf8_lossy(&wrapper.stdout), "jar");
    assert!(!dist.join("gradle-8.5-bin.zip.lck").exists());
    let user_lock = sandboxed_command(&with_tmp, "/bin/sh", &t)
        .expect("command")
        .arg("-c")
        .arg(format!(
            ": > '{}'",
            dist.join("gradle-8.5-bin.zip.lck").display()
        ))
        .output()
        .expect("ran");
    assert!(!user_lock.status.success(), "{user_lock:?}");
}

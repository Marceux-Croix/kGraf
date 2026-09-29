//! Decision 0087 through the host's own entry points: the dependency gate
//! runs before anything of the run is built, on every path that starts one.
//! A fake package manager stands in for cargo; no test contacts a registry.

use crate::deps::{
    Consent, DependencyTool, DepsError, FetchReport, FetchSite, Readiness, Registry, RunFetch,
};
use crate::{Error, Host, HostFactory, Notice, RunConfig};
use kgraf_core::RunId;
use kgraf_orchestrator::{OrchestratorError, OrchestratorFactory};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A package manager whose cache is a flag. At each fetch it records whether
/// the run's integration branch already existed: the fetch must come first.
struct Fake {
    root: PathBuf,
    ready: Mutex<bool>,
    log: Mutex<Vec<String>>,
}

impl Fake {
    fn missing(root: &Path) -> Arc<Self> {
        Arc::new(Self {
            root: root.to_path_buf(),
            ready: Mutex::new(false),
            log: Mutex::new(Vec::new()),
        })
    }
    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
    fn fetched(&self) -> bool {
        self.log().iter().any(|l| l.starts_with("fetch"))
    }
}

impl DependencyTool for Fake {
    fn registry(&self) -> Registry {
        Registry::CratesIo
    }
    fn applies(&self, _site: &FetchSite) -> bool {
        true
    }
    fn check(&self, site: &FetchSite) -> Readiness {
        assert_eq!(site.source, self.root, "the gate checks the project itself");
        self.log.lock().unwrap().push("check".into());
        if *self.ready.lock().unwrap() {
            Readiness::Ready
        } else {
            Readiness::Missing("no matching package named `itoa`".into())
        }
    }
    fn fetch(
        &self,
        _site: &FetchSite,
        _stop: &crate::deps::Stop,
    ) -> Result<FetchReport, DepsError> {
        let branches = git(&self.root, &["branch", "--list", "kgraf/*"]);
        self.log
            .lock()
            .unwrap()
            .push(format!("fetch branches=[{}]", branches.trim()));
        *self.ready.lock().unwrap() = true;
        Ok(FetchReport {
            registry: Registry::CratesIo,
            contacted: vec!["Updating crates.io index".into()],
            downloaded: 3,
            elapsed: Duration::from_millis(1),
        })
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git");
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn project() -> (tempfile::TempDir, RunConfig) {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).expect("mkdir");
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.name", "Test Owner"]);
    git(&root, &["config", "user.email", "owner@example.invalid"]);
    std::fs::write(root.join("README.md"), "# t\n").expect("write");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "init"]);
    let prices = dir.path().join("prices.json");
    std::fs::write(
        &prices,
        r#"{"llama3": {"uncached_input": 1, "cache_read": 1, "cache_write": 1, "output": 1, "reasoning": 1}}"#,
    )
    .expect("prices");
    let config = RunConfig::new(&root, vec!["ollama:llama3".into()], prices);
    (dir, config)
}

fn host_with(config: RunConfig, fake: &Arc<Fake>) -> Host {
    Host::open(config)
        .expect("open")
        .with_dependency_tool(fake.clone())
}

fn branch_exists(root: &Path, run: &str) -> bool {
    !git(root, &["branch", "--list", &format!("kgraf/{run}")])
        .trim()
        .is_empty()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unset_and_unattended_fails_loud_before_anything_is_built() {
    let (_dir, config) = project();
    let root = config.root.clone();
    let fake = Fake::missing(&root);
    let host = host_with(config, &fake);
    let err = match host.prepare(RunId::new("d1"), None, &mut |_| {}).await {
        Ok(_) => panic!("the run was built"),
        Err(e) => e,
    };
    let q = err.dependency_question().expect("a question to ask");
    assert_eq!(q.registry, Registry::CratesIo);
    assert!(q.text().contains("crates.io"), "{}", q.text());
    let text = format!("{err:#}");
    for needle in ["kgraf deps allow", "--fetch-deps", "cargo fetch"] {
        assert!(text.contains(needle), "`{needle}` missing: {text}");
    }
    assert!(!err.is_transient());
    assert!(!fake.fetched(), "{:?}", fake.log());
    assert!(!branch_exists(&root, "d1"), "nothing of the run was built");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stored_allowed_fetches_before_the_run_is_built_and_says_so() {
    let (_dir, config) = project();
    let root = config.root.clone();
    let fake = Fake::missing(&root);
    let host = host_with(config, &fake);
    host.set_dependency_consent(Registry::CratesIo, Consent::Allowed)
        .expect("store");
    let mut notices = Vec::new();
    let prepared = host
        .prepare(RunId::new("d2"), None, &mut |n| notices.push(n))
        .await
        .expect("prepared");
    assert_eq!(
        fake.log(),
        ["check", "fetch branches=[]", "check"],
        "fetched before the integration branch existed"
    );
    assert!(branch_exists(&root, "d2"));
    assert!(
        notices.iter().any(|n| matches!(n,
            Notice::DependenciesFetched { registry, summary }
                if registry == "crates.io" && summary.contains("Updating crates.io index"))),
        "{notices:?}"
    );
    drop(prepared);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stored_refused_fails_loud_and_a_flag_overrides_it_for_one_run() {
    let (_dir, config) = project();
    let root = config.root.clone();
    let fake = Fake::missing(&root);
    let host = host_with(config.clone(), &fake);
    host.set_dependency_consent(Registry::CratesIo, Consent::Refused)
        .expect("store");
    let err = match host.prepare(RunId::new("d3"), None, &mut |_| {}).await {
        Ok(_) => panic!("built"),
        Err(e) => e,
    };
    assert!(
        matches!(
            &err,
            Error::Dependencies(DepsError::Refused {
                for_this_run: false,
                ..
            })
        ),
        "{err}"
    );
    assert!(
        err.dependency_question().is_none(),
        "a refusal is not asked again"
    );
    assert!(!fake.fetched());
    assert!(!branch_exists(&root, "d3"));

    // `--fetch-deps` allows this run only, and stores nothing.
    let flagged = RunConfig {
        dependency_fetch: RunFetch::Allow,
        ..config
    };
    let host = host_with(flagged, &fake);
    host.prepare(RunId::new("d4"), None, &mut |_| {})
        .await
        .expect("prepared");
    assert!(fake.fetched());
    assert_eq!(
        host.dependency_consents().get(Registry::CratesIo).unwrap(),
        Consent::Refused
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_fetch_deps_refuses_even_when_the_project_allows() {
    let (_dir, config) = project();
    let root = config.root.clone();
    let fake = Fake::missing(&root);
    let host = host_with(
        RunConfig {
            dependency_fetch: RunFetch::Refuse,
            ..config
        },
        &fake,
    );
    host.set_dependency_consent(Registry::CratesIo, Consent::Allowed)
        .expect("store");
    let err = match host.prepare(RunId::new("d5"), None, &mut |_| {}).await {
        Ok(_) => panic!("built"),
        Err(e) => e,
    };
    assert!(format!("{err}").contains("--no-fetch-deps"), "{err}");
    assert!(!fake.fetched());
}

/// The desktop's "Allow once": a grant for one run id, not for the next.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_grant_covers_its_own_run_only_until_it_finishes() {
    let (_dir, config) = project();
    let root = config.root.clone();
    let fake = Fake::missing(&root);
    let host = host_with(config, &fake);
    host.grant_dependency_fetch(&RunId::new("other"));
    assert!(
        host.prepare(RunId::new("d6"), None, &mut |_| {})
            .await
            .is_err()
    );
    assert!(!fake.fetched());
    host.grant_dependency_fetch(&RunId::new("d6"));
    host.prepare(RunId::new("d6"), None, &mut |_| {})
        .await
        .expect("prepared");
    assert!(fake.fetched());
    assert_eq!(
        host.dependency_consents().get(Registry::CratesIo).unwrap(),
        Consent::Unset,
        "allow once stores nothing"
    );
    host.finish_run(&RunId::new("d6"));
    *fake.ready.lock().unwrap() = false;
    let again = match host.prepare(RunId::new("d6"), None, &mut |_| {}).await {
        Ok(_) => panic!("the grant outlived its run"),
        Err(e) => e,
    };
    assert!(again.dependency_question().is_some());
}

/// `Host::finish_run` stops the run's refetcher: a worker's fetch still
/// running is killed, and no later request of that run fetches.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn finishing_a_run_stops_its_refetcher() {
    let (_dir, config) = project();
    let root = config.root.clone();
    let fake = Fake::missing(&root);
    *fake.ready.lock().unwrap() = true;
    let host = host_with(config, &fake);
    let run = RunId::new("d9");
    let prepared = host
        .prepare(run.clone(), None, &mut |_| {})
        .await
        .expect("prepared");
    let refetch = host.run_refetcher(&run).expect("the run has a refetcher");
    host.finish_run(&run);
    assert!(host.run_refetcher(&run).is_none());
    let out = refetch.refetch_blocking(&root);
    assert!(
        matches!(&out, kgraf_tools::RefetchOutcome::Failed { reason } if reason.contains("ended")),
        "{out:?}"
    );
    drop(prepared);
}

/// The scheduler's and the desktop's path (`HostFactory::build`, through
/// `prepare_blocking`) passes the same gate, and a missing answer is a
/// settings failure, not a transient one to retry.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_factory_path_passes_the_same_gate() {
    let (_dir, config) = project();
    let root = config.root.clone();
    let fake = Fake::missing(&root);
    let host = Arc::new(host_with(config, &fake));
    let factory = HostFactory::new(host.clone());
    match factory.build(&RunId::new("f1")) {
        Err(OrchestratorError::Build { transient, detail }) => {
            assert!(!transient, "{detail}");
            assert!(detail.contains("kgraf deps allow"), "{detail}");
        }
        Err(other) => panic!("wrong error: {other}"),
        Ok(_) => panic!("built"),
    }
    assert!(!fake.fetched());
    assert!(!branch_exists(&root, "f1"));
    host.set_dependency_consent(Registry::CratesIo, Consent::Allowed)
        .expect("store");
    let built = factory.build(&RunId::new("f2"));
    assert!(built.is_ok(), "{:?}", built.err().map(|e| e.to_string()));
    assert!(
        fake.log().contains(&"fetch branches=[]".to_owned()),
        "{:?}",
        fake.log()
    );
    assert!(
        host.take_notices()
            .iter()
            .any(|n| matches!(n, Notice::DependenciesFetched { .. })),
        "the fetch is said on the factory path too"
    );
}

/// Ready dependencies need no consent file and no fetch: the common case.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ready_dependencies_pass_without_asking() {
    let (_dir, config) = project();
    let root = config.root.clone();
    let fake = Fake::missing(&root);
    *fake.ready.lock().unwrap() = true;
    let host = host_with(config, &fake);
    host.prepare(RunId::new("d7"), None, &mut |_| {})
        .await
        .expect("prepared");
    assert_eq!(fake.log(), ["check"]);
    assert!(!host.dependency_consents().path().exists());
}

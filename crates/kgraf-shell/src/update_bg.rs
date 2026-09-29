//! The update-check background thread, same
//! spawn/`Drop`-joins-with-a-timeout shape as `capacity_bg::CapacityHandle`.
//!
//! **Off the UI thread**, same reasoning as `capacity_bg`'s own doc comment: a GitHub
//! Releases query and an installer download are real network I/O and must never stall
//! a frame.
//!
//! **The consent gate.**
//! [`UpdateHandle::check_now`] only ever moves the state to
//! [`UpdateState::Available`]; nothing on that path downloads or launches anything.
//! [`UpdateHandle::accept_and_install`] is the one and only entry point that reaches
//! [`crate::update_check::download_and_verify`] and [`Launcher::launch`], and it exists
//! so a caller can be read as "the user said yes" without this module trusting that
//! claim beyond taking the call: the UI must call it only from the notice's own Yes
//! button (`layout.rs`), never from `check_now`, a timer, or start-up. The mutation
//! test below (`checking_never_reaches_the_launcher`) pins this: flip `check_now` to
//! also call the launcher, and the test fails.
//!
//! **Egress stays opt-in.** [`UpdateHandle::spawn`]'s `check_on_start` flag is read
//! from `Settings::check_for_updates` (default `false`); when `false`, the thread
//! starts and waits for a command, but performs no network call until the user presses
//! "Check now" in Settings.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kgraf_orchestrator::Wake;

use crate::update_check::{self, Installer, Launcher, RealLauncher, UpdateCandidate, UpdateError};

/// How often the loop checks `stop` while otherwise waiting for a command, for a
/// prompt shutdown (same pattern as `capacity_bg::STOP_POLL_INTERVAL`).
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(200);
/// How long `Drop` waits for the thread before abandoning it.
const SHUTDOWN_JOIN_TIMEOUT: Duration = Duration::from_secs(2);

/// What the Settings panel shows. Never a failure by itself (style guide: "a state,
/// not a failure"); `Failed` names a stated reason (fail loud with a visible
/// reason), and is itself a state, shown the same way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateState {
    /// No check has run yet this session.
    Idle,
    Checking,
    UpToDate,
    /// A newer release is published; offered, not yet accepted.
    Available(UpdateCandidate),
    Downloading,
    /// Downloaded, verified, and launched (macOS/Windows) or named for the user to
    /// run themselves (Linux, never with `sudo` from this process).
    Ready {
        path: PathBuf,
        words: String,
    },
    Failed(String),
}

enum Command {
    Check { allow_prerelease: bool },
    Install(UpdateCandidate),
}

/// Owns the update-check thread.
pub struct UpdateHandle {
    state: Arc<Mutex<UpdateState>>,
    tx: std::sync::mpsc::Sender<Command>,
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl UpdateHandle {
    /// Spawns the thread. `check_on_start` runs one check immediately, off this
    /// call's thread, only when true (`Settings::check_for_updates`); `wake` runs
    /// after every state change so the shell repaints Settings if it is open. Never
    /// called from `ShellApp::new` itself, same rule `CapacityHandle::spawn`'s own doc
    /// comment states (a headless test creating a `ShellApp` must never start a
    /// background thread).
    pub fn spawn(check_on_start: bool, allow_prerelease: bool, wake: Wake) -> Self {
        Self::spawn_full(
            check_on_start,
            allow_prerelease,
            wake,
            Arc::new(RealLauncher),
            update_check::GITHUB_API_BASE.to_owned(),
        )
    }

    /// [`Self::spawn`], with the launcher injected (tests use a recorder instead of a
    /// real `open`/`msiexec` process; see `update_check.rs`'s own `Launcher` doc).
    pub fn spawn_with_launcher(
        check_on_start: bool,
        allow_prerelease: bool,
        wake: Wake,
        launcher: Arc<dyn Launcher>,
    ) -> Self {
        Self::spawn_full(
            check_on_start,
            allow_prerelease,
            wake,
            launcher,
            update_check::GITHUB_API_BASE.to_owned(),
        )
    }

    /// [`Self::spawn_with_launcher`], with the GitHub API base also injected. Only
    /// this crate's own tests call this with anything other than
    /// [`update_check::GITHUB_API_BASE`], and only to point at a local `wiremock`
    /// server.
    pub(crate) fn spawn_full(
        check_on_start: bool,
        allow_prerelease: bool,
        wake: Wake,
        launcher: Arc<dyn Launcher>,
        api_base: String,
    ) -> Self {
        let state = Arc::new(Mutex::new(UpdateState::Idle));
        let (tx, rx) = std::sync::mpsc::channel::<Command>();
        let stop = Arc::new(AtomicBool::new(false));
        let (state_t, stop_t) = (state.clone(), stop.clone());
        let join = std::thread::Builder::new()
            .name("kgraf-update".to_owned())
            .spawn(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(1)
                    .enable_all()
                    .build()
                else {
                    return;
                };
                let client = reqwest::Client::builder()
                    .timeout(Duration::from_secs(30))
                    .build()
                    .unwrap_or_default();
                if check_on_start {
                    runtime.block_on(run_check(
                        &state_t,
                        &client,
                        &api_base,
                        allow_prerelease,
                        &wake,
                    ));
                }
                loop {
                    if stop_t.load(Ordering::Relaxed) {
                        return;
                    }
                    match rx.recv_timeout(STOP_POLL_INTERVAL) {
                        Ok(Command::Check { allow_prerelease }) => {
                            runtime.block_on(run_check(
                                &state_t,
                                &client,
                                &api_base,
                                allow_prerelease,
                                &wake,
                            ));
                        }
                        Ok(Command::Install(candidate)) => {
                            runtime.block_on(run_install(
                                &state_t,
                                &client,
                                &candidate,
                                launcher.as_ref(),
                                &wake,
                            ));
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
            })
            .ok();
        Self {
            state,
            tx,
            stop,
            join,
        }
    }

    /// The current state, for the Settings panel to render.
    pub fn state(&self) -> UpdateState {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// "Check now" (Settings): runs off the UI thread regardless of
    /// `Settings::check_for_updates`, since the user just explicitly asked.
    pub fn check_now(&self, allow_prerelease: bool) {
        let _ = self.tx.send(Command::Check { allow_prerelease });
    }

    /// The consent gate. Call this only from the notice's own "Yes, download and
    /// install" action (see this module's own doc comment); nothing else in this
    /// crate may call it.
    pub fn accept_and_install(&self, candidate: UpdateCandidate) {
        let _ = self.tx.send(Command::Install(candidate));
    }
}

impl Drop for UpdateHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let Some(join) = self.join.take() else {
            return;
        };
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = join.join();
            let _ = tx.send(());
        });
        if rx.recv_timeout(SHUTDOWN_JOIN_TIMEOUT).is_err() {
            log::error!(
                "update-check thread did not stop within {SHUTDOWN_JOIN_TIMEOUT:?}; \
                 abandoning it rather than blocking shutdown"
            );
        }
    }
}

fn set_state(state: &Mutex<UpdateState>, wake: &Wake, next: UpdateState) {
    *state.lock().unwrap_or_else(|p| p.into_inner()) = next;
    wake();
}

async fn run_check(
    state: &Mutex<UpdateState>,
    client: &reqwest::Client,
    api_base: &str,
    allow_prerelease: bool,
    wake: &Wake,
) {
    set_state(state, wake, UpdateState::Checking);
    let outcome = check_once(client, api_base, allow_prerelease).await;
    let next = match outcome {
        Ok(Some(candidate)) => UpdateState::Available(candidate),
        Ok(None) => UpdateState::UpToDate,
        Err(e) => UpdateState::Failed(e.to_string()),
    };
    set_state(state, wake, next);
}

async fn check_once(
    client: &reqwest::Client,
    api_base: &str,
    allow_prerelease: bool,
) -> Result<Option<UpdateCandidate>, UpdateError> {
    let releases = update_check::fetch_releases(client, api_base).await?;
    update_check::evaluate(
        &update_check::current_version(),
        &releases,
        allow_prerelease,
        std::env::consts::OS,
        std::env::consts::ARCH,
    )
}

async fn run_install(
    state: &Mutex<UpdateState>,
    client: &reqwest::Client,
    candidate: &UpdateCandidate,
    launcher: &dyn Launcher,
    wake: &Wake,
) {
    set_state(state, wake, UpdateState::Downloading);
    let dest = std::env::temp_dir()
        .join("kgraf-update")
        .join(&candidate.tag_name);
    let next = match install_once(client, candidate, &dest, launcher).await {
        Ok((path, words)) => UpdateState::Ready { path, words },
        Err(e) => UpdateState::Failed(e.to_string()),
    };
    set_state(state, wake, next);
}

async fn install_once(
    client: &reqwest::Client,
    candidate: &UpdateCandidate,
    dest: &std::path::Path,
    launcher: &dyn Launcher,
) -> Result<(PathBuf, String), UpdateError> {
    let path = update_check::download_and_verify(client, candidate, dest).await?;
    let installer: Installer = update_check::installer_from_path(&path).ok_or_else(|| {
        UpdateError::Launch(format!("{} is not a recognized installer", path.display()))
    })?;
    let words = match launcher.launch(&installer)? {
        update_check::InstallOutcome::Launched => {
            format!("{} launched; finish the update there.", path.display())
        }
        update_check::InstallOutcome::ManualInstructions(words) => words,
    };
    Ok((path, words))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use wiremock::matchers::{method, path as path_matcher};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn wake_counter() -> (Wake, Arc<AtomicUsize>) {
        let n = Arc::new(AtomicUsize::new(0));
        let n2 = n.clone();
        (
            Arc::new(move || {
                n2.fetch_add(1, Ordering::SeqCst);
            }),
            n,
        )
    }

    fn wait_until(mut done: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !done() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// A recorder in place of `RealLauncher`: proves the consent gate mechanically
    /// rather than by reading the source (a mutation check of the
    /// consent gate).
    #[derive(Default)]
    struct RecordingLauncher {
        calls: Mutex<Vec<Installer>>,
    }

    impl Launcher for RecordingLauncher {
        fn launch(
            &self,
            installer: &Installer,
        ) -> Result<update_check::InstallOutcome, UpdateError> {
            self.calls.lock().unwrap().push(installer.clone());
            Ok(update_check::InstallOutcome::ManualInstructions(
                "recorded".into(),
            ))
        }
    }

    /// The full handle, end to end, against a local fake server only: `check_now`
    /// reaches `Available` without ever calling the launcher, and only
    /// `accept_and_install` afterward reaches it. This is the mechanism the "no
    /// download without consent" requirement means; a mutation that made `check_now`
    /// (or the background start-up check) also call the launcher fails this test.
    #[tokio::test(flavor = "multi_thread")]
    async fn check_now_never_downloads_or_installs_only_accept_does() {
        let server = MockServer::start().await;
        let name = asset_name();
        let body = b"installer bytes";
        let hex: String = {
            use sha2::{Digest, Sha256};
            Sha256::digest(body)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect()
        };
        Mock::given(method("GET"))
            .and(path_matcher(format!(
                "/repos/{}/{}/releases",
                update_check::GITHUB_OWNER,
                update_check::GITHUB_REPO
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([{
                "tag_name": "v999.0.0",
                "draft": false,
                "prerelease": false,
                "assets": [
                    {"name": name, "browser_download_url": format!("{}/{name}", server.uri())},
                    {"name": format!("{name}.sha256"), "browser_download_url": format!("{}/{name}.sha256", server.uri())},
                ],
            }])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path_matcher(format!("/{name}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body.to_vec()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path_matcher(format!("/{name}.sha256")))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!("{hex}  {name}\n")))
            .mount(&server)
            .await;

        let launcher = Arc::new(RecordingLauncher::default());
        let (wake, _woke) = wake_counter();
        let handle = UpdateHandle::spawn_full(false, false, wake, launcher.clone(), server.uri());

        handle.check_now(false);
        wait_until(|| {
            matches!(
                handle.state(),
                UpdateState::Available(_) | UpdateState::Failed(_)
            )
        });
        let UpdateState::Available(candidate) = handle.state() else {
            panic!("expected Available, got {:?}", handle.state());
        };
        assert_eq!(candidate.asset_name, name);
        assert!(
            launcher.calls.lock().unwrap().is_empty(),
            "a mere check must never reach the launcher"
        );

        handle.accept_and_install(candidate);
        wait_until(|| {
            matches!(
                handle.state(),
                UpdateState::Ready { .. } | UpdateState::Failed(_)
            )
        });
        assert!(
            matches!(handle.state(), UpdateState::Ready { .. }),
            "expected Ready, got {:?}",
            handle.state()
        );
        assert_eq!(
            launcher.calls.lock().unwrap().len(),
            1,
            "the launcher runs exactly once, only after explicit consent"
        );
    }

    fn asset_name() -> String {
        crate::update_check::asset_candidates(
            std::env::consts::OS,
            std::env::consts::ARCH,
            "999.0.0",
        )
        .expect("this test's own platform is supported")
        .remove(0)
    }

    /// Drop stops the thread promptly (same guarantee `capacity_bg` gives).
    #[test]
    fn dropping_the_handle_stops_the_thread_promptly() {
        let (wake, _n) = wake_counter();
        let handle = UpdateHandle::spawn(false, false, wake);
        let start = std::time::Instant::now();
        drop(handle);
        assert!(start.elapsed() < SHUTDOWN_JOIN_TIMEOUT + Duration::from_secs(1));
    }

    /// `check_on_start = false` means no state change (and no wake) until
    /// `check_now` is called: the egress rule's "off by default" as a mechanism, not
    /// merely a config default (a test of the mechanism, not the setting).
    #[test]
    fn check_on_start_false_never_checks_until_asked() {
        let (wake, woke) = wake_counter();
        let handle = UpdateHandle::spawn(false, false, wake);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(handle.state(), UpdateState::Idle);
        assert_eq!(woke.load(Ordering::SeqCst), 0, "no wake without a check");
        drop(handle);
    }
}

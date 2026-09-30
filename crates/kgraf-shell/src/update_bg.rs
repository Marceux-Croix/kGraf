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
    wake: Wake,
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
        let wake_handle = wake.clone();
        let join = std::thread::Builder::new()
            .name("kgraf-update".to_owned())
            .spawn(move || {
                // However this thread ends, a check or download in flight is
                // over; see `ThreadExit`.
                let _exit = ThreadExit {
                    state: state_t.clone(),
                    wake: wake.clone(),
                };
                let runtime = match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(1)
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(e) => {
                        // Say so in the state, or a check that never runs
                        // reads as "idle" for good.
                        log::error!("update thread: the async runtime could not start: {e}");
                        set_state(
                            &state_t,
                            &wake,
                            UpdateState::Failed(format!(
                                "the update thread could not start its runtime: {e}"
                            )),
                        );
                        return;
                    }
                };
                // A client that cannot be built is a failure of the check or
                // download that needs it, said in the state; a default client
                // has none of these timeouts and is not a substitute.
                let client = reqwest::Client::builder()
                    .timeout(Duration::from_secs(30))
                    .build()
                    .map_err(|e| UpdateError::Client(e.to_string()));
                let download_client = update_check::download_client(
                    update_check::DOWNLOAD_CONNECT_TIMEOUT,
                    update_check::DOWNLOAD_IDLE_TIMEOUT,
                );
                if check_on_start {
                    match &client {
                        Ok(client) => runtime.block_on(run_check(
                            &state_t,
                            client,
                            &api_base,
                            allow_prerelease,
                            &wake,
                        )),
                        Err(e) => set_state(&state_t, &wake, UpdateState::Failed(e.to_string())),
                    }
                }
                loop {
                    if stop_t.load(Ordering::Relaxed) {
                        return;
                    }
                    match rx.recv_timeout(STOP_POLL_INTERVAL) {
                        Ok(Command::Check { allow_prerelease }) => match &client {
                            Ok(client) => runtime.block_on(run_check(
                                &state_t,
                                client,
                                &api_base,
                                allow_prerelease,
                                &wake,
                            )),
                            Err(e) => {
                                set_state_unless_downloading(
                                    &state_t,
                                    &wake,
                                    UpdateState::Failed(e.to_string()),
                                );
                            }
                        },
                        Ok(Command::Install(candidate)) => match &download_client {
                            Ok(download_client) => runtime.block_on(run_install(
                                &state_t,
                                download_client,
                                &candidate,
                                launcher.as_ref(),
                                &wake,
                            )),
                            Err(e) => {
                                set_state(&state_t, &wake, UpdateState::Failed(e.to_string()))
                            }
                        },
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
            })
            .ok();
        Self {
            state,
            tx,
            wake: wake_handle,
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
        if self.tx.send(Command::Check { allow_prerelease }).is_err() {
            // The thread has ended: nothing will answer this check.
            set_state(
                &self.state,
                &self.wake,
                UpdateState::Failed("the update thread is not running".to_owned()),
            );
        }
    }

    /// The consent gate. Call this only from the notice's own "Yes, download and
    /// install" action (see this module's own doc comment); nothing else in this
    /// crate may call it.
    ///
    /// Idempotent: the state moves to [`UpdateState::Downloading`] here, before the
    /// command is queued, so a second call while a download is in flight (a double
    /// click) returns without queueing another download.
    pub fn accept_and_install(&self, candidate: UpdateCandidate) {
        {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            // A thread that ended (a panic in a download, for one) cannot
            // finish a `Downloading` state it left behind: fail it, so the
            // user is told and a retry is not refused for good.
            if self.join.as_ref().is_none_or(|j| j.is_finished()) {
                *state = UpdateState::Failed("the update thread is not running".to_owned());
                drop(state);
                (self.wake)();
                return;
            }
            if *state == UpdateState::Downloading {
                return;
            }
            *state = UpdateState::Downloading;
        }
        (self.wake)();
        if self.tx.send(Command::Install(candidate)).is_err() {
            set_state(
                &self.state,
                &self.wake,
                UpdateState::Failed("the update thread is not running".to_owned()),
            );
        }
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

/// [`set_state`] unless a download is in flight: only the download's own end
/// moves the state out of `Downloading`. A check that finishes meanwhile must
/// not overwrite it, or `accept_and_install` would take a second download.
/// Whether the state was set.
fn set_state_unless_downloading(
    state: &Mutex<UpdateState>,
    wake: &Wake,
    next: UpdateState,
) -> bool {
    {
        let mut current = state.lock().unwrap_or_else(|p| p.into_inner());
        if *current == UpdateState::Downloading {
            return false;
        }
        *current = next;
    }
    wake();
    true
}

/// Ends a check or download that was in flight when the update thread exits
/// (a panic included). Without it the state stays `Checking` or `Downloading`
/// for good, and `accept_and_install` returns early on `Downloading`.
struct ThreadExit {
    state: Arc<Mutex<UpdateState>>,
    wake: Wake,
}

impl Drop for ThreadExit {
    fn drop(&mut self) {
        let in_flight = {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            let in_flight = matches!(*state, UpdateState::Checking | UpdateState::Downloading);
            if in_flight {
                *state = UpdateState::Failed("the update thread stopped".to_owned());
            }
            in_flight
        };
        if in_flight {
            (self.wake)();
        }
    }
}

async fn run_check(
    state: &Mutex<UpdateState>,
    client: &reqwest::Client,
    api_base: &str,
    allow_prerelease: bool,
    wake: &Wake,
) {
    if !set_state_unless_downloading(state, wake, UpdateState::Checking) {
        return;
    }
    let outcome = check_once(client, api_base, allow_prerelease).await;
    let next = match outcome {
        Ok(Some(candidate)) => UpdateState::Available(candidate),
        Ok(None) => UpdateState::UpToDate,
        Err(e) => UpdateState::Failed(e.to_string()),
    };
    set_state_unless_downloading(state, wake, next);
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

    /// A double click queues one download, not two: the second call sees
    /// `Downloading` and returns. Mechanism: count the asset requests the server
    /// saw and the launcher calls.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_second_accept_while_downloading_queues_nothing() {
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
            .and(path_matcher(format!("/{name}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(body.to_vec())
                    .set_delay(Duration::from_millis(500)),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path_matcher(format!("/{name}.sha256")))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!("{hex}  {name}\n")))
            .mount(&server)
            .await;
        let candidate = UpdateCandidate {
            version: semver::Version::new(999, 0, 0),
            tag_name: "v999.0.0-double-click".to_owned(),
            asset_name: name.clone(),
            download_url: format!("{}/{name}", server.uri()),
            checksum_name: format!("{name}.sha256"),
            checksum_url: format!("{}/{name}.sha256", server.uri()),
        };
        let launcher = Arc::new(RecordingLauncher::default());
        let (wake, _woke) = wake_counter();
        let handle = UpdateHandle::spawn_full(false, false, wake, launcher.clone(), server.uri());

        handle.accept_and_install(candidate.clone());
        assert_eq!(
            handle.state(),
            UpdateState::Downloading,
            "the state changes before the command is queued"
        );
        handle.accept_and_install(candidate);
        wait_until(|| matches!(handle.state(), UpdateState::Ready { .. }));
        // Time for a wrongly queued second download to run.
        tokio::time::sleep(Duration::from_millis(1200)).await;
        let asset_requests = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.url.path() == format!("/{name}"))
            .count();
        assert_eq!(asset_requests, 1, "the installer was downloaded twice");
        assert_eq!(launcher.calls.lock().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(
            std::env::temp_dir().join("kgraf-update/v999.0.0-double-click"),
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

    /// A check that finishes while a download is in flight leaves `Downloading`
    /// alone, so a second accept cannot start another download. Mechanism: the
    /// state is `Downloading` before and after, and the check never even asks
    /// the server.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_check_never_overwrites_a_download_in_flight() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&server)
            .await;
        let state = Mutex::new(UpdateState::Downloading);
        let (wake, woke) = wake_counter();
        run_check(&state, &reqwest::Client::new(), &server.uri(), false, &wake).await;
        assert_eq!(*state.lock().unwrap(), UpdateState::Downloading);
        assert_eq!(woke.load(Ordering::SeqCst), 0);
        assert!(server.received_requests().await.unwrap().is_empty());

        // The same check on an idle state does run and set its result.
        *state.lock().unwrap() = UpdateState::Idle;
        run_check(&state, &reqwest::Client::new(), &server.uri(), false, &wake).await;
        assert_eq!(*state.lock().unwrap(), UpdateState::UpToDate);
    }

    /// An update thread that ends with a check or download in flight leaves a
    /// `Failed` state, not `Downloading` for good; a panic counts.
    #[test]
    fn a_thread_that_ends_mid_download_leaves_a_failed_state() {
        let state = Arc::new(Mutex::new(UpdateState::Downloading));
        let (wake, woke) = wake_counter();
        let (s, w) = (state.clone(), wake.clone());
        let panicked = std::thread::spawn(move || {
            let _exit = ThreadExit { state: s, wake: w };
            panic!("a download panicked");
        })
        .join();
        assert!(panicked.is_err());
        assert!(matches!(*state.lock().unwrap(), UpdateState::Failed(_)));
        assert_eq!(woke.load(Ordering::SeqCst), 1);

        // A state that is not in flight is left as it is.
        *state.lock().unwrap() = UpdateState::UpToDate;
        drop(ThreadExit {
            state: state.clone(),
            wake,
        });
        assert_eq!(*state.lock().unwrap(), UpdateState::UpToDate);
    }

    /// After the thread is gone an accept fails loud instead of returning on a
    /// `Downloading` state nothing will ever finish.
    #[test]
    fn an_accept_after_the_thread_died_is_told_so_not_swallowed() {
        let (wake, _n) = wake_counter();
        let handle = UpdateHandle::spawn(false, false, wake);
        handle.stop.store(true, Ordering::Relaxed);
        wait_until(|| handle.join.as_ref().is_some_and(|j| j.is_finished()));
        *handle.state.lock().unwrap() = UpdateState::Downloading;
        handle.accept_and_install(UpdateCandidate {
            version: semver::Version::new(999, 0, 0),
            tag_name: "v999.0.0".to_owned(),
            asset_name: "a".to_owned(),
            download_url: "http://127.0.0.1:9/a".to_owned(),
            checksum_name: "a.sha256".to_owned(),
            checksum_url: "http://127.0.0.1:9/a.sha256".to_owned(),
        });
        assert!(
            matches!(handle.state(), UpdateState::Failed(_)),
            "{:?}",
            handle.state()
        );
    }

    /// A check asked of a thread that is gone is told so, not dropped.
    #[test]
    fn a_check_after_the_thread_died_is_told_so_not_dropped() {
        let (wake, woke) = wake_counter();
        let handle = UpdateHandle::spawn(false, false, wake);
        handle.stop.store(true, Ordering::Relaxed);
        wait_until(|| handle.join.as_ref().is_some_and(|j| j.is_finished()));
        assert_eq!(handle.state(), UpdateState::Idle);
        handle.check_now(false);
        assert!(
            matches!(handle.state(), UpdateState::Failed(_)),
            "{:?}",
            handle.state()
        );
        assert!(woke.load(Ordering::SeqCst) >= 1, "the UI is woken");
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

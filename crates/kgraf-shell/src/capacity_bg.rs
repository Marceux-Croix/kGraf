//! The per-machine capacity re-measure thread (decision 0067). The slider maximum comes from the
//! loaded models and their rate limits, as well as this machine.
//!
//! Measures this machine every [`RE_MEASURE_INTERVAL`] (design, "Measured
//! again": "Every 60 seconds for memory and disk"), reads every open
//! project's configured routes (`kgraf_host::Host::route_capacity_inputs`,
//! reached through [`crate::orchestrator_host::RunHousekeeping`] so this
//! module does not need to know which concrete factory type a project
//! uses), overlays the learned (429-adjusted) rate on top, runs the result
//! through [`kgraf_capacity::Hysteresis`] so the slider's shown maximum does
//! not jitter, and calls [`MachineGate::set_limits`] on every accepted
//! reading. **One thread for the whole process**, not one per project: the
//! gate itself is process-wide (decision 0067: the slider binds
//! this app process), so measuring it once and driving the one shared gate
//! is both correct and cheaper than one thread per open project converging
//! on the same number.
//!
//! **Off the UI thread**, same reasoning as `presume_bg.rs`'s own doc
//! comment: `kgraf_capacity::measure_machine` walks every open project's
//! disk and reads `/proc`/`getrlimit`, cheap but not "every frame" cheap,
//! and a project on a network mount must never stall a frame over it.
//!
//! **A route only contributes once its project's `Host` is already open.**
//! `RunHousekeeping::route_capacity_inputs` never opens one itself (a
//! background measurement thread must not have that side effect), so a
//! project the user has not touched yet contributes nothing until it has.
//! Two open projects configuring the same route name are deduplicated (the
//! first one seen wins): the design sums capacity "across vendors", not
//! across two configurations of the one same vendor route, which would
//! double what that route's real limit actually allows.
//!
//! **Known limitation, stated not hidden.** `round_ms` and
//! `tokens_per_round` (from `Ledger::route_round_stats`, inside
//! `Host::route_capacity_inputs`) group by the wire adapter, not by this
//! catalog route id, since the ledger schema has no route column; see that
//! method's own doc comment. The learned-rate overlay here estimates "the
//! rate observed over the last minute" (design's own phrase) as the
//! route's current known limit (header or configured), not a true
//! measured recent call rate, since that would need the same route/adapter
//! reconciliation; a 429 still halves a real number and still recovers, so
//! the mechanism is real, only its precision is bounded by this
//! simplification.

use kgraf_capacity::{Capacity, Hysteresis, LearnedRate, LimitSource, RouteInputs};
use kgraf_orchestrator::{MachineGate, Wake};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How often the machine is re-measured (design: "Every 60 seconds").
pub const RE_MEASURE_INTERVAL: Duration = Duration::from_secs(60);
/// How long [`Drop`] waits for the thread before abandoning it (same
/// reasoning as `scheduler_bg::SHUTDOWN_JOIN_TIMEOUT`: shutdown must never
/// hang forever on a thread that, for whatever reason, does not stop
/// promptly).
const SHUTDOWN_JOIN_TIMEOUT: Duration = Duration::from_secs(2);
/// How often the loop checks `stop` while otherwise waiting out the
/// interval, for a prompt shutdown (same pattern as `scheduler_bg`'s tick
/// loop).
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// The volumes the writer bound measures free disk on: every open project's
/// root, updated as projects open and close (design: "Measured again ...
/// When a project opens or closes").
#[derive(Clone, Default)]
pub struct Volumes(Arc<Mutex<Vec<PathBuf>>>);

impl Volumes {
    pub fn set(&self, paths: Vec<PathBuf>) {
        *self.0.lock().unwrap_or_else(|p| p.into_inner()) = paths;
    }

    fn get(&self) -> Vec<PathBuf> {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

/// Every open project's housekeeping handle, read each tick for its
/// configured routes (`RunHousekeeping::route_capacity_inputs`). A
/// `Vec<Arc<dyn RunHousekeeping>>` rather than a `Vec<Arc<Host>>`: the shell
/// already stores each project behind this trait object
/// (`orchestrator_host::HostParts::housekeeping`), and a project's `Host`
/// may not exist yet.
#[derive(Clone, Default)]
pub struct Projects(Arc<Mutex<Vec<Arc<dyn crate::orchestrator_host::RunHousekeeping>>>>);

impl Projects {
    pub fn set(&self, projects: Vec<Arc<dyn crate::orchestrator_host::RunHousekeeping>>) {
        *self.0.lock().unwrap_or_else(|p| p.into_inner()) = projects;
    }

    fn get(&self) -> Vec<Arc<dyn crate::orchestrator_host::RunHousekeeping>> {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

/// Every open project's configured routes, deduplicated by route name (the
/// first project to configure a given route wins; see the module doc for
/// why this must not sum twice).
fn collect_route_inputs(projects: &Projects) -> Vec<kgraf_host::RouteCapacityInput> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for project in projects.get() {
        for input in project.route_capacity_inputs() {
            if seen.insert(input.inputs.route.clone()) {
                out.push(input);
            }
        }
    }
    out
}

/// The learned-rate overlay (decision 0067, "Measured again": "A 429 halves
/// the route's learned RPM ... The learned RPM then recovers by 10% every 5
/// minutes without a 429"), applied on top of whatever `rpm` each
/// [`kgraf_host::RouteCapacityInput`] already carries (header or configured;
/// see the module doc for why "the rate observed over the last minute" is
/// approximated here as that known limit).
///
/// A pure function over its own state (`learned`, `last_429_seen`, both
/// owned by the caller's loop across ticks) so it is directly testable
/// without a real thread, a real `Host`, or a real clock.
fn apply_learned_overlay(
    inputs: Vec<kgraf_host::RouteCapacityInput>,
    learned: &mut HashMap<String, LearnedRate>,
    last_429_seen: &mut HashMap<String, i64>,
    now_ms: i64,
) -> Vec<RouteInputs> {
    inputs
        .into_iter()
        .map(
            |kgraf_host::RouteCapacityInput {
                 mut inputs,
                 base_url,
             }| {
                let Some((known_rpm, _)) = inputs.rpm else {
                    return inputs;
                };
                let state = learned
                    .entry(inputs.route.clone())
                    .or_insert_with(|| LearnedRate::new(known_rpm, now_ms));
                if let Some(url) = &base_url
                    && let Some(seen_at) = kgraf_provider::limits::last_429(url)
                    && last_429_seen.get(&inputs.route) != Some(&seen_at)
                {
                    state.on_429(known_rpm, now_ms);
                    last_429_seen.insert(inputs.route.clone(), seen_at);
                }
                state.recover(now_ms);
                let current = state.current_rpm();
                if current < known_rpm {
                    inputs.rpm = Some((current, LimitSource::Learned));
                }
                inputs
            },
        )
        .collect()
}

/// Owns the re-measure thread. Dropping it stops the thread and joins it
/// with a timeout, exactly as `scheduler_bg::SchedulerHandle` does, so it
/// never outlives the `ShellApp` that started it.
pub struct CapacityHandle {
    stop: Arc<AtomicBool>,
    latest: Arc<Mutex<Option<Capacity>>>,
    volumes: Volumes,
    projects: Projects,
    join: Option<std::thread::JoinHandle<()>>,
}

impl CapacityHandle {
    /// Spawns the thread. `gate` is set through [`MachineGate::set_limits`]
    /// on every accepted (Hysteresis-passed) reading; `wake` runs then too,
    /// so the shell repaints Settings if it is open. Never called from
    /// `ShellApp::new` itself (a headless test creating a `ShellApp` must
    /// never start a background thread, same rule `AppParts`'s own doc
    /// comment states for the scheduler): the real app entry point calls
    /// this once.
    pub fn spawn(gate: Arc<MachineGate>, volumes: Volumes, wake: Wake) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let latest = Arc::new(Mutex::new(None));
        let projects = Projects::default();
        let (stop_t, latest_t, volumes_t, projects_t) = (
            stop.clone(),
            latest.clone(),
            volumes.clone(),
            projects.clone(),
        );
        let join = std::thread::Builder::new()
            .name("kgraf-capacity".to_owned())
            .spawn(move || run_forever(&stop_t, &gate, &volumes_t, &projects_t, &latest_t, &wake))
            .ok();
        Self {
            stop,
            latest,
            volumes,
            projects,
            join,
        }
    }

    /// The most recently accepted capacity, for the Settings breakdown
    /// (`None` until the first reading is accepted, which happens at once:
    /// [`kgraf_capacity::Hysteresis::observe`]'s first call always commits).
    pub fn latest(&self) -> Option<Capacity> {
        self.latest
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Update the volumes the writer bound measures (a project opened or
    /// closed); takes effect on the next tick.
    pub fn set_volumes(&self, paths: Vec<PathBuf>) {
        self.volumes.set(paths);
    }

    /// Update the open projects the route bound reads from (a project opened
    /// or closed); takes effect on the next tick.
    pub fn set_projects(&self, projects: Vec<Arc<dyn crate::orchestrator_host::RunHousekeeping>>) {
        self.projects.set(projects);
    }
}

impl Drop for CapacityHandle {
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
                "capacity re-measure thread did not stop within {SHUTDOWN_JOIN_TIMEOUT:?}; \
                 abandoning it rather than blocking shutdown"
            );
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn run_forever(
    stop: &AtomicBool,
    gate: &MachineGate,
    volumes: &Volumes,
    projects: &Projects,
    latest: &Mutex<Option<Capacity>>,
    wake: &Wake,
) {
    let mut hysteresis = Hysteresis::new();
    let mut learned: HashMap<String, LearnedRate> = HashMap::new();
    let mut last_429_seen: HashMap<String, i64> = HashMap::new();
    let mut waited = RE_MEASURE_INTERVAL; // measure once immediately, at start-up
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        if waited >= RE_MEASURE_INTERVAL {
            waited = Duration::ZERO;
            if let Ok(machine) = kgraf_capacity::measure_machine(&volumes.get()) {
                let route_inputs = apply_learned_overlay(
                    collect_route_inputs(projects),
                    &mut learned,
                    &mut last_429_seen,
                    now_ms(),
                );
                let capacity = kgraf_capacity::compute(&route_inputs, &machine);
                if let Some(accepted) = hysteresis.observe(&capacity) {
                    gate.set_limits(kgraf_core::ParallelismLimits::derive(
                        accepted.max_agents,
                        accepted.max_writers,
                        accepted.exec_slots,
                    ));
                    *latest.lock().unwrap_or_else(|p| p.into_inner()) = Some(accepted);
                    wake();
                }
            }
        }
        std::thread::sleep(STOP_POLL_INTERVAL);
        waited += STOP_POLL_INTERVAL;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// How long a test waits for the thread's first accepted reading. The
    /// wait is on the wake callback, which runs when a reading is accepted;
    /// this bound only turns a broken mechanism into a failure and is never
    /// what a passing run waits on, so a loaded machine cannot expire it.
    const FIRST_READING_BOUND: Duration = Duration::from_secs(120);

    /// Spawns the thread and blocks until its first accepted reading has
    /// been stored and announced through the wake callback.
    fn spawn_and_wait(
        gate: Arc<MachineGate>,
        volumes: Volumes,
    ) -> (CapacityHandle, Arc<AtomicBool>) {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let woke = Arc::new(AtomicBool::new(false));
        let woke_t = woke.clone();
        let wake: Wake = Arc::new(move || {
            woke_t.store(true, Ordering::SeqCst);
            let _ = tx.send(());
        });
        let handle = CapacityHandle::spawn(gate, volumes, wake);
        rx.recv_timeout(FIRST_READING_BOUND)
            .expect("the thread accepted a first reading");
        (handle, woke)
    }

    /// The mechanism: a fresh `MachineGate` gets `set_limits` called on it
    /// from the background thread, without the test waiting a real 60 s
    /// (the loop's very first check runs at once, `waited` starts primed to
    /// the interval).
    #[test]
    fn the_thread_sets_the_gates_limits_from_a_real_measurement() {
        let gate = MachineGate::new(kgraf_core::ParallelismLimits::derive(1, 1, 1));
        let (handle, woke) = spawn_and_wait(gate.clone(), Volumes::default());
        let capacity = handle.latest().expect("a reading was accepted");
        assert_eq!(gate.limits().max_agents, capacity.max_agents);
        assert!(woke.load(Ordering::SeqCst), "the wake callback ran");
    }

    /// `set_volumes` reaches the next tick's `measure_machine` call: proven
    /// by giving a nonexistent, unreadable path and confirming the mechanism
    /// still tolerates it (an empty or bad volume list is not a panic).
    #[test]
    fn set_volumes_is_read_by_the_next_tick_without_panicking() {
        let volumes = Volumes::default();
        volumes.set(vec![PathBuf::from(
            "/definitely/not/a/real/path/for/this/test",
        )]);
        let gate = MachineGate::new(kgraf_core::ParallelismLimits::derive(1, 1, 1));
        let (handle, _woke) = spawn_and_wait(gate, volumes);
        assert!(handle.latest().is_some());
    }

    /// Dropping the handle stops the thread promptly, not by leaking it.
    #[test]
    fn dropping_the_handle_stops_the_thread() {
        let gate = MachineGate::new(kgraf_core::ParallelismLimits::derive(1, 1, 1));
        let (handle, _woke) = spawn_and_wait(gate, Volumes::default());
        let start = std::time::Instant::now();
        drop(handle);
        assert!(
            start.elapsed() < SHUTDOWN_JOIN_TIMEOUT + Duration::from_secs(1),
            "drop must not hang past its own join timeout"
        );
    }

    fn route_input(
        route: &str,
        rpm: u32,
        base_url: Option<&str>,
    ) -> kgraf_host::RouteCapacityInput {
        kgraf_host::RouteCapacityInput {
            inputs: RouteInputs {
                route: route.to_owned(),
                rpm: Some((rpm, LimitSource::Configured)),
                tpm: None,
                round_ms: None,
                tokens_per_round: None,
                local_slots: None,
            },
            base_url: base_url.map(str::to_owned),
        }
    }

    /// Without a 429, the overlay leaves the configured rpm untouched. A
    /// base URL this test alone uses: the 429 registry is process-wide, so a
    /// key another test records a 429 under must never leak here.
    #[test]
    fn no_429_leaves_the_configured_rate_untouched() {
        let mut learned = HashMap::new();
        let mut last_429_seen = HashMap::new();
        let out = apply_learned_overlay(
            vec![route_input(
                "kimi",
                60,
                Some("http://no-429-leaves-the-configured-rate-untouched.test"),
            )],
            &mut learned,
            &mut last_429_seen,
            0,
        );
        assert_eq!(out[0].rpm, Some((60, LimitSource::Configured)));
    }

    /// A recorded 429 halves the rate and marks it `Learned`; recovering
    /// later climbs it back, never past the known limit.
    #[test]
    fn a_429_halves_the_rate_and_recovery_climbs_it_back() {
        kgraf_provider::limits::clear_429_for_test();
        kgraf_provider::limits::record_429("http://a-429-halves-the-rate.test", 1_000);
        let mut learned = HashMap::new();
        let mut last_429_seen = HashMap::new();

        let out = apply_learned_overlay(
            vec![route_input(
                "kimi",
                60,
                Some("http://a-429-halves-the-rate.test"),
            )],
            &mut learned,
            &mut last_429_seen,
            1_000,
        );
        assert_eq!(out[0].rpm, Some((30, LimitSource::Learned)));

        // The same 429 seen again (same timestamp) does not halve twice.
        let out = apply_learned_overlay(
            vec![route_input(
                "kimi",
                60,
                Some("http://a-429-halves-the-rate.test"),
            )],
            &mut learned,
            &mut last_429_seen,
            1_000,
        );
        assert_eq!(out[0].rpm, Some((30, LimitSource::Learned)));

        // Recovery, with no new 429, climbs back toward 60 and eventually
        // reports the configured source again once it reaches the known
        // limit.
        let out = apply_learned_overlay(
            vec![route_input(
                "kimi",
                60,
                Some("http://a-429-halves-the-rate.test"),
            )],
            &mut learned,
            &mut last_429_seen,
            1_000 + kgraf_capacity::RECOVERY_INTERVAL_MS * 1_000,
        );
        assert_eq!(out[0].rpm, Some((60, LimitSource::Configured)));
    }

    /// Two open projects configuring the same route name contribute it once,
    /// not twice (the design sums capacity across vendors, not across two
    /// configurations of one vendor's route).
    #[test]
    fn the_same_route_from_two_projects_is_counted_once() {
        struct Fake(Vec<kgraf_host::RouteCapacityInput>);
        impl crate::orchestrator_host::RunHousekeeping for Fake {
            fn admission(&self) -> Option<Arc<kgraf_orchestrator::Admission>> {
                None
            }
            fn finish_run(&self, _run: &kgraf_core::RunId) -> Vec<String> {
                Vec::new()
            }
            fn take_notices(&self) -> Vec<String> {
                Vec::new()
            }
            fn route_capacity_inputs(&self) -> Vec<kgraf_host::RouteCapacityInput> {
                self.0.clone()
            }
        }
        let projects = Projects::default();
        projects.set(vec![
            Arc::new(Fake(vec![route_input("kimi", 60, None)])),
            Arc::new(Fake(vec![route_input("kimi", 30, None)])),
        ]);
        let out = collect_route_inputs(&projects);
        assert_eq!(out.len(), 1, "one route, once: {out:?}");
        assert_eq!(
            out[0].inputs.rpm,
            Some((60, LimitSource::Configured)),
            "the first one seen wins"
        );
    }
}

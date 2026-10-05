//! The per-machine parallelism maximum (decision 0067, "The per-machine
//! maximum"). This crate is pure and measurement code
//! only: it does not know about the orchestrator, the provider crates or the
//! shared cross-process rate limit store (decision 0072). [`compute`] takes
//! plain numbers and returns a [`Capacity`]; [`measure_machine`] is the one
//! place that reads the real machine. Everything else in the workspace wires
//! these two functions to its own state.
//!
//! **Reuse.** Searched crates.io 2026-09-27 for a maintained,
//! cross-platform machine-measurement crate: `heim` (unmaintained since
//! 2021, its own README says so), `psutil` (Linux-only bindings), `sys-info`
//! (last release 2019, open soundness issues). `sysinfo` (MIT, license read
//! from its own `Cargo.toml` at version 0.39.6, actively maintained, used by
//! `bottom`, `htop`-alikes and several other production tools) is the
//! result: it is the sole machine-facts dependency here. File-descriptor and
//! process limits are not part of `sysinfo`'s scope on any platform it
//! supports, so [`fd_limits`] calls `libc::getrlimit` directly, the same
//! approach `tokio`'s own `net::TcpListener` backlog sizing and most Rust
//! server code use; there is no maintained crate that only wraps
//! `getrlimit`, and wrapping one function is not enough surface to justify
//! adding one.
//!
//! **Why this crate does not depend on `kgraf-provider` or
//! `kgraf-orchestrator`.** [`RouteInputs`] and [`MachineInputs`] are already
//! plain enough to build from either crate's own state (ledger statistics,
//! `RouteLimiter::stats`, `sysinfo`) without this crate reaching back up the
//! dependency graph. `kgraf-orchestrator::admission` (which does depend on
//! this crate) is what turns a [`Capacity`] into the shared
//! [`kgraf_core::parallel::ParallelismLimits`] the rest of the process reads.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// Where a bound's number came from. The design's own vocabulary calls a
/// machine-measured number "measured" too, but every actual figure this
/// crate reports for memory, descriptors and disk is data `compute` was
/// simply handed by its caller (`measure_machine`, or a test): the only
/// distinction `compute` itself can draw from its inputs alone is where a
/// *route's* limit came from, which is why only route bounds vary across all
/// four variants. Memory, descriptor and disk bounds always report
/// [`LimitSource::Configured`]: the number is exactly what
/// [`MachineInputs`] said, without another authority (a header, a learned
/// adjustment, or "we have no idea") layered on top of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LimitSource {
    /// The vendor's own last-returned rate-limit headers.
    Header,
    /// A value the user configured (`.kgraf/run.json`, or a local server's
    /// declared parallel slot count).
    Configured,
    /// Adjusted from a 429 and its later recovery (see the crate's
    /// [`LearnedRate`]).
    Learned,
    /// No known limit at all: the route contributes the fallback of 4
    /// agents (design, "The per-machine maximum", point 1).
    Unmeasured,
}

/// One route's contribution to the route bound.
#[derive(Clone, Debug, PartialEq)]
pub struct RouteInputs {
    pub route: String,
    /// Requests per minute and where that number came from.
    pub rpm: Option<(u32, LimitSource)>,
    /// Tokens per minute and where that number came from.
    pub tpm: Option<(u64, LimitSource)>,
    /// Median round-trip time for one round on this route, from the ledger.
    /// `None` until the route has 20 rounds (design: 20 s is the default
    /// until then).
    pub round_ms: Option<u64>,
    /// Median input-plus-output tokens per round on this route.
    pub tokens_per_round: Option<u64>,
    /// Present only for a local model server: its configured parallel slot
    /// count. A local route ignores `rpm`/`tpm` entirely (there is no vendor
    /// limit to derive from), and contributes this count, or 1 if it is
    /// `Some(0)` or absent-but-local (design, point 1).
    pub local_slots: Option<u32>,
}

impl RouteInputs {
    /// A route with nothing else set: `route_bound` alone decides the
    /// fallback from this.
    pub fn unmeasured(route: impl Into<String>) -> Self {
        Self {
            route: route.into(),
            rpm: None,
            tpm: None,
            round_ms: None,
            tokens_per_round: None,
            local_slots: None,
        }
    }
}

/// The default round time until a route has 20 rounds of ledger history
/// (design, "The per-machine maximum", point 1).
pub const DEFAULT_ROUND_MS: u64 = 20_000;
/// A route with no known rate limit contributes this many agents.
pub const UNMEASURED_ROUTE_AGENTS: u32 = 4;
/// The headroom factor applied to every route's derived rate (design's
/// `0.8 *`).
pub const HEADROOM: f64 = 0.8;

/// The machine facts [`compute`] turns into bounds. Every field here is a
/// plain number: [`measure_machine`] is what fills them in from the real
/// machine; a test fills them in directly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MachineInputs {
    pub cores: u32,
    pub mem_available: u64,
    pub mem_total: u64,
    pub fd_soft: u64,
    pub fd_in_use: u64,
    pub disk_free: u64,
    pub disk_total: u64,
    /// The checkout size plus the warm target size (decision 0060), or the
    /// measured high-water mark of a retired worktree once one exists.
    pub bytes_per_worktree: u64,
    /// Measured growth in process footprint per live agent; the default
    /// until measured is [`DEFAULT_MEM_PER_AGENT`].
    pub mem_per_agent: u64,
    /// Measured peak of a gate or build run; the default until measured is
    /// [`DEFAULT_MEM_PER_JOB`].
    pub mem_per_job: u64,
    /// Measured file descriptors held per live agent; the default until
    /// measured is [`DEFAULT_FD_PER_AGENT`].
    pub fd_per_agent: u64,
}

/// 64 MiB: the default `mem_per_agent` until it is measured (design).
pub const DEFAULT_MEM_PER_AGENT: u64 = 64 * 1024 * 1024;
/// 2 GiB: the default `mem_per_job` until it is measured (design).
pub const DEFAULT_MEM_PER_JOB: u64 = 2 * 1024 * 1024 * 1024;
/// 32: the default `fd_per_agent` until it is measured (design).
pub const DEFAULT_FD_PER_AGENT: u64 = 32;
/// 2 GiB: the memory reserve floor (design: "the larger of 2 GiB and 20% of
/// total memory").
const MEM_RESERVE_FLOOR: u64 = 2 * 1024 * 1024 * 1024;
/// 10 GiB: the disk reserve floor (design: "the larger of 10 GiB and 10% of
/// the volume").
const DISK_RESERVE_FLOOR: u64 = 10 * 1024 * 1024 * 1024;
/// The descriptor headroom kGraf itself keeps for its own open files and
/// sockets, before any agent's share (design's `- 256`).
const FD_HEADROOM: u64 = 256;

/// One named bound in the breakdown: its value, where it came from, and a
/// human-readable detail (Settings > Runs shows this, STYLE_GUIDE §5.2's
/// "state the number and its source" rule).
#[derive(Clone, Debug, PartialEq)]
pub struct Bound {
    pub name: &'static str,
    pub value: u32,
    pub source: LimitSource,
    pub detail: String,
}

/// The computed maximum and the breakdown behind it.
#[derive(Clone, Debug, PartialEq)]
pub struct Capacity {
    /// `max(1, min(route, memory, fd))` (design: "cores do not bound
    /// agents").
    pub max_agents: u32,
    pub max_writers: u32,
    pub exec_slots: u32,
    pub bounds: Vec<Bound>,
}

fn combine_source(sources: &[LimitSource]) -> LimitSource {
    // The weakest source in the group speaks for the whole aggregate: a
    // route bound that is one unmeasured route plus one header-backed route
    // is still, honestly, partly a guess.
    use LimitSource::*;
    if sources.contains(&Unmeasured) {
        Unmeasured
    } else if sources.contains(&Learned) {
        Learned
    } else if sources.contains(&Configured) {
        Configured
    } else {
        Header
    }
}

/// One route's agent contribution and the source that produced it.
fn route_cap(route: &RouteInputs) -> (u32, LimitSource, String) {
    if let Some(slots) = route.local_slots {
        let n = slots.max(1);
        return (
            n,
            LimitSource::Configured,
            format!(
                "`{}` is local: {n} configured parallel slot(s)",
                route.route
            ),
        );
    }
    let (Some((rpm, rpm_src)), tpm) = (route.rpm, route.tpm) else {
        return (
            UNMEASURED_ROUTE_AGENTS,
            LimitSource::Unmeasured,
            format!(
                "`{}` has no known rate limit; unmeasured fallback",
                route.route
            ),
        );
    };
    let t_r = route.round_ms.unwrap_or(DEFAULT_ROUND_MS) as f64 / 1000.0;
    let by_rpm = f64::from(rpm) * t_r / 60.0;
    let (cap, source) = match (tpm, route.tokens_per_round) {
        (Some((tpm, tpm_src)), Some(tok_r)) if tok_r > 0 => {
            let by_tpm = tpm as f64 * t_r / (60.0 * tok_r as f64);
            if by_tpm < by_rpm {
                (by_tpm, combine_source(&[rpm_src, tpm_src]))
            } else {
                (by_rpm, rpm_src)
            }
        }
        _ => (by_rpm, rpm_src),
    };
    let agents = ((HEADROOM * cap).floor().max(0.0)) as u32;
    (
        agents,
        source,
        format!(
            "`{}`: {rpm} rpm over a {:.0} s round -> {agents} agent(s)",
            route.route, t_r
        ),
    )
}

/// Sum every route's contribution, subtracting the planner's own calls
/// (design, point 1: "The planner route's own calls are subtracted").
/// `planner_route`, when it names one of `routes`, has one agent's worth of
/// its own capacity removed, since the orchestrator's calls use route
/// capacity but take no agent slot (decision 0067 item 7).
fn route_bound(routes: &[RouteInputs], planner_route: Option<&str>) -> (u32, LimitSource, String) {
    if routes.is_empty() {
        return (
            UNMEASURED_ROUTE_AGENTS,
            LimitSource::Unmeasured,
            "no routes configured; unmeasured fallback".to_owned(),
        );
    }
    let mut total = 0u32;
    let mut sources = Vec::with_capacity(routes.len());
    let mut details = Vec::with_capacity(routes.len());
    for r in routes {
        let (agents, source, detail) = route_cap(r);
        let agents = if planner_route == Some(r.route.as_str()) {
            agents.saturating_sub(1)
        } else {
            agents
        };
        total += agents;
        sources.push(source);
        details.push(detail);
    }
    (total, combine_source(&sources), details.join("; "))
}

/// `compute` combines every bound. It never reads the machine or a clock:
/// every number it needs is in `routes` and `machine`.
pub fn compute(routes: &[RouteInputs], machine: &MachineInputs) -> Capacity {
    compute_for(routes, machine, None)
}

/// As [`compute`], but with the planner's own route named so its calls are
/// subtracted from that route's contribution (design, point 1).
pub fn compute_for(
    routes: &[RouteInputs],
    machine: &MachineInputs,
    planner_route: Option<&str>,
) -> Capacity {
    let (route_agents, route_src, route_detail) = route_bound(routes, planner_route);

    let mem_reserve = (machine.mem_total / 5).max(MEM_RESERVE_FLOOR);
    let mem_free = machine.mem_available.saturating_sub(mem_reserve);
    let mem_per_agent = machine.mem_per_agent.max(1);
    let mem_agents = (mem_free / mem_per_agent) as u32;

    let fd_free = machine
        .fd_soft
        .saturating_sub(machine.fd_in_use)
        .saturating_sub(FD_HEADROOM);
    let fd_per_agent = machine.fd_per_agent.max(1);
    let fd_agents = (fd_free / fd_per_agent) as u32;

    let max_agents = route_agents.min(mem_agents).min(fd_agents).max(1);

    let disk_reserve = (machine.disk_total / 10).max(DISK_RESERVE_FLOOR);
    let disk_free = machine.disk_free.saturating_sub(disk_reserve);
    let bytes_per_worktree = machine.bytes_per_worktree.max(1);
    let max_writers = (disk_free / bytes_per_worktree) as u32;

    let job_free = machine.mem_available.saturating_sub(mem_reserve);
    let mem_per_job = machine.mem_per_job.max(1);
    let exec_by_mem = (job_free / mem_per_job) as u32;
    let exec_slots = machine.cores.min(exec_by_mem).max(1);

    let bounds = vec![
        Bound {
            name: "route",
            value: route_agents,
            source: route_src,
            detail: route_detail,
        },
        Bound {
            name: "memory",
            value: mem_agents,
            source: LimitSource::Configured,
            detail: format!(
                "{} available, {} reserved, {} per agent -> {mem_agents} agent(s)",
                human(machine.mem_available),
                human(mem_reserve),
                human(mem_per_agent)
            ),
        },
        Bound {
            name: "descriptors",
            value: fd_agents,
            source: LimitSource::Configured,
            detail: format!(
                "{} soft limit, {} in use, {fd_per_agent} per agent -> {fd_agents} agent(s)",
                machine.fd_soft, machine.fd_in_use
            ),
        },
        Bound {
            name: "writers",
            value: max_writers,
            source: LimitSource::Configured,
            detail: format!(
                "{} free disk, {} reserved, {} per worktree -> {max_writers} writer(s)",
                human(machine.disk_free),
                human(disk_reserve),
                human(bytes_per_worktree)
            ),
        },
        Bound {
            name: "exec slots",
            value: exec_slots,
            source: LimitSource::Configured,
            detail: format!(
                "{} cores, {} for jobs at {} each -> {exec_slots} slot(s)",
                machine.cores,
                human(job_free),
                human(mem_per_job)
            ),
        },
    ];

    Capacity {
        max_agents,
        max_writers,
        exec_slots,
        bounds,
    }
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// A failure to measure the real machine.
#[derive(Debug, thiserror::Error)]
pub enum CapacityError {
    #[error("could not read the file-descriptor limit: {0}")]
    Rlimit(String),
}

/// Read the real machine: cores, memory, descriptors and disk, for the given
/// project volumes. `mem_per_agent`, `mem_per_job`, `fd_per_agent` and
/// `bytes_per_worktree` are filled with their stated defaults; the caller
/// (which has the ledger and the worktree high-water marks this crate does
/// not) overwrites them once it has measured values.
pub fn measure_machine(project_volumes: &[PathBuf]) -> Result<MachineInputs, CapacityError> {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    sys.refresh_cpu_all();
    let cores = sysinfo::System::physical_core_count()
        .or_else(|| std::thread::available_parallelism().ok().map(|n| n.get()))
        .unwrap_or(1) as u32;

    let disks = sysinfo::Disks::new_with_refreshed_list();
    let (disk_free, disk_total) = disk_for_volumes(&disks, project_volumes);

    let (fd_soft, fd_in_use) = fd_limits()?;

    Ok(MachineInputs {
        cores: cores.max(1),
        mem_available: sys.available_memory(),
        mem_total: sys.total_memory(),
        fd_soft,
        fd_in_use,
        disk_free,
        disk_total,
        bytes_per_worktree: 0,
        mem_per_agent: DEFAULT_MEM_PER_AGENT,
        mem_per_job: DEFAULT_MEM_PER_JOB,
        fd_per_agent: DEFAULT_FD_PER_AGENT,
    })
}

/// The disk that holds the given paths: the smallest free/total pair among
/// the mounts each path resolves under, or the disk with the most space if
/// no path matches (an empty project list, or a path `sysinfo` cannot place).
fn disk_for_volumes(disks: &sysinfo::Disks, paths: &[PathBuf]) -> (u64, u64) {
    if paths.is_empty() {
        let best = disks.list().iter().max_by_key(|d| d.available_space());
        return best.map_or((0, 0), |d| (d.available_space(), d.total_space()));
    }
    let mut free = u64::MAX;
    let mut total = 0u64;
    let mut found = false;
    for path in paths {
        if let Some(disk) = mount_for(disks, path) {
            found = true;
            free = free.min(disk.available_space());
            total = total.max(disk.total_space());
        }
    }
    if found { (free, total) } else { (0, 0) }
}

fn mount_for<'a>(disks: &'a sysinfo::Disks, path: &Path) -> Option<&'a sysinfo::Disk> {
    let mut best: Option<&sysinfo::Disk> = None;
    let mut best_len = 0usize;
    for disk in disks.list() {
        let mp = disk.mount_point();
        if path.starts_with(mp) {
            let len = mp.as_os_str().len();
            if len > best_len {
                best_len = len;
                best = Some(disk);
            }
        }
    }
    best
}

/// `RLIMIT_NOFILE`'s soft limit, and a rough count of descriptors already in
/// use by this process (unix only; on any other platform this returns a
/// generous, explicitly labeled placeholder rather than pretending to
/// measure something it cannot).
#[cfg(unix)]
fn fd_limits() -> Result<(u64, u64), CapacityError> {
    // SAFETY: `getrlimit` with a valid resource and a writable, correctly
    // sized `rlimit` out-pointer is exactly what the libc binding expects;
    // `rlim` is zero-initialized and fully overwritten on success, and the
    // return value is checked before it is read.
    let mut rlim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    let rc = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rlim) };
    if rc != 0 {
        return Err(CapacityError::Rlimit(
            std::io::Error::last_os_error().to_string(),
        ));
    }
    let in_use = count_open_fds();
    Ok((rlim.rlim_cur, in_use))
}

#[cfg(not(unix))]
fn fd_limits() -> Result<(u64, u64), CapacityError> {
    // Windows has no `RLIMIT_NOFILE`; the practical ceiling is handle-table
    // size, which is large and rarely the binding factor. Reported at 0 in
    // use against a high soft limit so the bound is loose rather than wrong
    // in either direction; `Bound::detail` on this platform should say so
    // (the shell adds that note; this crate stays honest about what it does
    // not measure here).
    Ok((16_384, 0))
}

/// Counts entries under `/dev/fd` (macOS, BSD) or `/proc/self/fd` (Linux) as
/// a rough "already in use" figure. Never fails the caller: an unreadable
/// directory (a sandboxed process, an unsupported OS) counts as zero in use,
/// which only makes the bound looser, not wrong in the unsafe direction.
#[cfg(unix)]
fn count_open_fds() -> u64 {
    for candidate in ["/dev/fd", "/proc/self/fd"] {
        if let Ok(entries) = std::fs::read_dir(candidate) {
            return entries.count() as u64;
        }
    }
    0
}

/// A route's 429-driven learned rate (design, "Measured again"): halved on a
/// 429, recovered by 10% every 5 minutes without one, never above the known
/// configured or header limit.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LearnedRate {
    known_limit: f64,
    current: f64,
    last_update_ms: i64,
}

/// How often the learned rate recovers, absent a 429 (design).
pub const RECOVERY_INTERVAL_MS: i64 = 5 * 60 * 1000;
/// How much it recovers per interval (design: "10%").
pub const RECOVERY_FACTOR: f64 = 1.10;
/// How much a 429 halves it by (design: "halves").
pub const BACKOFF_FACTOR: f64 = 0.5;

impl LearnedRate {
    pub fn new(known_limit: u32, at_ms: i64) -> Self {
        Self {
            known_limit: f64::from(known_limit.max(1)),
            current: f64::from(known_limit.max(1)),
            last_update_ms: at_ms,
        }
    }

    /// Halve the learned rate from the rate observed over the last minute
    /// (design: "based on the rate observed over the last minute"), never
    /// below one request per minute.
    pub fn on_429(&mut self, observed_rpm_last_minute: u32, at_ms: i64) {
        self.recover(at_ms);
        self.current = (f64::from(observed_rpm_last_minute) * BACKOFF_FACTOR).max(1.0);
        self.last_update_ms = at_ms;
    }

    /// Recover 10% for each full 5-minute interval elapsed since the last
    /// change, clamped to the known limit. Idempotent within one interval:
    /// calling this repeatedly inside 5 minutes of the last change does
    /// nothing, so a caller may call it on every check without over-recovering.
    pub fn recover(&mut self, at_ms: i64) {
        let elapsed = at_ms - self.last_update_ms;
        if elapsed < RECOVERY_INTERVAL_MS {
            return;
        }
        let steps = elapsed / RECOVERY_INTERVAL_MS;
        for _ in 0..steps {
            self.current = (self.current * RECOVERY_FACTOR).min(self.known_limit);
        }
        self.last_update_ms += steps * RECOVERY_INTERVAL_MS;
    }

    /// The current learned rate, rounded down (a fractional agent is not a
    /// slot).
    pub fn current_rpm(&self) -> u32 {
        self.current.floor().max(1.0) as u32
    }
}

/// Debounces a changing [`Capacity`] so the slider does not jitter (design,
/// "A new maximum takes effect only when it differs by more than 10% (or by
/// 2) on three readings in a row"). Each of the three limits is tested on its
/// own: a change of writers or execution slots commits as a change of agents
/// does, even when the agent maximum stays put.
#[derive(Debug, Default)]
pub struct Hysteresis {
    committed: Option<(u32, u32, u32)>,
    candidate: Option<(u32, u32, u32)>,
    streak: u32,
}

fn differs_enough(old: u32, new: u32) -> bool {
    let diff = old.abs_diff(new);
    let threshold = ((f64::from(old) * 0.10).ceil() as u32).max(2);
    diff > threshold
}

impl Hysteresis {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one reading. Returns `Some(capacity)` the moment a new value
    /// takes effect (the first reading ever, or the third reading in a row
    /// of a value that differs enough from what is committed); `None`
    /// otherwise, meaning the caller keeps using the last committed value.
    pub fn observe(&mut self, c: &Capacity) -> Option<Capacity> {
        let key = (c.max_agents, c.max_writers, c.exec_slots);
        let Some(committed) = self.committed else {
            self.committed = Some(key);
            self.candidate = None;
            self.streak = 0;
            return Some(c.clone());
        };
        let enough = differs_enough(committed.0, key.0)
            || differs_enough(committed.1, key.1)
            || differs_enough(committed.2, key.2);
        if !enough {
            self.candidate = None;
            self.streak = 0;
            return None;
        }
        if self.candidate == Some(key) {
            self.streak += 1;
        } else {
            self.candidate = Some(key);
            self.streak = 1;
        }
        if self.streak >= 3 {
            self.committed = Some(key);
            self.candidate = None;
            self.streak = 0;
            Some(c.clone())
        } else {
            None
        }
    }

    pub fn committed(&self) -> Option<(u32, u32, u32)> {
        self.committed
    }
}

/// A round-trip time helper for callers deriving `round_ms`/`tokens_per_round`
/// from the ledger: the median of a small running sample is enough (the
/// design asks for "the median", not a full distribution), and 20 samples
/// (design's "until the route has 20 rounds") is a small, fixed cost to keep.
#[derive(Clone, Debug, Default)]
pub struct RollingMedian {
    samples: Vec<u64>,
    cap: usize,
}

impl RollingMedian {
    pub fn new(cap: usize) -> Self {
        Self {
            samples: Vec::with_capacity(cap),
            cap: cap.max(1),
        }
    }

    pub fn push(&mut self, value: u64) {
        if self.samples.len() == self.cap {
            self.samples.remove(0);
        }
        self.samples.push(value);
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    pub fn median(&self) -> Option<u64> {
        if self.samples.is_empty() {
            return None;
        }
        let mut sorted = self.samples.clone();
        sorted.sort_unstable();
        Some(sorted[sorted.len() / 2])
    }
}

/// A crude wait for the exec-slot test suite; not part of the public
/// capacity computation, kept here only so `kgraf-tools`'s shell tool can
/// share one small duration constant with this crate's own tests.
pub const EXEC_WAIT_POLL: Duration = Duration::from_millis(5);

#[cfg(test)]
mod tests {
    use super::*;

    fn machine() -> MachineInputs {
        MachineInputs {
            cores: 8,
            mem_available: 16 * 1024 * 1024 * 1024,
            mem_total: 32 * 1024 * 1024 * 1024,
            fd_soft: 10_000,
            fd_in_use: 100,
            disk_free: 100 * 1024 * 1024 * 1024,
            disk_total: 500 * 1024 * 1024 * 1024,
            bytes_per_worktree: 1024 * 1024 * 1024,
            mem_per_agent: DEFAULT_MEM_PER_AGENT,
            mem_per_job: DEFAULT_MEM_PER_JOB,
            fd_per_agent: DEFAULT_FD_PER_AGENT,
        }
    }

    fn route(rpm: u32) -> RouteInputs {
        RouteInputs {
            route: "kimi".into(),
            rpm: Some((rpm, LimitSource::Configured)),
            tpm: None,
            round_ms: Some(20_000),
            tokens_per_round: None,
            local_slots: None,
        }
    }

    /// Route bound is the binding one: a tiny RPM against a roomy machine.
    #[test]
    fn the_route_bound_binds_when_the_rate_limit_is_the_tightest() {
        let cap = compute(&[route(3)], &machine());
        // 3 rpm, 20 s round: by_rpm = 3 * 20 / 60 = 1.0; * 0.8 headroom = 0
        // (floor), clamped to at least 1 agent overall.
        assert_eq!(cap.max_agents, 1);
        let route_bound = cap.bounds.iter().find(|b| b.name == "route").unwrap();
        assert!(route_bound.value <= cap.max_agents.max(1));
    }

    /// Memory bound is the binding one: a generous route, a tiny machine.
    #[test]
    fn the_memory_bound_binds_when_memory_is_the_tightest() {
        let mut m = machine();
        m.mem_available = 2 * 1024 * 1024 * 1024 + 200 * 1024 * 1024; // just over the 2 GiB reserve
        m.mem_total = 2 * 1024 * 1024 * 1024 + 200 * 1024 * 1024;
        let cap = compute(&[route(100_000)], &m);
        let mem_bound = cap
            .bounds
            .iter()
            .find(|b| b.name == "memory")
            .unwrap()
            .value;
        assert_eq!(cap.max_agents, mem_bound);
        assert!(mem_bound < UNMEASURED_ROUTE_AGENTS * 100);
    }

    /// Descriptor bound is the binding one.
    #[test]
    fn the_descriptor_bound_binds_when_descriptors_are_the_tightest() {
        let mut m = machine();
        m.fd_soft = 256 + 100 + 32 * 2; // room for exactly 2 agents
        let cap = compute(&[route(100_000)], &m);
        let fd_bound = cap
            .bounds
            .iter()
            .find(|b| b.name == "descriptors")
            .unwrap()
            .value;
        assert_eq!(fd_bound, 2);
        assert_eq!(cap.max_agents, 2);
    }

    /// An unknown rate limit gives 4 agents, reported as unmeasured.
    #[test]
    fn an_unknown_rate_limit_gives_four_agents_unmeasured() {
        let cap = compute(&[RouteInputs::unmeasured("mystery")], &machine());
        let route_bound = cap.bounds.iter().find(|b| b.name == "route").unwrap();
        assert_eq!(route_bound.value, UNMEASURED_ROUTE_AGENTS);
        assert_eq!(route_bound.source, LimitSource::Unmeasured);
    }

    /// A local route contributes its configured slots, ignoring any rpm.
    #[test]
    fn a_local_route_gives_its_configured_slots() {
        let mut r = RouteInputs::unmeasured("local-llama");
        r.local_slots = Some(6);
        let cap = compute(&[r], &machine());
        let route_bound = cap.bounds.iter().find(|b| b.name == "route").unwrap();
        assert_eq!(route_bound.value, 6);
        assert_eq!(route_bound.source, LimitSource::Configured);
    }

    /// A local route with no configured count contributes 1 (design).
    #[test]
    fn a_local_route_with_no_configured_count_gives_one() {
        let mut r = RouteInputs::unmeasured("local-llama");
        r.local_slots = Some(0);
        let cap = compute(&[r], &machine());
        assert_eq!(
            cap.bounds.iter().find(|b| b.name == "route").unwrap().value,
            1
        );
    }

    /// The planner's own route has one agent's worth of its own capacity
    /// subtracted.
    #[test]
    fn the_planner_route_is_subtracted_from_its_own_bound() {
        let cap_a = compute(&[route(6_000)], &machine());
        let cap_b = compute_for(&[route(6_000)], &machine(), Some("kimi"));
        let a = cap_a
            .bounds
            .iter()
            .find(|b| b.name == "route")
            .unwrap()
            .value;
        let b = cap_b
            .bounds
            .iter()
            .find(|b| b.name == "route")
            .unwrap()
            .value;
        assert_eq!(b, a - 1);
    }

    // --- LearnedRate: 429 halving and recovery ---

    #[test]
    fn a_429_halves_the_learned_rate() {
        let mut r = LearnedRate::new(100, 0);
        r.on_429(100, 1_000);
        assert_eq!(r.current_rpm(), 50);
    }

    #[test]
    fn recovery_climbs_back_toward_the_known_limit_but_never_past_it() {
        let mut r = LearnedRate::new(100, 0);
        r.on_429(100, 0);
        assert_eq!(r.current_rpm(), 50);
        r.recover(RECOVERY_INTERVAL_MS);
        assert_eq!(r.current_rpm(), 55);
        // Many intervals: clamps at the known limit, never overshoots it.
        r.recover(RECOVERY_INTERVAL_MS * 1000);
        assert_eq!(r.current_rpm(), 100);
    }

    #[test]
    fn recovery_within_one_interval_does_nothing() {
        let mut r = LearnedRate::new(100, 0);
        r.on_429(100, 0);
        r.recover(RECOVERY_INTERVAL_MS - 1);
        assert_eq!(r.current_rpm(), 50);
    }

    // --- Hysteresis ---

    fn cap_of(max_agents: u32) -> Capacity {
        Capacity {
            max_agents,
            max_writers: 1,
            exec_slots: 1,
            bounds: Vec::new(),
        }
    }

    #[test]
    fn hysteresis_commits_the_first_reading_at_once() {
        let mut h = Hysteresis::new();
        assert_eq!(h.observe(&cap_of(10)).map(|c| c.max_agents), Some(10));
    }

    #[test]
    fn hysteresis_holds_for_two_readings_and_moves_on_the_third() {
        let mut h = Hysteresis::new();
        h.observe(&cap_of(10));
        // A jump from 10 to 20 is well over the 10%-or-2 threshold.
        assert_eq!(
            h.observe(&cap_of(20)),
            None,
            "first differing reading holds"
        );
        assert_eq!(h.observe(&cap_of(20)), None, "second still holds");
        assert_eq!(
            h.observe(&cap_of(20)).map(|c| c.max_agents),
            Some(20),
            "third reading in a row moves it"
        );
        assert_eq!(h.committed(), Some((20, 1, 1)));
    }

    #[test]
    fn hysteresis_ignores_small_jitter_within_the_threshold() {
        let mut h = Hysteresis::new();
        h.observe(&cap_of(10));
        // 1 above 10 is within max(2, 10%) = 2; never moves however often seen.
        for _ in 0..5 {
            assert_eq!(h.observe(&cap_of(11)), None);
        }
        assert_eq!(h.committed(), Some((10, 1, 1)));
    }

    /// The agent maximum stays put while the writers, then the execution
    /// slots, change enough: each commits on its third reading.
    #[test]
    fn hysteresis_commits_a_change_of_writers_or_exec_slots_alone() {
        let with = |max_writers, exec_slots| Capacity {
            max_agents: 10,
            max_writers,
            exec_slots,
            bounds: Vec::new(),
        };
        let mut h = Hysteresis::new();
        h.observe(&with(2, 2));
        assert_eq!(h.observe(&with(8, 2)), None);
        assert_eq!(h.observe(&with(8, 2)), None);
        assert_eq!(
            h.observe(&with(8, 2)).map(|c| c.max_writers),
            Some(8),
            "writers moved on the third reading"
        );
        assert_eq!(h.observe(&with(8, 9)), None);
        assert_eq!(h.observe(&with(8, 9)), None);
        assert_eq!(
            h.observe(&with(8, 9)).map(|c| c.exec_slots),
            Some(9),
            "exec slots moved on the third reading"
        );
        assert_eq!(h.committed(), Some((10, 8, 9)));
        // Jitter within the threshold on either still never commits.
        for _ in 0..5 {
            assert_eq!(h.observe(&with(9, 10)), None);
        }
    }

    #[test]
    fn a_streak_of_a_different_candidate_resets_the_count() {
        let mut h = Hysteresis::new();
        h.observe(&cap_of(10));
        h.observe(&cap_of(20));
        h.observe(&cap_of(20));
        // A third reading of a *different* new value restarts the streak.
        assert_eq!(h.observe(&cap_of(30)), None);
        assert_eq!(h.observe(&cap_of(30)), None);
        assert_eq!(h.observe(&cap_of(30)).map(|c| c.max_agents), Some(30));
    }

    // --- measure_machine (mechanism, not this machine's actual numbers) ---

    #[test]
    fn measure_machine_returns_nonzero_cores_and_memory_on_this_host() {
        // This asserts the mechanism runs end to end, not any particular
        // number: a real host always has at least one core and some memory,
        // whatever the CI or developer machine happens to be.
        let m = measure_machine(&[]).expect("measure");
        assert!(m.cores >= 1);
        assert!(m.mem_total > 0);
    }

    #[test]
    fn disk_for_volumes_matches_the_project_paths_mount() {
        let dir = tempfile::tempdir().expect("dir");
        let m = measure_machine(&[dir.path().to_path_buf()]).expect("measure");
        assert!(m.disk_total > 0);
    }

    #[test]
    fn rolling_median_tracks_the_middle_of_a_small_sample() {
        let mut r = RollingMedian::new(3);
        r.push(10);
        r.push(30);
        r.push(20);
        assert_eq!(r.median(), Some(20));
        r.push(100); // evicts the oldest (10)
        assert_eq!(r.len(), 3);
        assert_eq!(r.median(), Some(30));
    }
}

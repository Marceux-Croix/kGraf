//! Bounded autonomy (decision 0017) and spend accounting at admission (review §A6).
//!
//! **The concurrency limits are pre-flight.** [`AdmissionGate::try_admit`] refuses a
//! work item before its workers exist; nothing here kills a worker after it started.
//! The gate counts work items in flight (the user-facing
//! cap) and live agents (the hard ceiling recursive decomposition cannot breach)
//! separately, because a work item with parallel attempts holds several agents.
//!
//! **kGraf does not limit spend** (decision 0080, superseding the
//! spend-cap parts of 0017): [`BudgetBook::reserve`] never refuses. Money caps are
//! the user's own provider console, not this process. What remains is accounting:
//! a hold is estimated cost tracked from admission to settlement so `spent_run` and
//! `spent_task` (and `kgraf spend`) reflect real, settled cost, and every token is attributed to
//! the ledger. The hold and settle machinery is ported from LibreChat
//! `packages/api/src/middleware/checkBalance.ts` (`checkBalance`, `holdReservation`), commit
//! 7b2362d7a7c6148b84850924dc7fa5fc43307923, MIT (license read at the source 2026-09-26; see
//! `third_party/librechat/UPSTREAM`); upstream used the hold to refuse a dispatch that would
//! overspend a balance, which decision 0080 removes. A hold is renewed at half its time-to-live,
//! measured from the stored expiry, so only a hold nobody renews (a forwarder that died) lapses;
//! the orchestrator loop calls [`BudgetBook::renew_due`] on its tick, since the loop already wakes
//! for lease renewal (0048) and a timer per hold would be a task per dispatch.

use kgraf_core::{Money, ParallelismLimits};
use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

/// The starting user-facing cap: inside the three-to-five band 0017 names, a
/// starting point to tune with measurement, not a researched number.
pub const DEFAULT_MAX_WORK_ITEMS: usize = 4;
/// The starting hard ceiling on live agents.
pub const DEFAULT_MAX_LIVE_AGENTS: usize = 8;
/// How long a budget hold lives without renewal. Same order as the lease TTL.
pub const DEFAULT_RESERVATION_TTL_MS: i64 = 120_000;
const HOUR_MS: i64 = 60 * 60 * 1_000;

/// The two limits of decision 0017.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    max_work_items: usize,
    max_live_agents: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LimitsError {
    #[error("a limit of zero admits nothing")]
    Zero,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_work_items: DEFAULT_MAX_WORK_ITEMS,
            max_live_agents: DEFAULT_MAX_LIVE_AGENTS,
        }
    }
}

impl Limits {
    pub fn new(max_work_items: usize, max_live_agents: usize) -> Result<Self, LimitsError> {
        if max_work_items == 0 || max_live_agents == 0 {
            return Err(LimitsError::Zero);
        }
        Ok(Self {
            max_work_items,
            max_live_agents,
        })
    }

    pub fn max_work_items(&self) -> usize {
        self.max_work_items
    }

    pub fn max_live_agents(&self) -> usize {
        self.max_live_agents
    }
}

/// Why a spawn was refused before it started.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("{in_flight} work items are in flight; the cap is {cap}")]
    WorkItemCap { in_flight: usize, cap: usize },
    #[error("{live} agents are live and {requested} more were asked for; the ceiling is {ceiling}")]
    LiveAgentCeiling {
        live: usize,
        requested: usize,
        ceiling: usize,
    },
}

/// Why an [`AdmissionGate`] refuses a caller's own bookkeeping request (as
/// opposed to [`Refusal`], which is why a spawn itself was refused capacity).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AdmissionError {
    /// [`AdmissionGate::release_one`] was called on an [`Admitted`] that
    /// already covers zero agents (a caller
    /// bug is reported through the crate's other refusal path, a typed
    /// `Result`, rather than a panic that would take the whole process down
    /// for one double-release).
    #[error("release_one called on an admission with no agent left to release")]
    AlreadyReleased,
}

/// Proof a work item was admitted. Not `Clone`: it is returned to the gate once.
#[must_use = "an admission holds capacity until it is released"]
#[derive(Debug)]
pub struct Admitted {
    agents: usize,
    /// Agent slots also reserved against a shared [`MachineGate`] (0, when
    /// this gate has none): released alongside the local count so the two
    /// never drift apart.
    machine_taken: u32,
}

impl Admitted {
    pub fn agents(&self) -> usize {
        self.agents
    }
}

/// The process-wide, machine-sized ceiling every project's [`Admission`]
/// shares (decision 0067, "Scope": "one `MachineGate` shared by every
/// project's `Admission`"). Agent-slot accounting is a synchronous,
/// lock-free reservation (atomics): an admission spawn is a pre-flight check
/// that must never await, and the refusal must come before anything
/// starts, not after a wait. Execution slots are a real
/// [`tokio::sync::Semaphore`], because the design explicitly wants a shell
/// call that finds no free slot to *wait*, visibly, rather than be refused
/// (design, "Cores do not bound agents": "A shell call that finds no free
/// slot waits, and its wait state says 'waiting for a machine slot'").
pub struct MachineGate {
    max_agents: AtomicU32,
    agents_live: AtomicU32,
    limits: std::sync::Mutex<ParallelismLimits>,
    /// Swapped wholesale on `set_limits` rather than resized in place: a
    /// permit already handed out from the old `Semaphore` keeps a live shell
    /// call running to completion (pre-flight only, nothing already running
    /// is ever cancelled by a lowered limit), and a fresh `Semaphore` at the
    /// new size is simpler and just as correct for every *future* acquire.
    exec: std::sync::RwLock<Arc<tokio::sync::Semaphore>>,
}

/// A held execution slot (one running shell-tool process). Dropping it frees
/// the slot for the next waiter.
pub struct ExecSlot(#[allow(dead_code)] tokio::sync::OwnedSemaphorePermit);

/// The one [`MachineGate`] of this process (decision 0067, "Scope": "The
/// slider and its gate are therefore process-wide: one `MachineGate` shared
/// by every project's `Admission`"; owner answer 4, 2026-09-27: the slider
/// binds this app process only, not across processes). Every [`Host`] this
/// process opens, one per project, passes its own `Limits`
/// to [`Admission::with_machine`], but shares this one gate, so two
/// projects' admissions together still cannot exceed the machine's own
/// ceiling. `default_limits` seeds the gate only on the very first call in
/// this process; a later call with a different value does not reopen it
/// (the shell's re-measure thread calls [`MachineGate::set_limits`] on the
/// already-open gate instead, once `kgraf-capacity::compute` has real
/// numbers).
///
/// [`Host`]: crate's own consumers in `kgraf-host`; not named as a type here
/// to avoid a dependency cycle (`kgraf-host` depends on
/// `kgraf-orchestrator`, not the reverse).
pub fn process_machine_gate(default_limits: ParallelismLimits) -> Arc<MachineGate> {
    static GATE: std::sync::OnceLock<Arc<MachineGate>> = std::sync::OnceLock::new();
    GATE.get_or_init(|| MachineGate::new(default_limits))
        .clone()
}

impl MachineGate {
    pub fn new(limits: ParallelismLimits) -> Arc<Self> {
        Arc::new(Self {
            max_agents: AtomicU32::new(limits.max_agents),
            agents_live: AtomicU32::new(0),
            exec: std::sync::RwLock::new(Arc::new(tokio::sync::Semaphore::new(
                limits.exec_slots.max(1) as usize,
            ))),
            limits: std::sync::Mutex::new(limits),
        })
    }

    /// Change the limits while agents run. Lowering `max_agents` refuses the
    /// next admission but kills nothing already live, exactly as
    /// [`AdmissionGate::set_limits`] does for the local ceiling (0017's
    /// pre-flight rule, carried to the shared gate).
    pub fn set_limits(&self, limits: ParallelismLimits) {
        let old_exec = {
            let mut cur = self.limits.lock().unwrap_or_else(|p| p.into_inner());
            let old = cur.exec_slots;
            *cur = limits;
            old
        };
        self.max_agents
            .store(limits.max_agents, AtomicOrdering::SeqCst);
        let exec = self.exec.write().unwrap_or_else(|p| p.into_inner());
        match limits.exec_slots.cmp(&old_exec) {
            std::cmp::Ordering::Greater => {
                exec.add_permits((limits.exec_slots - old_exec) as usize)
            }
            std::cmp::Ordering::Less => {
                exec.forget_permits((old_exec - limits.exec_slots) as usize);
            }
            std::cmp::Ordering::Equal => {}
        }
    }

    pub fn limits(&self) -> ParallelismLimits {
        *self.limits.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn agents_live(&self) -> u32 {
        self.agents_live.load(AtomicOrdering::SeqCst)
    }

    /// Reserve `n` agent slots against the shared ceiling, or refuse without
    /// changing anything (a spawn over the shared limit is refused
    /// before it starts, and two projects together cannot pass the machine
    /// limit).
    fn try_take_agents(&self, n: u32) -> bool {
        let max = self.max_agents.load(AtomicOrdering::SeqCst);
        let mut cur = self.agents_live.load(AtomicOrdering::SeqCst);
        loop {
            if cur.saturating_add(n) > max {
                return false;
            }
            match self.agents_live.compare_exchange_weak(
                cur,
                cur + n,
                AtomicOrdering::SeqCst,
                AtomicOrdering::SeqCst,
            ) {
                Ok(_) => return true,
                Err(actual) => cur = actual,
            }
        }
    }

    fn release_agents(&self, n: u32) {
        self.agents_live.fetch_sub(n, AtomicOrdering::SeqCst);
    }

    /// Take one execution slot, waiting if none is free. The caller's own
    /// wait-state reporting is what names the wait "waiting for a machine
    /// slot" (`kgraf-tools::shell`); this method only blocks until a permit
    /// is free.
    pub async fn exec_slot(&self) -> ExecSlot {
        let sem = self.exec.read().unwrap_or_else(|p| p.into_inner()).clone();
        // `acquire_owned` on a semaphore this type never closes cannot
        // return `Err`: closing would need a `close()` call this type never
        // makes.
        let permit = sem
            .acquire_owned()
            .await
            .expect("MachineGate's exec semaphore is never closed");
        ExecSlot(permit)
    }
}

/// `kgraf-tools::ShellTool` gates its command executions through
/// [`kgraf_tools::ExecGate`], a trait rather than this type directly, because
/// `kgraf-orchestrator` already depends on `kgraf-tools`
/// (`kgraf_tools::ExecGate`'s own doc comment explains why that rules out the
/// reverse dependency `kgraf-tools` would need to name `MachineGate`
/// itself). Implementing that trait for `MachineGate` here, in the crate that
/// owns both dependencies, is what lets `kgraf-host` hand one `Arc<MachineGate>`
/// to both `Admission::with_machine` and `ShellTool::with_exec_gate`.
#[async_trait::async_trait]
impl kgraf_tools::ExecGate for MachineGate {
    async fn acquire(&self) -> Box<dyn Send + Sync> {
        Box::new(self.exec_slot().await)
    }
}

/// The gate every spawn passes through.
#[derive(Debug)]
pub struct AdmissionGate {
    limits: Limits,
    items: usize,
    agents: usize,
    refused_items: u64,
    refused_agents: u64,
    /// Shared with every other project's `AdmissionGate` in this process
    /// (decision 0067). `None` in the tests above and in any caller that has
    /// not yet adopted the shared gate: `try_admit`/`release` behave exactly
    /// as before when this is absent, so no existing caller's behavior
    /// changes just from this field's addition.
    machine: Option<Arc<MachineGate>>,
}

impl std::fmt::Debug for MachineGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MachineGate")
            .field("max_agents", &self.max_agents.load(AtomicOrdering::Relaxed))
            .field(
                "agents_live",
                &self.agents_live.load(AtomicOrdering::Relaxed),
            )
            .finish()
    }
}

impl AdmissionGate {
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            items: 0,
            agents: 0,
            refused_items: 0,
            refused_agents: 0,
            machine: None,
        }
    }

    /// As [`AdmissionGate::new`], additionally reserving every admission
    /// against `machine` before this gate's own local counts (design,
    /// "Scope"): several projects, or several `AdmissionGate`s in one
    /// process's tests, sharing one `MachineGate` cannot together exceed it.
    pub fn new_with_machine(limits: Limits, machine: Arc<MachineGate>) -> Self {
        Self {
            limits,
            items: 0,
            agents: 0,
            refused_items: 0,
            refused_agents: 0,
            machine: Some(machine),
        }
    }

    /// Admit one work item that will run `agents` workers, or refuse it.
    pub fn try_admit(&mut self, agents: usize) -> Result<Admitted, Refusal> {
        let agents = agents.max(1);
        // The shared machine ceiling is checked, and reserved, before this
        // gate's own local counts change at all: a refusal here leaves both
        // this gate's own state and the machine gate's exactly as they were.
        if let Some(machine) = &self.machine {
            let n = u32::try_from(agents).unwrap_or(u32::MAX);
            if !machine.try_take_agents(n) {
                self.refused_agents += 1;
                return Err(Refusal::LiveAgentCeiling {
                    live: machine.agents_live() as usize,
                    requested: agents,
                    ceiling: machine.limits().max_agents as usize,
                });
            }
        }
        if self.items >= self.limits.max_work_items {
            self.refused_items += 1;
            if let Some(machine) = &self.machine {
                machine.release_agents(u32::try_from(agents).unwrap_or(u32::MAX));
            }
            return Err(Refusal::WorkItemCap {
                in_flight: self.items,
                cap: self.limits.max_work_items,
            });
        }
        if self.agents + agents > self.limits.max_live_agents {
            self.refused_agents += 1;
            if let Some(machine) = &self.machine {
                machine.release_agents(u32::try_from(agents).unwrap_or(u32::MAX));
            }
            return Err(Refusal::LiveAgentCeiling {
                live: self.agents,
                requested: agents,
                ceiling: self.limits.max_live_agents,
            });
        }
        self.items += 1;
        self.agents += agents;
        Ok(Admitted {
            agents,
            machine_taken: if self.machine.is_some() {
                u32::try_from(agents).unwrap_or(u32::MAX)
            } else {
                0
            },
        })
    }

    /// Give an admission back.
    pub fn release(&mut self, admitted: Admitted) {
        self.items = self.items.saturating_sub(1);
        self.agents = self.agents.saturating_sub(admitted.agents);
        if admitted.machine_taken > 0
            && let Some(machine) = &self.machine
        {
            machine.release_agents(admitted.machine_taken);
        }
    }

    /// Release one agent's slot from a multi-agent admission, without
    /// releasing the work item itself: for the orchestrator stopping
    /// one attempt of several under one work item while the others keep
    /// running. Frees exactly one slot, locally and (when this gate has one)
    /// on the shared [`MachineGate`], so a waiting spawn elsewhere can be
    /// admitted at once; the work-item cap is untouched until the caller
    /// eventually calls [`AdmissionGate::release`] with the same `admitted`
    /// (now covering the agents that are left, possibly zero) once every
    /// agent under it has finished.
    ///
    /// Returns `Err(AdmissionError::AlreadyReleased)` if `admitted` already
    /// covers zero agents: releasing an agent that was not held is a caller
    /// bug (double-release), reported rather than silently turned into a
    /// no-op, but as a typed refusal consistent with
    /// [`AdmissionGate::try_admit`]'s `Refusal` rather than a panic (a caller bug here
    /// must not bring down the whole process). A caller checks [`Admitted::agents`] first, or
    /// simply never calls this more times than [`AdmissionGate::try_admit`]'s `agents` argument for
    /// that admission.
    pub fn release_one(&mut self, admitted: &mut Admitted) -> Result<(), AdmissionError> {
        if admitted.agents == 0 {
            return Err(AdmissionError::AlreadyReleased);
        }
        admitted.agents -= 1;
        self.agents = self.agents.saturating_sub(1);
        if admitted.machine_taken > 0 {
            admitted.machine_taken -= 1;
            if let Some(machine) = &self.machine {
                machine.release_agents(1);
            }
        }
        Ok(())
    }

    pub fn in_flight(&self) -> (usize, usize) {
        (self.items, self.agents)
    }

    /// Refusals so far: (work-item cap, live-agent ceiling).
    pub fn refusals(&self) -> (u64, u64) {
        (self.refused_items, self.refused_agents)
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Change the limits while work runs (the settings surface's cap, 0017). Work
    /// already admitted keeps running; a lower cap refuses new admissions until
    /// enough of it finishes, and nothing is killed (pre-flight only).
    pub fn set_limits(&mut self, limits: Limits) {
        self.limits = limits;
    }
}

/// A hold on budget. Not `Clone`: settled or released once.
#[must_use = "a reservation holds budget until it is settled"]
#[derive(Debug)]
pub struct Reservation {
    id: u64,
    amount: Money,
}

impl Reservation {
    pub fn amount(&self) -> Money {
        self.amount
    }
}

#[derive(Clone, Debug)]
struct Hold {
    run: String,
    task: String,
    amount: Money,
    expires_ms: i64,
    /// Held for a worker that is in flight: never lapses; released only by
    /// settlement. A lapse exists for a hold nobody will settle, and an
    /// in-flight worker's forwarder always reports.
    pinned: bool,
}

/// A settled cost above the hold it was admitted on: the estimate undercounted
/// the call. Accounting only (decision 0080 removed every spend cap).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Overrun {
    pub run: String,
    pub task: String,
    pub held: Money,
    pub actual: Money,
}

/// Accounting counters: nothing here is a refusal (decision 0080).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BudgetStats {
    pub lapsed: u64,
    pub renewed: u64,
    /// Settlements above their hold: the estimate undercounted the call, an
    /// accounting fact with no consequence for dispatch (decision 0080).
    pub overruns: u64,
}

/// Holds and settled spend, for every run in the process: accounting only, since
/// decision 0080 removed every spend cap. `spent_run` and `spent_task` are what
/// `kgraf spend` and the shell's cost display read.
#[derive(Debug)]
pub struct BudgetBook {
    ttl_ms: i64,
    holds: BTreeMap<u64, Hold>,
    next_id: u64,
    spent_run: BTreeMap<String, Money>,
    spent_task: BTreeMap<String, Money>,
    /// Settled spend with its time, oldest first, for the trailing hour.
    recent: VecDeque<(i64, Money)>,
    /// Ledger calls already in `recent` through [`BudgetBook::seed_hour`]: a
    /// resume that charges the same call again adds it to its run and task but
    /// not to the hour a second time.
    hour_seeded: std::collections::BTreeSet<String>,
    stats: BudgetStats,
}

impl Default for BudgetBook {
    fn default() -> Self {
        Self::new()
    }
}

impl BudgetBook {
    pub fn new() -> Self {
        Self::with_ttl(DEFAULT_RESERVATION_TTL_MS)
    }

    pub fn with_ttl(ttl_ms: i64) -> Self {
        Self {
            ttl_ms: ttl_ms.max(1),
            holds: BTreeMap::new(),
            next_id: 0,
            spent_run: BTreeMap::new(),
            spent_task: BTreeMap::new(),
            recent: VecDeque::new(),
            hour_seeded: std::collections::BTreeSet::new(),
            stats: BudgetStats::default(),
        }
    }

    /// Put one ledger call's settled spend in the trailing hour, attributed to
    /// no run or task: a new book (a host reopened, a restarted process) keeps
    /// the hourly cap's history. Once per call id.
    pub fn seed_hour(&mut self, call: &str, at: i64, amount: Money) {
        if amount == Money::ZERO || !self.hour_seeded.insert(call.to_owned()) {
            return;
        }
        let pos = self.recent.partition_point(|(t, _)| *t <= at);
        self.recent.insert(pos, (at, amount));
    }

    /// [`BudgetBook::record_spend`] for one ledger call: its run and task are
    /// charged, and the hour too unless [`BudgetBook::seed_hour`] already put
    /// the call there.
    pub fn record_call_spend(&mut self, call: &str, run: &str, task: &str, actual: Money, at: i64) {
        if self.hour_seeded.contains(call) {
            if actual == Money::ZERO {
                return;
            }
            *self.spent_run.entry(run.to_owned()).or_insert(Money::ZERO) += actual;
            *self
                .spent_task
                .entry(task.to_owned())
                .or_insert(Money::ZERO) += actual;
            return;
        }
        self.record_spend(run, task, actual, at);
    }

    pub fn stats(&self) -> BudgetStats {
        self.stats
    }

    /// Settled spend for one run.
    pub fn spent_run(&self, run: &str) -> Money {
        self.spent_run.get(run).copied().unwrap_or(Money::ZERO)
    }

    /// Settled spend of one task.
    pub fn spent_task(&self, task: &str) -> Money {
        self.spent_task.get(task).copied().unwrap_or(Money::ZERO)
    }

    /// Money currently held, all runs.
    pub fn held(&self) -> Money {
        self.holds.values().map(|h| h.amount).sum()
    }

    /// Money currently held for one run.
    pub fn held_run(&self, run: &str) -> Money {
        self.holds
            .values()
            .filter(|h| h.run == run)
            .map(|h| h.amount)
            .sum()
    }

    /// Money currently held for one task.
    pub fn held_task(&self, task: &str) -> Money {
        self.holds
            .values()
            .filter(|h| h.task == task)
            .map(|h| h.amount)
            .sum()
    }

    /// Settled spend in the hour ending at `now`, all runs.
    pub fn spent_last_hour(&mut self, now: i64) -> Money {
        self.trim_window(now);
        self.recent.iter().map(|(_, m)| *m).sum()
    }

    fn trim_window(&mut self, now: i64) {
        while self
            .recent
            .front()
            .is_some_and(|(at, _)| *at <= now - HOUR_MS)
        {
            self.recent.pop_front();
        }
    }

    fn lapse(&mut self, now: i64) {
        let before = self.holds.len();
        self.holds.retain(|_, h| h.pinned || h.expires_ms > now);
        self.stats.lapsed += (before - self.holds.len()) as u64;
    }

    /// Hold `estimate` for `task` of `run`. Never refuses (decision 0080: kGraf
    /// does not limit spend); the hold exists only so [`BudgetBook::settle`] can
    /// compare the estimate to the actual, ledgered cost. A pinned hold (an
    /// in-flight worker's) never lapses.
    pub fn reserve(
        &mut self,
        run: &str,
        task: &str,
        estimate: Money,
        now: i64,
        pinned: bool,
    ) -> Reservation {
        self.lapse(now);
        let id = self.next_id;
        self.next_id += 1;
        self.holds.insert(
            id,
            Hold {
                run: run.to_owned(),
                task: task.to_owned(),
                amount: estimate,
                expires_ms: now + self.ttl_ms,
                pinned,
            },
        );
        Reservation {
            id,
            amount: estimate,
        }
    }

    /// Renew every unpinned live hold whose remaining life is at most half the
    /// TTL, measured from its stored expiry (upstream's rule). Returns how many.
    pub fn renew_due(&mut self, now: i64) -> usize {
        self.lapse(now);
        let mut n = 0;
        for hold in self.holds.values_mut().filter(|h| !h.pinned) {
            if hold.expires_ms - self.ttl_ms / 2 <= now {
                hold.expires_ms = now + self.ttl_ms;
                n += 1;
            }
        }
        self.stats.renewed += n as u64;
        n
    }

    /// Release the hold and record what was actually spent. A cost above the
    /// hold is returned as an [`Overrun`] and counted.
    #[must_use = "an overrun must be acted on, not dropped"]
    pub fn settle(
        &mut self,
        reservation: Reservation,
        run: &str,
        task: &str,
        actual: Money,
        now: i64,
    ) -> Option<Overrun> {
        self.holds.remove(&reservation.id);
        self.record_spend(run, task, actual, now);
        (actual > reservation.amount).then(|| {
            self.stats.overruns += 1;
            Overrun {
                run: run.to_owned(),
                task: task.to_owned(),
                held: reservation.amount,
                actual,
            }
        })
    }

    /// Release a hold that spent nothing.
    pub fn release(&mut self, reservation: Reservation) {
        self.holds.remove(&reservation.id);
    }

    /// Record spend that had no hold (spend found in the ledger on resume).
    pub fn record_spend(&mut self, run: &str, task: &str, actual: Money, at: i64) {
        if actual == Money::ZERO {
            return;
        }
        *self.spent_run.entry(run.to_owned()).or_insert(Money::ZERO) += actual;
        *self
            .spent_task
            .entry(task.to_owned())
            .or_insert(Money::ZERO) += actual;
        // Keep the window ordered even when resume seeds older entries late.
        let pos = self.recent.partition_point(|(t, _)| *t <= at);
        self.recent.insert(pos, (at, actual));
    }

    /// Whether the hold is still live (for tests and status).
    pub fn is_held(&self, reservation: &Reservation) -> bool {
        self.holds.contains_key(&reservation.id)
    }
}

/// The one admission gate and spend accounting book of the process (decision
/// 0017's concurrency limits bind work, not runs): every run started in the
/// process, scheduled runs included, takes capacity and records spend through
/// the same instance. No spend cap is enforced here (decision 0080).
#[derive(Debug)]
pub struct Admission {
    gate: std::sync::Mutex<AdmissionGate>,
    budget: std::sync::Mutex<BudgetBook>,
}

impl Admission {
    pub fn new(limits: Limits) -> std::sync::Arc<Self> {
        Self::with_ttl(limits, DEFAULT_RESERVATION_TTL_MS)
    }

    pub fn with_ttl(limits: Limits, ttl_ms: i64) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            gate: std::sync::Mutex::new(AdmissionGate::new(limits)),
            budget: std::sync::Mutex::new(BudgetBook::with_ttl(ttl_ms)),
        })
    }

    /// As [`Admission::new`], with every admission this instance grants also
    /// reserved against the shared `gate` (decision 0067). Every project's `Host` in this process
    /// passes the same `Arc<MachineGate>`, so two projects' admissions together still cannot exceed
    /// the machine's own ceiling.
    pub fn with_machine(limits: Limits, gate: Arc<MachineGate>) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            gate: std::sync::Mutex::new(AdmissionGate::new_with_machine(limits, gate)),
            budget: std::sync::Mutex::new(BudgetBook::new()),
        })
    }

    /// The gate. Held only for a synchronous check; never across an await.
    pub fn gate(&self) -> std::sync::MutexGuard<'_, AdmissionGate> {
        self.gate.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The budget. Held only for a synchronous check; never across an await.
    pub fn budget(&self) -> std::sync::MutexGuard<'_, BudgetBook> {
        self.budget.lock().unwrap_or_else(|p| p.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A call seeded into the hour (a new host) and charged again by a resume
    /// counts once in the hour, and fully in its run and task.
    #[test]
    fn a_seeded_call_counts_once_in_the_hour() {
        let mut book = BudgetBook::new();
        book.seed_hour("c1", 10_000, Money(40));
        book.seed_hour("c1", 10_000, Money(40));
        assert_eq!(book.spent_last_hour(20_000), Money(40), "once per call");
        book.record_call_spend("c1", "r1", "r1/t", Money(40), 10_000);
        assert_eq!(book.spent_last_hour(20_000), Money(40), "not twice");
        assert_eq!(book.spent_run("r1"), Money(40));
        assert_eq!(book.spent_task("r1/t"), Money(40));
        book.record_call_spend("c2", "r1", "r1/t", Money(5), 11_000);
        assert_eq!(
            book.spent_last_hour(20_000),
            Money(45),
            "an unseeded call counts"
        );
        assert_eq!(
            book.spent_last_hour(10_000 + HOUR_MS + 1),
            Money(5),
            "the hour moves on"
        );
    }

    #[test]
    fn exceeding_the_work_item_cap_is_refused_before_spawn() {
        let mut gate = AdmissionGate::new(Limits::new(2, 10).expect("limits"));
        let a = gate.try_admit(1).expect("first");
        let b = gate.try_admit(1).expect("second");
        assert_eq!(
            gate.try_admit(1).expect_err("third"),
            Refusal::WorkItemCap {
                in_flight: 2,
                cap: 2
            }
        );
        gate.release(a);
        let c = gate.try_admit(1).expect("after release");
        gate.release(b);
        gate.release(c);
        assert_eq!(gate.in_flight(), (0, 0));
        assert_eq!(gate.refusals(), (1, 0));
    }

    #[test]
    fn a_cap_changed_while_work_runs_binds_the_next_admission_and_kills_nothing() {
        let mut gate = AdmissionGate::new(Limits::new(3, 10).expect("limits"));
        let a = gate.try_admit(1).expect("first");
        let _b = gate.try_admit(1).expect("second");
        // Lowered below what is in flight: both keep running, the next is refused.
        gate.set_limits(Limits::new(1, 10).expect("limits"));
        assert_eq!(gate.in_flight(), (2, 2), "nothing admitted was dropped");
        assert_eq!(
            gate.try_admit(1).expect_err("over the new cap"),
            Refusal::WorkItemCap {
                in_flight: 2,
                cap: 1
            }
        );
        gate.release(a);
        assert!(gate.try_admit(1).is_err(), "still at the new cap of 1");
        // Raised: admitted at once.
        gate.set_limits(Limits::new(4, 10).expect("limits"));
        let _c = gate.try_admit(1).expect("under the raised cap");
        assert_eq!(gate.in_flight(), (2, 2));
    }

    #[test]
    fn the_live_agent_ceiling_binds_independently_of_the_item_cap() {
        let mut gate = AdmissionGate::new(Limits::new(5, 3).expect("limits"));
        let _a = gate.try_admit(2).expect("two attempts");
        assert_eq!(
            gate.try_admit(2).expect_err("ceiling"),
            Refusal::LiveAgentCeiling {
                live: 2,
                requested: 2,
                ceiling: 3
            }
        );
        let _b = gate.try_admit(1).expect("fits");
        assert_eq!(gate.in_flight(), (2, 3));
    }

    #[test]
    fn zero_limits_are_refused() {
        assert_eq!(Limits::new(0, 1), Err(LimitsError::Zero));
        assert_eq!(Limits::new(1, 0), Err(LimitsError::Zero));
    }

    /// Decision 0080: reservation never refuses, however large the estimate or
    /// however much is already committed. Every reservation still settles into
    /// the run's and task's accounted spend, so a run that would have exceeded
    /// the old caps runs to completion and its full cost is recorded.
    #[test]
    fn reservation_never_refuses_and_every_call_still_settles() {
        let mut book = BudgetBook::new();
        let a = book.reserve("r", "t1", Money(600), 0, false);
        let b = book.reserve("r", "t2", Money(600), 0, false);
        assert!(book.settle(a, "r", "t1", Money(600), 1).is_none());
        assert!(book.settle(b, "r", "t2", Money(600), 2).is_none());
        assert_eq!(book.spent_run("r"), Money(1_200), "both calls are ledgered");
        // A third, far larger reservation on the same run and task also never
        // refuses.
        let c = book.reserve("r", "t1", Money(1_000_000), 3, false);
        assert!(book.settle(c, "r", "t1", Money(1_000_000), 4).is_none());
        assert_eq!(book.spent_run("r"), Money(1_001_200));
        assert_eq!(book.spent_task("t1"), Money(1_000_600));
    }

    /// An overrun (settled above its hold) is still counted for accounting: it
    /// no longer pauses dispatch or refuses further reservations (decision
    /// 0080).
    #[test]
    fn an_overrun_is_counted_but_never_refuses_further_reservation() {
        let mut book = BudgetBook::new();
        let a = book.reserve("r", "t1", Money(300), 0, false);
        let over = book
            .settle(a, "r", "t1", Money(400), 0)
            .expect("400 over a 300 hold");
        assert_eq!((over.held, over.actual), (Money(300), Money(400)));
        assert_eq!(book.stats().overruns, 1);
        // Reserving again for the same task, already over its old hold, still
        // never refuses.
        let b = book.reserve("r", "t1", Money(200), 0, false);
        assert!(book.settle(b, "r", "t1", Money(200), 0).is_none());
        assert_eq!(book.spent_task("t1"), Money(600));
    }

    #[test]
    fn a_hold_renewed_at_half_ttl_survives_and_an_unrenewed_one_lapses() {
        let mut book = BudgetBook::with_ttl(100);
        let kept = book.reserve("r", "t1", Money(400), 0, false);
        assert_eq!(book.renew_due(49), 0, "not yet at half");
        assert_eq!(book.renew_due(50), 1, "renewed at half the TTL");
        assert!(book.is_held(&kept));
        let lost = book.reserve("r", "t2", Money(400), 60, false);
        // No renewal of either after this point: both lapse at their expiries.
        let _ = book.reserve("r", "t3", Money(400), 149, false);
        let _t3 = book.reserve("r", "t3", Money(400), 161, false);
        assert!(!book.is_held(&kept));
        assert!(!book.is_held(&lost));
        assert_eq!(book.stats().lapsed, 2);
    }

    #[test]
    fn a_pinned_hold_never_lapses_however_long_nothing_renews_it() {
        let mut book = BudgetBook::with_ttl(100);
        let worker = book.reserve("r", "t1", Money(900), 0, true);
        let _ = book.reserve("r", "t2", Money(200), 1_000_000, false);
        assert!(book.is_held(&worker));
        book.release(worker);
    }

    /// The process-wide singleton is one object across every call in this
    /// process, whatever `default_limits` a later caller passes (a second
    /// `Host` opening with different configured caps must still share the
    /// first `Host`'s gate, not open a second one).
    #[test]
    fn process_machine_gate_is_one_object_across_every_call() {
        let a = process_machine_gate(machine_limits(3));
        let b = process_machine_gate(machine_limits(99));
        assert!(Arc::ptr_eq(&a, &b));
    }

    // --- MachineGate: the shared, process-wide ceiling (decision 0067) ---

    fn machine_limits(max_agents: u32) -> ParallelismLimits {
        ParallelismLimits::derive(max_agents, max_agents, max_agents)
    }

    fn limits_with_exec(max_agents: u32, exec_slots: u32) -> ParallelismLimits {
        ParallelismLimits::derive(max_agents, max_agents, exec_slots)
    }

    /// Two `Admission`s (as two open projects would each have) sharing one
    /// `MachineGate` of 3 cannot together admit 4 agents: the fourth is
    /// refused before it spawns.
    #[test]
    fn two_admissions_on_one_machine_gate_cannot_together_exceed_it() {
        let gate = MachineGate::new(machine_limits(3));
        let a = Admission::with_machine(Limits::new(10, 10).expect("limits"), gate.clone());
        let b = Admission::with_machine(Limits::new(10, 10).expect("limits"), gate.clone());
        let admitted_a = a.gate().try_admit(2).expect("first project takes two");
        assert_eq!(gate.agents_live(), 2);
        let refused = b
            .gate()
            .try_admit(2)
            .expect_err("second project's two would make four, over the gate of three");
        assert_eq!(
            refused,
            Refusal::LiveAgentCeiling {
                live: 2,
                requested: 2,
                ceiling: 3
            }
        );
        // The refusal touched neither this admission's own local count...
        assert_eq!(b.gate().in_flight(), (0, 0));
        // ...nor the shared gate's.
        assert_eq!(gate.agents_live(), 2);
        let admitted_b = b.gate().try_admit(1).expect("one more fits");
        assert_eq!(gate.agents_live(), 3);
        a.gate().release(admitted_a);
        assert_eq!(gate.agents_live(), 1);
        b.gate().release(admitted_b);
        assert_eq!(gate.agents_live(), 0);
    }

    /// A limit lowered under load on the shared gate kills nothing already
    /// admitted, and refuses the next admission (0017's pre-flight rule,
    /// carried to the shared gate).
    #[test]
    fn a_lowered_shared_limit_kills_nothing_and_refuses_the_next_admission() {
        let gate = MachineGate::new(machine_limits(5));
        let admission = Admission::with_machine(Limits::new(10, 10).expect("limits"), gate.clone());
        let held = admission.gate().try_admit(3).expect("three admitted");
        gate.set_limits(machine_limits(2));
        assert_eq!(gate.agents_live(), 3, "nothing already admitted is dropped");
        assert!(
            admission.gate().try_admit(1).is_err(),
            "over the new, lower ceiling"
        );
        admission.gate().release(held);
        assert_eq!(gate.agents_live(), 0);
        let _ok = admission
            .gate()
            .try_admit(2)
            .expect("under the lowered ceiling once released");
    }

    /// One work item admitted with 3 agents, one attempt
    /// stops early. `release_one` frees exactly one slot (locally and on the
    /// shared gate), the work item's own slot stays held (a fourth agent for
    /// a *new* item still cannot exceed the ceiling), and a waiting spawn
    /// elsewhere is admitted at once.
    #[test]
    fn release_one_frees_a_single_agent_slot_without_releasing_the_work_item() {
        let gate = MachineGate::new(machine_limits(3));
        let a = Admission::with_machine(Limits::new(10, 10).expect("limits"), gate.clone());
        let b = Admission::with_machine(Limits::new(10, 10).expect("limits"), gate.clone());
        let mut held = a.gate().try_admit(3).expect("three agents, one item");
        assert_eq!(gate.agents_live(), 3);
        assert_eq!(a.gate().in_flight(), (1, 3));

        // A second project's admission cannot fit even one more: the gate is
        // full.
        assert!(b.gate().try_admit(1).is_err());

        a.gate()
            .release_one(&mut held)
            .expect("one of three to release");
        assert_eq!(held.agents(), 2, "two agents of the original three remain");
        assert_eq!(gate.agents_live(), 2, "the shared gate sees the freed slot");
        assert_eq!(
            a.gate().in_flight(),
            (1, 2),
            "the work item itself is still counted: two other agents run under it"
        );

        // The freed slot admits a waiting spawn elsewhere at once.
        let waiting = b
            .gate()
            .try_admit(1)
            .expect("the slot release_one freed admits this");

        // Releasing the last two agents (one at a time, then the item itself)
        // brings the gate back to only `waiting`'s reservation.
        a.gate()
            .release_one(&mut held)
            .expect("another of three to release");
        assert_eq!(held.agents(), 1);
        a.gate().release(held);
        assert_eq!(gate.agents_live(), 1, "only b's one agent remains");
        b.gate().release(waiting);
        assert_eq!(gate.agents_live(), 0);
    }

    /// Releasing an agent that was never held is a caller bug,
    /// reported as `Err(AdmissionError::AlreadyReleased)` rather than
    /// silently under-counting the gate (a double-release must not panic and take
    /// the whole process down; it is a typed refusal the caller can handle,
    /// consistent with `try_admit`'s `Refusal`).
    #[test]
    fn release_one_on_an_exhausted_admission_returns_already_released() {
        let mut gate = AdmissionGate::new(Limits::new(10, 10).expect("limits"));
        let mut held = gate.try_admit(1).expect("one agent");
        gate.release_one(&mut held).expect("one agent to release");
        assert_eq!(held.agents(), 0);
        assert_eq!(
            gate.release_one(&mut held),
            Err(AdmissionError::AlreadyReleased),
            "nothing left to release; the gate must not panic"
        );
    }

    /// With 2 exec slots, three concurrent shell calls run two at once and
    /// the third waits, visibly (its own `.await` on `exec_slot()` is the
    /// wait; the caller names the wait state).
    #[tokio::test]
    async fn with_two_exec_slots_a_third_concurrent_call_waits() {
        let gate = MachineGate::new(limits_with_exec(1, 2));
        let s1 = gate.exec_slot().await;
        let s2 = gate.exec_slot().await;
        let (tx, rx) = tokio::sync::oneshot::channel();
        let gate_clone = std::sync::Arc::clone(&gate);
        let waiter = tokio::spawn(async move {
            let _slot = gate_clone.exec_slot().await;
            let _ = tx.send(());
        });
        // The waiter must still be waiting a short time later: no third slot
        // exists yet.
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert!(
            !waiter.is_finished(),
            "a third caller must wait, not run past two slots"
        );
        drop(s1);
        rx.await.expect("the waiter proceeds once a slot frees");
        drop(s2);
        waiter.await.expect("no panic");
    }

    /// Raising `exec_slots` on a live gate lets a waiter through without
    /// waiting for any already-held slot to free.
    #[tokio::test]
    async fn raising_exec_slots_frees_a_waiter_at_once() {
        let gate = MachineGate::new(limits_with_exec(1, 1));
        let _held = gate.exec_slot().await;
        let gate_clone = std::sync::Arc::clone(&gate);
        let waiter = tokio::spawn(async move { gate_clone.exec_slot().await });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(!waiter.is_finished());
        gate.set_limits(limits_with_exec(1, 2));
        let _second = tokio::time::timeout(std::time::Duration::from_millis(200), waiter)
            .await
            .expect("did not time out")
            .expect("no panic");
    }
}

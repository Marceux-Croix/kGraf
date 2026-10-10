//! Per-route provider call rate limits (decision 0056).
//!
//! The limiter is `governor` (MIT, 0.10.4, license read on crates.io 2026-09-26): a
//! GCRA limiter, lock-free, with a quota of requests per minute and a burst. This
//! module adds only the policy around it: how long a call may wait for a slot, and
//! what happens when it may not wait that long.
//!
//! **Failing loud (fail loud).** A call that would wait longer than the route's
//! `max_wait_ms` is refused with [`ProviderError::RateLimited`] as soon as the
//! projected wait exceeds the limit, not after sleeping it out. The request never
//! left, so its ledger row is closed as failed ([`crate::error::Billing::NotBilled`]):
//! the refusal is attributed like every other call (every call is attributed to the ledger), and nothing is
//! dropped silently.
//!
//! **One limiter per route, per process, or one shared across every process on
//! this machine.** A provider's limit is per endpoint and key, not per model, so
//! every provider this process builds for a route (the planner's and every
//! worker's) wraps the same [`RouteLimiter`]. [`RouteLimiter::new`] is
//! process-local (`governor`, this module's tests and bench only);
//! [`RouteLimiter::new_shared`] additionally checks and reserves the slot in
//! [`crate::crosslimit::SharedLimiterStore`], so an interactive run, a scheduled
//! run, and a second project's host on the same machine and the same key all
//! draw from one counter. Production wiring (`kgraf-host::config::limiters`)
//! always uses `new_shared`; see `crosslimit.rs` for the design and why it fails
//! loud instead of falling back to a process-local limiter when the shared store
//! cannot be opened.
//!
//! **Requests only.** The quota counts calls. It does not count tokens, so a
//! vendor's tokens-per-minute limit is not enforced here.
//!
//! **Not FIFO.** Waiters are not queued: each one re-checks when its projected
//! slot arrives, so under contention a later caller can take a slot before an
//! earlier one, and the earlier one can then fail on its wait limit.
//!
//! **Cancellation.** The ledger row is opened before the wait (the token is the
//! caller's), so a row's `started_ms` includes time spent queued for a slot;
//! [`RouteLimiter::stats`] reports the waiting separately. A call cancelled while
//! it waits never left, so its row is closed as failed, not left unreconciled.

use crate::accounting::CallToken;
use crate::capability::{Capabilities, StreamEvent, Streaming};
use crate::error::ProviderError;
use crate::message::{Request, Response};
use crate::provider::Provider;
use governor::clock::{Clock, DefaultClock};
use governor::{DefaultDirectRateLimiter, Quota, RateLimiter};
use kgraf_core::Protocol;
use serde::{Deserialize, Serialize};
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// Where a rate-limiter notice is said (the
/// two `eprintln!`s this module used to raise were only ever seen by a CLI
/// invocation; the desktop shell has no stderr a user watches). Mirrors
/// `kgraf_secrets::RefusalSink`: until a sink is set, notices queue in
/// [`take_rate_limit_notices`], which the shell polls once per frame exactly
/// as it already polls `kgraf_secrets::take_refusal_notices`; the CLI's own
/// sink prints to stderr, so the CLI's behavior is unchanged.
pub type NoticeSink = Box<dyn Fn(&str) + Send + Sync>;

struct Notices {
    sink: Mutex<Option<NoticeSink>>,
    pending: Mutex<Vec<String>>,
}

static NOTICES: LazyLock<Notices> = LazyLock::new(|| Notices {
    sink: Mutex::new(None),
    pending: Mutex::new(Vec::new()),
});

fn say(notice: String) {
    let sink = NOTICES.sink.lock().unwrap_or_else(|e| e.into_inner());
    match sink.as_ref() {
        Some(say) => say(&notice),
        None => NOTICES
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(notice),
    }
}

/// Set where a rate-limiter notice is said, and say any that waited for it.
pub fn set_notice_sink(sink: NoticeSink) {
    for notice in take_rate_limit_notices() {
        sink(&notice);
    }
    *NOTICES.sink.lock().unwrap_or_else(|e| e.into_inner()) = Some(sink);
}

/// Notices not yet said through a sink, oldest first.
pub fn take_rate_limit_notices() -> Vec<String> {
    std::mem::take(&mut *NOTICES.pending.lock().unwrap_or_else(|e| e.into_inner()))
}

/// A stuck lock on a route's shared rate-limit store: another process's
/// connection has held the SQLite write lock for the caller's entire wait
/// budget. Said once per occurrence (not deduplicated: a lock that is still
/// stuck a minute later is worth saying again, unlike a scrubber refusal that
/// is the same fact every time).
fn notice_lock_contended(route: &str) {
    say(format!(
        "kGraf could not check route `{route}`'s shared rate limit: another process is still \
         holding the shared store's write lock. This is a stuck lock, not the route's real \
         quota; if it persists, another kGraf process may be stalled."
    ));
}

/// A route's configured limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimit {
    /// Sustained calls per minute.
    pub requests_per_minute: u32,
    /// Calls allowed back to back before the sustained rate applies.
    #[serde(default = "one")]
    pub burst: u32,
    /// Longest a call may wait for a slot before it is refused.
    pub max_wait_ms: u64,
}

fn one() -> u32 {
    1
}

/// A limit that cannot be used. Never clamped (fail loud).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RateLimitError {
    #[error("requests_per_minute must be above zero")]
    ZeroRate,
    #[error("burst must be above zero")]
    ZeroBurst,
    #[error("`{0}` is not RPM[/BURST][@MAX_WAIT_MS], for example 60/5@30000")]
    BadSpec(String),
}

impl RateLimit {
    /// Parse `RPM[/BURST][@MAX_WAIT_MS]`. The wait defaults to 60 seconds.
    pub fn parse(spec: &str) -> Result<Self, RateLimitError> {
        let bad = || RateLimitError::BadSpec(spec.to_owned());
        let (rate, wait) = match spec.split_once('@') {
            Some((r, w)) => (r, w.trim().parse::<u64>().map_err(|_| bad())?),
            None => (spec, 60_000),
        };
        let (rpm, burst) = match rate.split_once('/') {
            Some((r, b)) => (r, b.trim().parse::<u32>().map_err(|_| bad())?),
            None => (rate, 1),
        };
        let limit = Self {
            requests_per_minute: rpm.trim().parse().map_err(|_| bad())?,
            burst,
            max_wait_ms: wait,
        };
        limit.quota()?;
        Ok(limit)
    }

    fn quota(&self) -> Result<Quota, RateLimitError> {
        let rpm = NonZeroU32::new(self.requests_per_minute).ok_or(RateLimitError::ZeroRate)?;
        let burst = NonZeroU32::new(self.burst).ok_or(RateLimitError::ZeroBurst)?;
        Ok(Quota::per_minute(rpm).allow_burst(burst))
    }
}

/// What a route's limiter has done so far.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LimiterStats {
    /// Calls let through, with or without waiting.
    pub admitted: u64,
    /// Calls that had to wait for a slot.
    pub waited: u64,
    /// Total time calls spent waiting, in milliseconds.
    pub waited_ms: u64,
    /// Calls refused because the wait would exceed `max_wait_ms`.
    pub refused: u64,
    /// Of `refused`, how many were a shared store's write lock still held by
    /// another process when this caller's own wait budget ran out (decision 0072), rather than the route's real quota being
    /// exhausted. Counted separately so a stuck lock is visible on its own,
    /// not folded into ordinary rate limiting. Always zero on a
    /// [`Backend::Local`] limiter.
    pub lock_timeouts: u64,
}

/// Where a [`RouteLimiter`] keeps its state.
enum Backend {
    /// `governor`, in this process's memory only.
    Local {
        limiter: DefaultDirectRateLimiter,
        clock: DefaultClock,
    },
    /// [`crate::crosslimit::SharedLimiterStore`], shared by every kGraf
    /// process on this machine. `key` is set once, by
    /// [`RouteLimiter::bind_key`], before the first real call: production
    /// wiring (`kgraf-host::config::limited`) does this as soon as a route's
    /// credential is resolved, which always happens before any provider built
    /// on top of this limiter can be called. An unbound key or
    /// a second, different key bound later are both errors
    /// raised at the next check, never silently treated as "unbound" or
    /// silently ignored.
    Shared {
        store: Arc<crate::crosslimit::SharedLimiterStore>,
        key: std::sync::OnceLock<String>,
        conflict: std::sync::OnceLock<String>,
    },
}

/// One route's limiter, shared by every provider built for the route.
pub struct RouteLimiter {
    route: String,
    limit: RateLimit,
    backend: Backend,
    /// When this limiter was constructed: the "opened at" timestamp
    /// [`RouteLimiter::bind_key`] registers the route's configured limit
    /// under (decision 0072: the most recently configured
    /// limit wins). Unused on a [`Backend::Local`] limiter.
    opened_at: SystemTime,
    admitted: AtomicU64,
    waited: AtomicU64,
    waited_ms: AtomicU64,
    refused: AtomicU64,
    lock_timeouts: AtomicU64,
}

/// What one check found, distinguishing "this caller's own wait budget ran
/// out" from "the shared store itself is broken":
/// the two map to different [`ProviderError`] variants.
enum CheckOutcome {
    /// Admitted now.
    Admitted,
    /// Must wait this long before the next attempt.
    Wait(Duration),
    /// The wait for a lock (in-process or SQLite's own) ran out the
    /// caller's budget: this is the shared-store equivalent of a `Wait`
    /// that turned out to exceed `max_wait_ms`, not an infrastructure
    /// failure, and maps to [`ProviderError::RateLimited`].
    BudgetExhausted,
    /// A genuine failure to read or write the shared store (or a
    /// misconfiguration: an unbound or conflicting key), which maps to
    /// [`ProviderError::RateLimitStoreUnavailable`].
    StoreUnavailable(String),
}

impl RouteLimiter {
    /// A process-local limiter. Used by this module's tests and its bench;
    /// production wiring uses [`RouteLimiter::new_shared`] instead, since a
    /// process-local limiter next to another kGraf process's own would let
    /// the two together exceed the route's real limit.
    pub fn new(route: impl Into<String>, limit: RateLimit) -> Result<Arc<Self>, RateLimitError> {
        let quota = limit.quota()?;
        Ok(Arc::new(Self {
            route: route.into(),
            limit,
            backend: Backend::Local {
                limiter: RateLimiter::direct(quota),
                clock: DefaultClock::default(),
            },
            opened_at: SystemTime::now(),
            admitted: AtomicU64::new(0),
            waited: AtomicU64::new(0),
            waited_ms: AtomicU64::new(0),
            refused: AtomicU64::new(0),
            lock_timeouts: AtomicU64::new(0),
        }))
    }

    /// A limiter backed by `store`, shared with every other kGraf process
    /// pointed at the same store file. Call [`RouteLimiter::bind_key`] once
    /// the route's credential is known, before any provider built on this
    /// limiter is called.
    pub fn new_shared(
        route: impl Into<String>,
        limit: RateLimit,
        store: Arc<crate::crosslimit::SharedLimiterStore>,
    ) -> Result<Arc<Self>, RateLimitError> {
        limit.quota()?;
        Ok(Arc::new(Self {
            route: route.into(),
            limit,
            backend: Backend::Shared {
                store,
                key: std::sync::OnceLock::new(),
                conflict: std::sync::OnceLock::new(),
            },
            opened_at: SystemTime::now(),
            admitted: AtomicU64::new(0),
            waited: AtomicU64::new(0),
            waited_ms: AtomicU64::new(0),
            refused: AtomicU64::new(0),
            lock_timeouts: AtomicU64::new(0),
        }))
    }

    /// Set the key id (see [`crate::crosslimit::key_id`]) a shared limiter's
    /// checks are scoped to, and register this limiter's configured `limit`
    /// as this process's for `route`+`key_id`, opened at construction time
    /// (decision 0072: the most recently configured limit
    /// wins, replacing the earlier "whichever is stricter" rule, which could
    /// never raise a limit again once lowered). A no-op on a
    /// [`Backend::Local`] limiter, and a no-op when called again with the
    /// same key (worker rotation rebuilds a provider for the same route
    /// repeatedly; each build binds again, and only the first bind
    /// registers). A second call with a genuinely **different** key id
    /// does not silently keep the first: it is recorded as a
    /// conflict and every following check on this limiter raises loudly
    /// instead of mixing two credentials' shared state under one key.
    ///
    /// If this process's own registration loses to a limit already
    /// registered more recently by another process, this says **one**
    /// notice (see [`say`]), naming both limits, here at bind time rather
    /// than on every check (the previous version's
    /// mismatch warning printed on every call that saw one).
    pub fn bind_key(&self, key_id: &str) {
        if let Backend::Shared {
            store,
            key,
            conflict,
        } = &self.backend
        {
            match key.get() {
                None => {
                    let _ = key.set(key_id.to_owned());
                    match store.register_limit(&self.route, key_id, self.limit, self.opened_at) {
                        Ok(effective) if effective != self.limit => {
                            say(format!(
                                "kGraf: route `{}` was configured here as {}/{}, but a more \
                                 recently opened process already set {}/{}; the more recent \
                                 configuration is the one in force (decision 0072)",
                                self.route,
                                self.limit.requests_per_minute,
                                self.limit.burst,
                                effective.requests_per_minute,
                                effective.burst
                            ));
                        }
                        Ok(_) => {}
                        Err(e) => {
                            say(format!(
                                "kGraf: route `{}` could not register its configured rate limit \
                                 with the shared store: {e}",
                                self.route
                            ));
                        }
                    }
                }
                Some(bound) if bound == key_id => {}
                Some(_) => {
                    let _ = conflict.set(key_id.to_owned());
                }
            }
        }
    }

    pub fn route(&self) -> &str {
        &self.route
    }

    pub fn limit(&self) -> RateLimit {
        self.limit
    }

    pub fn stats(&self) -> LimiterStats {
        LimiterStats {
            admitted: self.admitted.load(Ordering::Relaxed),
            waited: self.waited.load(Ordering::Relaxed),
            waited_ms: self.waited_ms.load(Ordering::Relaxed),
            refused: self.refused.load(Ordering::Relaxed),
            lock_timeouts: self.lock_timeouts.load(Ordering::Relaxed),
        }
    }

    /// One check. `budget` bounds a [`Backend::Shared`] check's wait for the
    /// store's write lock (never longer than the caller has
    /// left to wait overall), in two separately bounded stages: [`crate::crosslimit::SharedLimiterStore::check_bounded`]
    /// waits for the route's connection with `tokio::time::timeout` first,
    /// then recomputes what is left of `budget` for SQLite's own wait,
    /// inside [`tokio::task::spawn_blocking`], so a stalled lock holder
    /// never blocks a `tokio` worker thread synchronously and never costs a
    /// caller more than its own `budget` in total.
    async fn check_once(&self, budget: Duration) -> CheckOutcome {
        match &self.backend {
            Backend::Local { limiter, clock } => match limiter.check() {
                Ok(()) => CheckOutcome::Admitted,
                Err(not_until) => CheckOutcome::Wait(not_until.wait_time_from(clock.now())),
            },
            Backend::Shared {
                store,
                key,
                conflict,
            } => {
                if let Some(other) = conflict.get() {
                    // Raised, never silently ignored.
                    return CheckOutcome::StoreUnavailable(format!(
                        "route `{}`'s shared limiter was bound to two different credentials in \
                         one process (a second key id `{other}` arrived after the first was \
                         already bound); refusing rather than mixing their shared limiter state",
                        self.route
                    ));
                }
                // Raised, never silently treated as an
                // "unbound" key that would happily share a bucket with any
                // other unbound caller.
                let Some(key) = key.get() else {
                    return CheckOutcome::StoreUnavailable(format!(
                        "route `{}`'s shared limiter has no credential bound yet \
                         (RouteLimiter::bind_key was never called before this call)",
                        self.route
                    ));
                };
                match store
                    .check_bounded(&self.route, key, self.limit, SystemTime::now, budget)
                    .await
                {
                    Ok(crate::crosslimit::Admission::Admitted) => CheckOutcome::Admitted,
                    Ok(crate::crosslimit::Admission::Wait(wait)) => CheckOutcome::Wait(wait),
                    // A lock timeout is this caller's
                    // own budget running out, not the store being broken;
                    // `acquire` maps it like an ordinary `Wait` that turned
                    // out to exceed `max_wait_ms`, never as
                    // `RateLimitStoreUnavailable`.
                    Err(crate::crosslimit::CrossLimitError::LockTimeout { .. }) => {
                        CheckOutcome::BudgetExhausted
                    }
                    Err(e) => CheckOutcome::StoreUnavailable(e.to_string()),
                }
            }
        }
    }

    /// Take a slot, waiting up to `max_wait_ms`. Refuses as soon as the projected
    /// wait exceeds it. Returns how long the call waited.
    pub async fn acquire(&self, adapter: &'static str) -> Result<Duration, ProviderError> {
        let start = Instant::now();
        let max = Duration::from_millis(self.limit.max_wait_ms);
        let mut slept = false;
        loop {
            // A shared check's wait for its locks is bounded
            // by what is left of this call's own `max_wait_ms`, not by this
            // module inventing its own ceiling (or SQLite's default, which
            // multiplied out to about 1,000 s before this fix).
            let remaining = max
                .saturating_sub(start.elapsed())
                .max(Duration::from_millis(1));
            match self.check_once(remaining).await {
                CheckOutcome::Admitted => {
                    let waited = if slept {
                        start.elapsed()
                    } else {
                        Duration::ZERO
                    };
                    self.admitted.fetch_add(1, Ordering::Relaxed);
                    if slept {
                        self.waited.fetch_add(1, Ordering::Relaxed);
                        self.waited_ms
                            .fetch_add(waited.as_millis() as u64, Ordering::Relaxed);
                    }
                    return Ok(waited);
                }
                CheckOutcome::Wait(next) => {
                    let projected = start.elapsed() + next;
                    if projected > max {
                        self.refused.fetch_add(1, Ordering::Relaxed);
                        return Err(ProviderError::RateLimited {
                            adapter,
                            route: self.route.clone(),
                            wait_ms: projected.as_millis() as u64,
                            max_wait_ms: self.limit.max_wait_ms,
                            lock_contended: false,
                            ledger_error: None,
                        });
                    }
                    tokio::time::sleep(next).await;
                    slept = true;
                }
                CheckOutcome::BudgetExhausted => {
                    // A stuck lock, not the route's real quota (decision 0072): counted and notified
                    // separately from an ordinary refusal so the two are
                    // never mistaken for one another.
                    self.refused.fetch_add(1, Ordering::Relaxed);
                    self.lock_timeouts.fetch_add(1, Ordering::Relaxed);
                    notice_lock_contended(&self.route);
                    return Err(ProviderError::RateLimited {
                        adapter,
                        route: self.route.clone(),
                        wait_ms: start.elapsed().as_millis() as u64,
                        max_wait_ms: self.limit.max_wait_ms,
                        lock_contended: true,
                        ledger_error: None,
                    });
                }
                CheckOutcome::StoreUnavailable(detail) => {
                    return Err(ProviderError::RateLimitStoreUnavailable {
                        adapter,
                        route: self.route.clone(),
                        detail,
                        ledger_error: None,
                    });
                }
            }
        }
    }
}

/// A provider that records its catalog route on every call's ledger row and
/// otherwise passes calls through: the wrapper for a route with no rate limit.
///
/// The route on the row is what scopes a price to a route (decision 0067,
/// `PriceTable::require_route_scoped`). A call with no route is costed by the
/// bare model entry, so a deployment named like a priced model would take that
/// model's price. Recording the route only when a limiter is configured left
/// exactly the unlimited routes open to that.
pub struct RouteRecorded {
    inner: Arc<dyn Provider>,
    route: String,
}

impl RouteRecorded {
    pub fn new(inner: Arc<dyn Provider>, route: impl Into<String>) -> Self {
        Self {
            inner,
            route: route.into(),
        }
    }

    /// Records the route while the row is in flight. A failure to record is
    /// logged, never fatal, the same rule the limiter follows: a row without a
    /// route is priced as an unscoped call, and the log says which call.
    async fn record(&self, token: &CallToken<'_>) {
        if let Err(e) = token.record_route_async(&self.route).await {
            eprintln!(
                "kgraf: could not record the route on call {}: {e}",
                token.id()
            );
        }
    }
}

#[async_trait::async_trait]
impl Provider for RouteRecorded {
    fn adapter(&self) -> &'static str {
        self.inner.adapter()
    }

    fn protocol(&self) -> Protocol {
        self.inner.protocol()
    }

    fn resends_reasoning(&self) -> bool {
        self.inner.resends_reasoning()
    }

    fn undelivered_output_tokens(&self, response: &Response) -> Option<u64> {
        self.inner.undelivered_output_tokens(response)
    }

    fn forces_tool_choice(&self) -> bool {
        self.inner.forces_tool_choice()
    }

    async fn complete(
        &self,
        request: &Request,
        token: &CallToken,
    ) -> Result<Response, ProviderError> {
        self.record(token).await;
        self.inner.complete(request, token).await
    }

    /// The inner provider's capabilities, with streaming routed through the
    /// recorder too: a stream that bypassed it would leave its row without a
    /// route.
    fn capabilities(&self) -> Capabilities<'_> {
        let inner = self.inner.capabilities();
        Capabilities {
            streaming: inner.streaming.map(|_| self as &dyn Streaming),
            cache_control: inner.cache_control,
            reasoning: inner.reasoning,
        }
    }
}

#[async_trait::async_trait]
impl Streaming for RouteRecorded {
    async fn stream(
        &self,
        request: &Request,
        token: &CallToken,
        sink: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> Result<Response, ProviderError> {
        let Some(inner) = self.inner.capabilities().streaming else {
            return Err(ProviderError::CapabilityNotHonored {
                adapter: self.inner.adapter(),
                capability: "streaming",
            });
        };
        self.record(token).await;
        inner.stream(request, token, sink).await
    }
}

/// A provider whose calls pass its route's limiter first.
pub struct RateLimited {
    inner: Arc<dyn Provider>,
    limiter: Arc<RouteLimiter>,
}

impl RateLimited {
    pub fn new(inner: Arc<dyn Provider>, limiter: Arc<RouteLimiter>) -> Self {
        Self { inner, limiter }
    }

    pub fn limiter(&self) -> &Arc<RouteLimiter> {
        &self.limiter
    }

    async fn admit(&self, token: &CallToken<'_>) -> Result<(), ProviderError> {
        self.limiter.admit(self.inner.adapter(), token).await
    }
}

impl RouteLimiter {
    /// Take a slot for the call `token` opened, or close the token as never
    /// sent and return the refusal. The one admission path: a generative
    /// provider takes it through [`RateLimited`], a route of any other kind
    /// through its caller (decision 0089), so every call on a limited route is
    /// paced alike.
    ///
    /// A cancel while waiting drops this future; the guard then closes the row as
    /// failed, since the request never left.
    pub async fn admit(
        &self,
        adapter: &'static str,
        token: &CallToken<'_>,
    ) -> Result<(), ProviderError> {
        // Decision 0067: the catalog
        // route id, distinct from `adapter` (the wire adapter
        // constant, which several routes can share), recorded while the row
        // is still in flight, before the rate-limit check, so a refused
        // call still carries its route (harmless: `route_round_stats` reads
        // only settled, counted rows, so a refused call never skews it, and
        // leaves out the embedding, rerank and decision calls admitted here,
        // which are not rounds). A failure to record it
        // is logged, never fatal: a route this crate cannot yet resolve is
        // reported as "unmeasured" downstream, not as a broken call.
        if let Err(e) = token.record_route_async(self.route()).await {
            eprintln!(
                "kgraf: could not record the route on call {}: {e}",
                token.id()
            );
        }
        let mut guard = FailIfDropped { token, armed: true };
        let acquired = self.acquire(adapter).await;
        guard.armed = false;
        if let Err(mut e) = acquired {
            if let Err(ledger) = token.close_on_async(&e, None).await {
                // Kept on every variant that carries a
                // `ledger_error` field, not only `RateLimited`.
                match &mut e {
                    ProviderError::RateLimited { ledger_error, .. }
                    | ProviderError::RateLimitStoreUnavailable { ledger_error, .. } => {
                        *ledger_error = Some(ledger.to_string());
                    }
                    _ => {}
                }
            }
            return Err(e);
        }
        Ok(())
    }
}

/// Closes a call's row as failed if the wait for a slot is abandoned.
struct FailIfDropped<'a, 'l> {
    token: &'a CallToken<'l>,
    armed: bool,
}

impl Drop for FailIfDropped<'_, '_> {
    fn drop(&mut self) {
        if self.armed {
            // Submitted eagerly; the ack is not awaited because `drop` cannot
            // wait. A failed write leaves the row in flight, which
            // `Ledger::unreconciled` reports.
            drop(self.token.fail_async());
        }
    }
}

#[async_trait::async_trait]
impl Provider for RateLimited {
    fn adapter(&self) -> &'static str {
        self.inner.adapter()
    }

    fn protocol(&self) -> Protocol {
        self.inner.protocol()
    }

    fn resends_reasoning(&self) -> bool {
        self.inner.resends_reasoning()
    }

    fn undelivered_output_tokens(&self, response: &Response) -> Option<u64> {
        self.inner.undelivered_output_tokens(response)
    }

    fn forces_tool_choice(&self) -> bool {
        self.inner.forces_tool_choice()
    }

    async fn complete(
        &self,
        request: &Request,
        token: &CallToken,
    ) -> Result<Response, ProviderError> {
        self.admit(token).await?;
        self.inner.complete(request, token).await
    }

    /// The inner provider's capabilities, with streaming routed through the
    /// limiter too: a stream that bypassed it would be an unlimited call path.
    fn capabilities(&self) -> Capabilities<'_> {
        let inner = self.inner.capabilities();
        Capabilities {
            streaming: inner.streaming.map(|_| self as &dyn Streaming),
            cache_control: inner.cache_control,
            reasoning: inner.reasoning,
        }
    }
}

#[async_trait::async_trait]
impl Streaming for RateLimited {
    async fn stream(
        &self,
        request: &Request,
        token: &CallToken,
        sink: &mut (dyn FnMut(StreamEvent) + Send),
    ) -> Result<Response, ProviderError> {
        let Some(inner) = self.inner.capabilities().streaming else {
            return Err(ProviderError::CapabilityNotHonored {
                adapter: self.inner.adapter(),
                capability: "streaming",
            });
        };
        self.admit(token).await?;
        inner.stream(request, token, sink).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounting::{Ledger, Settlement};
    use crate::message::{Content, StopReason};
    use kgraf_core::TokenUsage;

    /// A provider that answers at once and settles its token, as adapters do.
    struct Instant0;

    #[async_trait::async_trait]
    impl Provider for Instant0 {
        fn adapter(&self) -> &'static str {
            "instant"
        }
        fn protocol(&self) -> Protocol {
            Protocol::Additive
        }
        async fn complete(
            &self,
            request: &Request,
            token: &CallToken,
        ) -> Result<Response, ProviderError> {
            let usage = TokenUsage {
                uncached_input: 10,
                text_output: 1,
                ..TokenUsage::default()
            };
            token.settle(&usage, None)?;
            Ok(Response {
                finish_reason: None,
                model: request.model.clone(),
                content: vec![Content::text("ok")],
                stop_reason: StopReason::EndTurn,
                usage,
                request_id: None,
            })
        }
    }

    /// A provider that reports a fixed undelivered count, to see the wrappers
    /// pass the question through.
    struct Counts;

    #[async_trait::async_trait]
    impl Provider for Counts {
        fn adapter(&self) -> &'static str {
            "counts"
        }
        fn protocol(&self) -> Protocol {
            Protocol::Additive
        }
        fn undelivered_output_tokens(&self, _response: &Response) -> Option<u64> {
            Some(900)
        }
        fn forces_tool_choice(&self) -> bool {
            true
        }
        async fn complete(
            &self,
            request: &Request,
            token: &CallToken,
        ) -> Result<Response, ProviderError> {
            Instant0.complete(request, token).await
        }
    }

    /// A wrapper that kept the default would hide a reply the server cut down.
    #[test]
    fn both_wrappers_pass_the_undelivered_output_question_to_the_adapter() {
        let response = Response {
            finish_reason: None,
            model: "m".into(),
            content: Vec::new(),
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
            request_id: None,
        };
        let limiter = RouteLimiter::new("r", RateLimit::parse("60/5").expect("spec")).expect("lim");
        let limited = RateLimited::new(Arc::new(Counts), limiter);
        assert_eq!(limited.undelivered_output_tokens(&response), Some(900));
        let recorded = RouteRecorded::new(Arc::new(Counts), "r");
        assert_eq!(recorded.undelivered_output_tokens(&response), Some(900));
        assert_eq!(Instant0.undelivered_output_tokens(&response), None);
    }

    /// A wrapper that kept the default would hide the adapter's wire form for
    /// a forced tool choice, and the worker would never send one.
    #[test]
    fn both_wrappers_pass_the_forced_tool_choice_question_to_the_adapter() {
        let limiter = RouteLimiter::new("r", RateLimit::parse("60/5").expect("spec")).expect("lim");
        assert!(RateLimited::new(Arc::new(Counts), limiter).forces_tool_choice());
        assert!(RouteRecorded::new(Arc::new(Counts), "r").forces_tool_choice());
        assert!(!Instant0.forces_tool_choice());
    }

    fn limited(spec: &str) -> RateLimited {
        let limiter = RouteLimiter::new("r", RateLimit::parse(spec).expect("spec")).expect("lim");
        RateLimited::new(Arc::new(Instant0), limiter)
    }

    async fn call(p: &RateLimited, ledger: &Ledger) -> Result<Response, ProviderError> {
        let token = ledger
            .begin(p.adapter(), p.protocol(), "m", "agent", "task")
            .expect("begin");
        p.complete(&Request::new("m", "hi", 10), &token).await
    }

    #[test]
    fn a_spec_parses_and_a_zero_rate_is_refused_not_clamped() {
        assert_eq!(
            RateLimit::parse("60/5@30000").expect("full"),
            RateLimit {
                requests_per_minute: 60,
                burst: 5,
                max_wait_ms: 30_000
            }
        );
        assert_eq!(RateLimit::parse("10").expect("bare").max_wait_ms, 60_000);
        assert_eq!(RateLimit::parse("0"), Err(RateLimitError::ZeroRate));
        assert_eq!(RateLimit::parse("5/0"), Err(RateLimitError::ZeroBurst));
        assert!(matches!(
            RateLimit::parse("fast"),
            Err(RateLimitError::BadSpec(_))
        ));
    }

    #[tokio::test]
    async fn a_call_over_the_allowed_wait_is_refused_loudly_and_attributed() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        // One call a minute, no wait allowed.
        let p = limited("1/1@50");
        call(&p, &ledger).await.expect("first call has the slot");
        let started = Instant::now();
        let err = call(&p, &ledger).await.expect_err("second is refused");
        assert!(
            started.elapsed() < Duration::from_millis(40),
            "refused at once, not after sleeping out the wait"
        );
        match err {
            ProviderError::RateLimited {
                route,
                wait_ms,
                max_wait_ms,
                ..
            } => {
                assert_eq!(route, "r");
                assert!(wait_ms > 50_000, "about a minute away, got {wait_ms}");
                assert_eq!(max_wait_ms, 50);
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
        // The refusal is in the ledger as a failed (never sent) call.
        let calls = ledger.calls().expect("calls");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].settlement, Settlement::Counted);
        assert_eq!(calls[1].settlement, Settlement::Failed);
        assert_eq!(
            p.limiter().stats(),
            LimiterStats {
                admitted: 1,
                refused: 1,
                ..LimiterStats::default()
            }
        );
    }

    #[tokio::test]
    async fn a_call_inside_the_allowed_wait_waits_for_its_slot() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        // 1,200 a minute is one every 50 ms; a 2 s wait is plenty.
        let p = limited("1200/1@2000");
        call(&p, &ledger).await.expect("first");
        let started = Instant::now();
        call(&p, &ledger).await.expect("second waits, then runs");
        let waited = started.elapsed();
        assert!(waited >= Duration::from_millis(30), "waited {waited:?}");
        let stats = p.limiter().stats();
        assert_eq!((stats.admitted, stats.waited, stats.refused), (2, 1, 0));
    }

    /// A provider that streams, to check the stream path is limited too.
    struct Streams;

    #[async_trait::async_trait]
    impl Provider for Streams {
        fn adapter(&self) -> &'static str {
            "streams"
        }
        fn protocol(&self) -> Protocol {
            Protocol::Additive
        }
        async fn complete(
            &self,
            request: &Request,
            token: &CallToken,
        ) -> Result<Response, ProviderError> {
            Instant0.complete(request, token).await
        }
        fn capabilities(&self) -> Capabilities<'_> {
            Capabilities::none().with_streaming(self)
        }
    }

    #[async_trait::async_trait]
    impl Streaming for Streams {
        async fn stream(
            &self,
            request: &Request,
            token: &CallToken,
            _sink: &mut (dyn FnMut(StreamEvent) + Send),
        ) -> Result<Response, ProviderError> {
            Instant0.complete(request, token).await
        }
    }

    #[tokio::test]
    async fn the_stream_path_is_limited_too_and_absent_streaming_stays_absent() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        let limiter =
            RouteLimiter::new("r", RateLimit::parse("1/1@10").expect("spec")).expect("lim");
        let p = RateLimited::new(Arc::new(Streams), limiter);
        let streaming = p.capabilities().streaming.expect("streaming kept");
        let request = Request::new("m", "hi", 10);
        let mut sink = |_e: StreamEvent| {};
        let t1 = ledger
            .begin("streams", Protocol::Additive, "m", "a", "t")
            .expect("b");
        streaming
            .stream(&request, &t1, &mut sink)
            .await
            .expect("first");
        let t2 = ledger
            .begin("streams", Protocol::Additive, "m", "a", "t")
            .expect("b");
        assert!(matches!(
            streaming.stream(&request, &t2, &mut sink).await,
            Err(ProviderError::RateLimited { .. })
        ));
        assert!(limited("1").capabilities().streaming.is_none());
    }

    #[tokio::test]
    async fn a_call_cancelled_while_it_waits_is_failed_not_left_unreconciled() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        // One call a minute, a two-minute wait allowed: the second call waits.
        let p = limited("1/1@120000");
        call(&p, &ledger).await.expect("first");
        let token = ledger
            .begin(p.adapter(), p.protocol(), "m", "agent", "task")
            .expect("begin");
        let request = Request::new("m", "hi", 10);
        let waited =
            tokio::time::timeout(Duration::from_millis(50), p.complete(&request, &token)).await;
        assert!(waited.is_err(), "still waiting when cancelled");
        drop(token);
        let calls = ledger.calls().expect("calls");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].settlement, Settlement::Failed);
        assert!(ledger.unreconciled().expect("unreconciled").is_empty());
    }

    #[tokio::test]
    async fn providers_built_for_one_route_share_its_limit() {
        let ledger = Ledger::open_in_memory().expect("ledger");
        let limiter =
            RouteLimiter::new("r", RateLimit::parse("1/1@10").expect("spec")).expect("lim");
        let planner = RateLimited::new(Arc::new(Instant0), limiter.clone());
        let worker = RateLimited::new(Arc::new(Instant0), limiter);
        call(&planner, &ledger)
            .await
            .expect("planner takes the slot");
        assert!(matches!(
            call(&worker, &ledger).await,
            Err(ProviderError::RateLimited { .. })
        ));
    }

    /// A shared limiter admits, then refuses, exactly as a local one does: the
    /// backend swap changes where the state lives, not the policy a caller sees.
    #[tokio::test]
    async fn a_shared_limiter_refuses_past_its_limit_like_a_local_one() {
        let dir = tempfile::tempdir().expect("dir");
        let store = Arc::new(
            crate::crosslimit::SharedLimiterStore::open(&dir.path().join("shared.sqlite3"))
                .expect("open"),
        );
        let ledger = Ledger::open_in_memory().expect("ledger");
        let limiter =
            RouteLimiter::new_shared("r", RateLimit::parse("1/1@10").expect("spec"), store)
                .expect("lim");
        limiter.bind_key("k");
        let p = RateLimited::new(Arc::new(Instant0), limiter);
        call(&p, &ledger).await.expect("first has the slot");
        assert!(matches!(
            call(&p, &ledger).await,
            Err(ProviderError::RateLimited { .. })
        ));
    }

    /// Two [`RouteLimiter`]s built from the same shared store and the same
    /// route and key act as one limit even though they are two separate
    /// in-process objects: exactly the case a second process's own
    /// `RouteLimiter` reaches (requirement: several processes on one key).
    #[tokio::test]
    async fn two_route_limiters_on_the_same_shared_store_share_one_limit() {
        let dir = tempfile::tempdir().expect("dir");
        let store = Arc::new(
            crate::crosslimit::SharedLimiterStore::open(&dir.path().join("shared.sqlite3"))
                .expect("open"),
        );
        let ledger = Ledger::open_in_memory().expect("ledger");
        let spec = RateLimit::parse("1/1@10").expect("spec");
        let a = RouteLimiter::new_shared("r", spec, store.clone()).expect("a");
        a.bind_key("k");
        let b = RouteLimiter::new_shared("r", spec, store).expect("b");
        b.bind_key("k");
        let pa = RateLimited::new(Arc::new(Instant0), a);
        let pb = RateLimited::new(Arc::new(Instant0), b);
        call(&pa, &ledger).await.expect("first process takes it");
        assert!(matches!(
            call(&pb, &ledger).await,
            Err(ProviderError::RateLimited { .. })
        ));
    }

    /// A shared store that cannot be reached fails loud with a distinct error,
    /// never silently admitting as an unlimited or process-local call would.
    #[tokio::test]
    async fn a_broken_shared_store_fails_loud_rather_than_admitting() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("shared.sqlite3");
        let store = Arc::new(crate::crosslimit::SharedLimiterStore::open(&path).expect("open"));
        let ledger = Ledger::open_in_memory().expect("ledger");
        let limiter = RouteLimiter::new_shared("r", RateLimit::parse("60").expect("spec"), store)
            .expect("lim");
        limiter.bind_key("k");
        // Simulate the store breaking out from under the open connection (a
        // partial migration from another process, a deleted table): the next
        // check must raise, not silently admit.
        rusqlite::Connection::open(&path)
            .expect("second connection")
            .execute_batch("DROP TABLE gcra;")
            .expect("drop the table from under the first connection");
        let p = RateLimited::new(Arc::new(Instant0), limiter);
        match call(&p, &ledger).await {
            Err(ProviderError::RateLimitStoreUnavailable { .. }) => {}
            other => panic!("expected RateLimitStoreUnavailable, got {other:?}"),
        }
    }

    /// A shared limiter never bound raises loud, rather than
    /// silently sharing an "unbound" bucket with every other unbound caller.
    #[tokio::test]
    async fn an_unbound_shared_key_raises_rather_than_falling_back_silently() {
        let dir = tempfile::tempdir().expect("dir");
        let store = Arc::new(
            crate::crosslimit::SharedLimiterStore::open(&dir.path().join("shared.sqlite3"))
                .expect("open"),
        );
        let limiter = RouteLimiter::new_shared("r", RateLimit::parse("60").expect("spec"), store)
            .expect("lim");
        // No `bind_key` call.
        let ledger = Ledger::open_in_memory().expect("ledger");
        let p = RateLimited::new(Arc::new(Instant0), limiter);
        match call(&p, &ledger).await {
            Err(ProviderError::RateLimitStoreUnavailable { detail, .. }) => {
                assert!(detail.contains("no credential bound"), "{detail}");
            }
            other => panic!("expected RateLimitStoreUnavailable, got {other:?}"),
        }
    }

    /// Binding a second, different key on one limiter raises
    /// loud on the next check, rather than silently keeping (or silently
    /// replacing) the first.
    #[tokio::test]
    async fn binding_a_second_different_key_raises_rather_than_being_ignored() {
        let dir = tempfile::tempdir().expect("dir");
        let store = Arc::new(
            crate::crosslimit::SharedLimiterStore::open(&dir.path().join("shared.sqlite3"))
                .expect("open"),
        );
        let limiter = RouteLimiter::new_shared("r", RateLimit::parse("60").expect("spec"), store)
            .expect("lim");
        limiter.bind_key("first");
        limiter.bind_key("first"); // Rebinding the same key is a no-op.
        limiter.bind_key("second"); // A genuinely different key is a conflict.
        let ledger = Ledger::open_in_memory().expect("ledger");
        let p = RateLimited::new(Arc::new(Instant0), limiter);
        match call(&p, &ledger).await {
            Err(ProviderError::RateLimitStoreUnavailable { detail, .. }) => {
                assert!(detail.contains("two different credentials"), "{detail}");
            }
            other => panic!("expected RateLimitStoreUnavailable, got {other:?}"),
        }
    }

    /// The previous design took the in-process
    /// `std::sync::Mutex<Connection>` unconditionally, outside any budget, so
    /// eight callers on one route stalled behind a ninth (a stuck lock
    /// holder) stacked up to about the *sum* of their budgets (roughly 8.5 s
    /// for eight 300 ms-budget callers), each parked on its own
    /// blocking-pool thread the whole time. With the connection acquired
    /// through `tokio::time::timeout` before any blocking thread is spent,
    /// every caller returns within about its own budget instead.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn eight_callers_against_a_stalled_holder_all_return_within_about_their_budget() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("shared.sqlite3");
        let store = Arc::new(crate::crosslimit::SharedLimiterStore::open(&path).expect("open"));
        let limiter =
            RouteLimiter::new_shared("r", RateLimit::parse("60@300").expect("spec"), store)
                .expect("lim");
        limiter.bind_key("k");

        // A completely separate connection holds the write lock for the
        // whole test: every caller must find it stalled the entire time.
        let holder = rusqlite::Connection::open(&path).expect("holder connection");
        holder
            .busy_timeout(Duration::from_secs(10))
            .expect("busy_timeout");
        holder
            .execute_batch("BEGIN IMMEDIATE")
            .expect("hold the write lock");

        let start = Instant::now();
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let limiter = limiter.clone();
                tokio::spawn(async move { limiter.acquire("adapter").await })
            })
            .collect();
        for handle in handles {
            let result = handle.await.expect("task did not panic");
            assert!(
                matches!(result, Err(ProviderError::RateLimited { .. })),
                "{result:?}"
            );
        }
        let elapsed = start.elapsed();
        drop(holder);
        assert!(
            elapsed < Duration::from_secs(2),
            "8 callers took {elapsed:?} total against a 300 ms budget; expected close to one \
             budget period, not a multiple of it (each caller must not queue behind every \
             other caller's own full wait)"
        );
    }

    /// A stuck lock (another process
    /// holding the shared store's write lock for the whole budget) is
    /// distinguishable from a genuine rate-limit refusal, both in the error
    /// (`lock_contended`) and in `LimiterStats` (`lock_timeouts`, counted
    /// apart from `refused`'s ordinary-refusal siblings). Without the fix,
    /// both outcomes were the same `RateLimited` with no way to tell a stuck
    /// lock from real load.
    #[tokio::test]
    async fn a_stuck_lock_is_distinguished_from_a_real_rate_limit_refusal() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("shared.sqlite3");
        let store = Arc::new(crate::crosslimit::SharedLimiterStore::open(&path).expect("open"));
        let limiter =
            RouteLimiter::new_shared("r", RateLimit::parse("60@100").expect("spec"), store)
                .expect("lim");
        limiter.bind_key("k");

        // A separate connection holds the write lock for the whole budget:
        // this caller can never even check the quota.
        let holder = rusqlite::Connection::open(&path).expect("holder connection");
        holder
            .busy_timeout(Duration::from_secs(10))
            .expect("busy_timeout");
        holder
            .execute_batch("BEGIN IMMEDIATE")
            .expect("hold the write lock");

        match limiter.acquire("adapter").await {
            Err(ProviderError::RateLimited {
                lock_contended: true,
                ..
            }) => {}
            other => panic!("expected a lock-contended RateLimited, got {other:?}"),
        }
        drop(holder);
        let stats = limiter.stats();
        assert_eq!(
            (stats.refused, stats.lock_timeouts),
            (1, 1),
            "a stuck lock counts as a refusal and, separately, as a lock timeout"
        );

        // A genuine rate-limit refusal (the quota itself, no lock in the
        // way) must not be counted as a lock timeout.
        let dir2 = tempfile::tempdir().expect("dir");
        let store2 = Arc::new(
            crate::crosslimit::SharedLimiterStore::open(&dir2.path().join("shared.sqlite3"))
                .expect("open"),
        );
        let real_limit =
            RouteLimiter::new_shared("r", RateLimit::parse("1/1@10").expect("spec"), store2)
                .expect("lim");
        real_limit.bind_key("k");
        real_limit
            .acquire("adapter")
            .await
            .expect("first has the slot");
        match real_limit.acquire("adapter").await {
            Err(ProviderError::RateLimited {
                lock_contended: false,
                ..
            }) => {}
            other => panic!("expected an ordinary RateLimited, got {other:?}"),
        }
        let stats2 = real_limit.stats();
        assert_eq!(
            (stats2.refused, stats2.lock_timeouts),
            (1, 0),
            "an ordinary quota refusal must not be counted as a lock timeout"
        );
    }

    /// A ledger-close failure is kept on
    /// `RateLimitStoreUnavailable`, not only on `RateLimited`. Uses the
    /// ledger's own writer-fault injection (`test-support`), the same
    /// mechanism `accounting.rs`'s own tests use to fail a specific
    /// operation mid-batch.
    #[tokio::test]
    async fn a_ledger_close_failure_is_kept_on_rate_limit_store_unavailable() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("shared.sqlite3");
        let store = Arc::new(crate::crosslimit::SharedLimiterStore::open(&path).expect("open"));
        let limiter = RouteLimiter::new_shared("r", RateLimit::parse("60").expect("spec"), store)
            .expect("lim");
        limiter.bind_key("k");
        // Break the store out from under the limiter so `acquire` fails with
        // `RateLimitStoreUnavailable` (same technique as
        // `a_broken_shared_store_fails_loud_rather_than_admitting`).
        rusqlite::Connection::open(&path)
            .expect("second connection")
            .execute_batch("DROP TABLE gcra;")
            .expect("drop the table from under the limiter's connection");

        // Op 1: `begin` (acknowledged on its own, before the provider call).
        // Op 2: `admit`'s own route record (decision 0067), submitted before the rate-limit check.
        // Op 3: the close this failure path submits when `acquire` fails;
        // faulting it makes `close_on_async` itself return an error.
        let ledger = crate::accounting::Ledger::open_with_faults(
            &dir.path().join("ledger.db"),
            crate::accounting::WriterFaults {
                panic_after_op: Some(3),
                abort_after_op: None,
            },
        )
        .expect("ledger");
        let p = RateLimited::new(Arc::new(Instant0), limiter);
        let token = ledger
            .begin(p.adapter(), p.protocol(), "m", "agent", "task")
            .expect("begin (op 1, acknowledged before the fault)");
        match p.complete(&Request::new("m", "hi", 10), &token).await {
            Err(ProviderError::RateLimitStoreUnavailable {
                ledger_error: Some(_),
                ..
            }) => {}
            other => {
                panic!("expected RateLimitStoreUnavailable with a kept ledger_error, got {other:?}")
            }
        }
    }
}

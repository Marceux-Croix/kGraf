//! How fast a route is, measured from the ledger (decision 0100, section 1).
//!
//! [`RouteSpeed`] reads a [`LatencyHistory`]'s route calls and reports, per
//! route and output-size class: latency p50 and p90, output tokens per
//! second, time to first token, the rate-limit wait, and the 429 answers
//! with their retry-after. The same figures are split by whether other calls
//! on the route ran at the same time (overlapped) or not (lone).
//!
//! On a hosted route the rate-limit wait and a 429's retry-after are part of
//! model time, so the estimate keeps them apart from generation: a throttled
//! route is told apart from a slow one. Time to first token is reported only
//! from calls that streamed; a call that did not stream has none, and the
//! estimate says so instead of reading zero.
//!
//! With too few calls in a class the figures are the defaults of decision
//! 0100 for the route's class (hosted or local), labeled
//! [`SpeedSource::Default`]. A default is never shown as measured.

use crate::answer_wait::{LatencyHistory, RouteCall, SizeClass};
use crate::sizing::percentile;
use std::collections::BTreeMap;

/// Fewer calls than this in a class, and the defaults stand in.
pub const MIN_CALLS: usize = 5;

/// Whether a route is a hosted service or a server on the user's machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteClass {
    Hosted,
    Local,
}

impl RouteClass {
    pub fn of(local: bool) -> Self {
        if local { Self::Local } else { Self::Hosted }
    }
}

/// Where a set of figures came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpeedSource {
    /// Measured from this many ledger calls.
    Measured { calls: usize },
    /// The route class's first-run default; `calls` measured calls exist,
    /// too few to decide.
    Default { calls: usize },
}

impl SpeedSource {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Measured { .. } => "measured",
            Self::Default { .. } => "default",
        }
    }
}

/// First-run figures for a route class and size class (decision 0100,
/// section 1). Small calls use the triage row; large calls use the delta
/// row's seconds per 1,000 output tokens as a rate.
fn default_figures(class: RouteClass, size: SizeClass) -> (u64, u64, f64) {
    match (class, size) {
        (RouteClass::Hosted, SizeClass::Small) => (3_000, 8_000, 50.0),
        (RouteClass::Hosted, SizeClass::Large) => (20_000, 40_000, 50.0),
        (RouteClass::Local, SizeClass::Small) => (8_000, 30_000, 1_000.0 / 15.0),
        (RouteClass::Local, SizeClass::Large) => (15_000, 45_000, 1_000.0 / 15.0),
    }
}

/// How many of the newest finished route calls one read takes.
pub const WINDOW: usize = 200;

/// The route-keyed read: the newest [`WINDOW`] finished calls on `route`
/// (every route when `None`) in a [`LatencyHistory`]. One ledger query.
pub fn read_route_calls(
    ledger: &kgraf_provider::Ledger,
    route: Option<&str>,
) -> Result<LatencyHistory, String> {
    let rows = ledger
        .recent_route_calls(route, WINDOW)
        .map_err(|e| e.to_string())?;
    Ok(LatencyHistory {
        route_calls: rows.iter().map(RouteCall::from_row).collect(),
        ..LatencyHistory::default()
    })
}

/// The figures of one group of calls.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Figures {
    pub calls: usize,
    pub p50_ms: u64,
    pub p90_ms: u64,
    /// Output tokens per second of generation (wall time less the rate-limit
    /// wait), the median over calls that produced output.
    pub tokens_per_s: Option<f64>,
}

/// One route's speed for one size class.
#[derive(Clone, Debug, PartialEq)]
pub struct RouteSpeed {
    pub route: String,
    pub class: RouteClass,
    pub size: SizeClass,
    pub source: SpeedSource,
    /// Latency and rate over every counted call of the class.
    pub overall: Figures,
    /// The same, over calls that ran while another call on the route ran.
    /// `None` below [`MIN_CALLS`].
    pub overlapped: Option<Figures>,
    /// The same, over calls that ran alone. `None` below [`MIN_CALLS`].
    pub lone: Option<Figures>,
    /// Median time to first token over the calls that streamed; `None` when
    /// none did.
    pub ttft_p50_ms: Option<u64>,
    /// Calls with a recorded first-token time.
    pub ttft_calls: usize,
    /// Median and 90th percentile of the rate-limit wait over the calls a
    /// limiter admitted; `None` when none was.
    pub queued_p50_ms: Option<u64>,
    pub queued_p90_ms: Option<u64>,
    /// 429 answers on the route (all size classes: a refused attempt has no
    /// output to class it by), and the longest retry-after any named.
    pub throttled: usize,
    pub max_retry_after_ms: Option<u64>,
}

fn figures(calls: &[&RouteCall]) -> Option<Figures> {
    if calls.len() < MIN_CALLS {
        return None;
    }
    let latencies: Vec<u64> = calls.iter().map(|c| c.latency_ms).collect();
    let rates: Vec<u64> = calls
        .iter()
        .filter_map(|c| c.tokens_per_s())
        .map(|r| r.round() as u64)
        .collect();
    Some(Figures {
        calls: calls.len(),
        p50_ms: percentile(latencies.clone(), 50)?,
        p90_ms: percentile(latencies, 90)?,
        tokens_per_s: percentile(rates, 50).map(|r| r as f64),
    })
}

impl RouteSpeed {
    /// The speed of `route` for `size` from `history`'s route calls. `class`
    /// is the route's class, for the defaults.
    pub fn from_history(
        history: &LatencyHistory,
        route: &str,
        class: RouteClass,
        size: SizeClass,
    ) -> Self {
        let on_route: Vec<&RouteCall> = history
            .route_calls
            .iter()
            .filter(|c| c.route == route)
            .collect();
        let throttles: Vec<_> = on_route.iter().filter_map(|c| c.throttled).collect();
        let counted: Vec<&RouteCall> = on_route
            .iter()
            .copied()
            .filter(|c| c.throttled.is_none() && c.size_class() == size)
            .collect();
        let measured = figures(&counted);
        let (source, overall) = match measured {
            Some(f) => (SpeedSource::Measured { calls: f.calls }, f),
            None => {
                let (p50, p90, rate) = default_figures(class, size);
                (
                    SpeedSource::Default {
                        calls: counted.len(),
                    },
                    Figures {
                        calls: counted.len(),
                        p50_ms: p50,
                        p90_ms: p90,
                        tokens_per_s: Some(rate),
                    },
                )
            }
        };
        let (over, lone): (Vec<&RouteCall>, Vec<&RouteCall>) =
            counted.iter().copied().partition(|c| c.overlapped);
        let ttft: Vec<u64> = counted.iter().filter_map(|c| c.ttft_ms).collect();
        let queued: Vec<u64> = counted.iter().filter_map(|c| c.queued_ms).collect();
        Self {
            route: route.to_owned(),
            class,
            size,
            source,
            overall,
            overlapped: figures(&over),
            lone: figures(&lone),
            ttft_calls: ttft.len(),
            ttft_p50_ms: percentile(ttft, 50),
            queued_p50_ms: percentile(queued.clone(), 50),
            queued_p90_ms: percentile(queued, 90),
            throttled: throttles.len(),
            max_retry_after_ms: throttles.iter().filter_map(|t| t.retry_after_ms).max(),
        }
    }

    /// The estimate for every route in `history`, both size classes.
    /// `class_of` gives a route's class.
    pub fn all(history: &LatencyHistory, class_of: impl Fn(&str) -> RouteClass) -> Vec<RouteSpeed> {
        let routes: BTreeMap<&str, ()> = history
            .route_calls
            .iter()
            .map(|c| (c.route.as_str(), ()))
            .collect();
        routes
            .keys()
            .flat_map(|r| {
                [SizeClass::Small, SizeClass::Large]
                    .into_iter()
                    .map(|s| Self::from_history(history, r, class_of(r), s))
            })
            .collect()
    }

    /// One short line: the figures and where they came from, with the
    /// rate-limit wait and the throttling named apart from generation.
    pub fn line(&self) -> String {
        let size = match self.size {
            SizeClass::Small => "small",
            SizeClass::Large => "large",
        };
        let mut out = format!(
            "{} {size}: p50 {} ms, p90 {} ms ({})",
            self.route,
            self.overall.p50_ms,
            self.overall.p90_ms,
            self.source.label()
        );
        if let Some(r) = self.overall.tokens_per_s {
            out.push_str(&format!(", {r:.0} tokens/s"));
        }
        match self.ttft_p50_ms {
            Some(t) => out.push_str(&format!(", first token p50 {t} ms")),
            None => out.push_str(", first token absent"),
        }
        if let Some(q) = self.queued_p50_ms {
            out.push_str(&format!(", rate-limit wait p50 {q} ms"));
        }
        if self.throttled > 0 {
            out.push_str(&format!(", {} x 429", self.throttled));
            if let Some(r) = self.max_retry_after_ms {
                out.push_str(&format!(" (retry-after up to {r} ms)"));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::answer_wait::Throttle;

    fn call(route: &str, latency_ms: u64, tokens: u64) -> RouteCall {
        RouteCall {
            route: route.into(),
            latency_ms,
            output_tokens: tokens,
            ttft_ms: None,
            queued_ms: None,
            throttled: None,
            overlapped: false,
        }
    }

    fn history(calls: Vec<RouteCall>) -> LatencyHistory {
        LatencyHistory {
            route_calls: calls,
            ..LatencyHistory::default()
        }
    }

    #[test]
    fn too_little_history_gives_the_class_default_labeled_default() {
        let h = history(vec![call("r", 900, 100); 2]);
        let hosted = RouteSpeed::from_history(&h, "r", RouteClass::Hosted, SizeClass::Small);
        assert_eq!(hosted.source, SpeedSource::Default { calls: 2 });
        assert_eq!(
            (hosted.overall.p50_ms, hosted.overall.p90_ms),
            (3_000, 8_000)
        );
        assert!(hosted.line().contains("(default)"), "{}", hosted.line());
        let local = RouteSpeed::from_history(&h, "r", RouteClass::Local, SizeClass::Small);
        assert_eq!(
            (local.overall.p50_ms, local.overall.p90_ms),
            (8_000, 30_000)
        );
        let none = RouteSpeed::from_history(
            &LatencyHistory::default(),
            "r",
            RouteClass::Hosted,
            SizeClass::Large,
        );
        assert_eq!(none.source, SpeedSource::Default { calls: 0 });
    }

    #[test]
    fn enough_calls_are_measured_and_split_by_size_and_overlap() {
        let mut calls = Vec::new();
        for ms in [1_000, 1_100, 1_200, 1_300, 1_400] {
            calls.push(call("r", ms, 100));
        }
        for ms in [5_000, 6_000, 7_000, 8_000, 9_000] {
            let mut c = call("r", ms, 100);
            c.overlapped = true;
            calls.push(c);
        }
        calls.push(call("r", 30_000, 900)); // large: not in the small class
        calls.push(call("other", 1, 1)); // another route
        let s =
            RouteSpeed::from_history(&history(calls), "r", RouteClass::Hosted, SizeClass::Small);
        assert_eq!(s.source, SpeedSource::Measured { calls: 10 });
        assert_eq!((s.overall.p50_ms, s.overall.p90_ms), (1_400, 8_000));
        let lone = s.lone.expect("lone");
        let over = s.overlapped.expect("overlapped");
        assert_eq!(lone.p50_ms, 1_200);
        assert_eq!(over.p50_ms, 7_000);
        assert!(over.p50_ms > lone.p50_ms);
        // 100 tokens in 1.2 s of generation.
        assert_eq!(lone.tokens_per_s, Some(83.0));
    }

    #[test]
    fn a_call_that_did_not_stream_has_no_first_token_time_not_zero() {
        let mut calls = vec![call("r", 1_000, 100); 5];
        let s = RouteSpeed::from_history(
            &history(calls.clone()),
            "r",
            RouteClass::Hosted,
            SizeClass::Small,
        );
        assert_eq!((s.ttft_p50_ms, s.ttft_calls), (None, 0));
        assert!(s.line().contains("first token absent"));
        calls[0].ttft_ms = Some(300);
        calls[1].ttft_ms = Some(500);
        let s =
            RouteSpeed::from_history(&history(calls), "r", RouteClass::Hosted, SizeClass::Small);
        assert_eq!((s.ttft_p50_ms, s.ttft_calls), (Some(300), 2));
    }

    /// A throttled hosted route: the rate-limit wait and the 429s show apart
    /// from generation, which is the call's time without its wait.
    #[test]
    fn a_throttled_route_shows_its_wait_apart_from_generation() {
        let mut calls = Vec::new();
        for _ in 0..5 {
            let mut c = call("hosted", 4_000, 100);
            c.queued_ms = Some(3_000); // waited 3 s for a slot, generated 1 s
            calls.push(c);
        }
        let mut refused = call("hosted", 20, 0);
        refused.throttled = Some(Throttle {
            retry_after_ms: Some(2_500),
        });
        calls.push(refused);
        let s = RouteSpeed::from_history(
            &history(calls),
            "hosted",
            RouteClass::Hosted,
            SizeClass::Small,
        );
        assert_eq!(s.overall.p50_ms, 4_000, "model time includes the wait");
        assert_eq!(s.queued_p50_ms, Some(3_000));
        assert_eq!(
            s.overall.tokens_per_s,
            Some(100.0),
            "100 tokens in 1 s of generation"
        );
        assert_eq!(s.throttled, 1);
        assert_eq!(s.max_retry_after_ms, Some(2_500));
        let line = s.line();
        assert!(line.contains("rate-limit wait p50 3000 ms"), "{line}");
        assert!(
            line.contains("1 x 429 (retry-after up to 2500 ms)"),
            "{line}"
        );
    }

    /// The whole path, with a stub vendor that throttles: the limiter makes a
    /// call wait for its slot, the vendor answers every third call with a 429
    /// and a retry-after, and the ledger read shows both apart from
    /// generation.
    #[tokio::test]
    async fn a_hosted_route_throttled_by_the_stub_shows_queue_time_apart_from_generation() {
        use kgraf_core::{Protocol, TokenUsage};
        use kgraf_provider::message::{Content, StopReason};
        use kgraf_provider::{
            CallToken, Ledger, Provider, ProviderError, RateLimit, RateLimited, Request, Response,
            RouteLimiter, Settlement,
        };
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct Stub(AtomicUsize);
        #[async_trait::async_trait]
        impl Provider for Stub {
            fn adapter(&self) -> &'static str {
                "stub-vendor"
            }
            fn protocol(&self) -> Protocol {
                Protocol::Additive
            }
            async fn complete(
                &self,
                request: &Request,
                token: &CallToken,
            ) -> Result<Response, ProviderError> {
                let n = self.0.fetch_add(1, Ordering::SeqCst);
                if n % 3 == 2 {
                    let e = ProviderError::Http {
                        adapter: "stub-vendor",
                        status: 429,
                        body: "slow down".into(),
                        retry_after_ms: Some(1_500),
                    };
                    token.close_on_async(&e, None).await?;
                    return Err(e);
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                let usage = TokenUsage {
                    uncached_input: 10,
                    text_output: 50,
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

        let ledger = Ledger::open_in_memory().expect("ledger");
        // 1,200 a minute is one slot every 50 ms. The nine calls start at
        // once, so all but the first wait for a slot however slow the host
        // is; calls made one after another would not queue on a loaded host.
        let limiter = RouteLimiter::new("hosted", RateLimit::parse("1200/1@5000").expect("spec"))
            .expect("limiter");
        let p = RateLimited::new(Arc::new(Stub(AtomicUsize::new(0))), limiter);
        let tokens: Vec<_> = (0..9)
            .map(|_| {
                ledger
                    .begin(p.adapter(), p.protocol(), "m", "agent", "task")
                    .expect("begin")
            })
            .collect();
        let request = Request::new("m", "hi", 10);
        futures_util::future::join_all(tokens.iter().map(|t| p.complete(&request, t))).await;
        let h = read_route_calls(&ledger, Some("hosted")).expect("read");
        let throttled = h
            .route_calls
            .iter()
            .filter(|c| c.throttled.is_some())
            .count();
        assert_eq!(throttled, 3, "{:?}", h.route_calls);
        assert!(
            ledger
                .calls()
                .expect("calls")
                .iter()
                .filter(|c| c.settlement == Settlement::Failed)
                .all(|c| c.http_status == Some(429) && c.retry_after_ms == Some(1_500))
        );
        let s = RouteSpeed::from_history(&h, "hosted", RouteClass::Hosted, SizeClass::Small);
        assert_eq!(s.throttled, 3);
        assert_eq!(s.max_retry_after_ms, Some(1_500));
        // The six counted calls are measured; most waited for a slot, and the
        // wait is on the row apart from the 20 ms of generation.
        assert_eq!(s.source, SpeedSource::Measured { calls: 6 });
        let queued = s.queued_p50_ms.expect("a limiter admitted the calls");
        assert!(queued >= 20, "the slot wait is recorded: {queued} ms");
        assert!(
            s.overall.p50_ms >= queued,
            "model time includes the wait: p50 {} ms, wait {queued} ms",
            s.overall.p50_ms
        );
    }
}

//! The delta call's prompt-cache layout through the real loop (decision 0100):
//! with the layout off a run sends no mark and no key; on, each change to a
//! finished run carries the run's cache key and a marked plan block while a
//! read can still come, and no mark once the previous delta call is older
//! than the provider's cache lifetime (run on a stub clock). A route that
//! takes no marks (an OpenAI-compatible one) gets the key, the id order and
//! the stable bytes, and no mark.

mod support;

use kgraf_core::RunId;
use kgraf_orchestrator::{
    ChangeInput, ManualClock, ModelCaller, ModelDecomposer, Orchestrator, OrchestratorConfig,
    Received, RunStatus,
};
use kgraf_provider::capability::{CacheControl, Capabilities};
use kgraf_provider::{CallToken, Content, Provider, ProviderError, Request, Response};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::*;

const WINDOW: u64 = 200_000;

/// Wraps the scripted planner: records each request whole, and says it takes
/// cache breakpoints (a five-minute lifetime) only when `marks` is set.
struct Route {
    inner: Arc<ScriptFake>,
    marks: bool,
    seen: Mutex<Vec<Request>>,
}

impl CacheControl for Route {
    fn max_breakpoints(&self) -> usize {
        4
    }

    fn minimum_cacheable_tokens(&self) -> Option<u32> {
        None
    }
}

#[async_trait::async_trait]
impl Provider for Route {
    fn adapter(&self) -> &'static str {
        "fake-plan"
    }

    fn protocol(&self) -> kgraf_core::Protocol {
        kgraf_core::Protocol::Additive
    }

    async fn complete(
        &self,
        request: &Request,
        token: &CallToken,
    ) -> Result<Response, ProviderError> {
        self.seen.lock().expect("lock").push(request.clone());
        self.inner.complete(request, token).await
    }

    fn capabilities(&self) -> Capabilities<'_> {
        if self.marks {
            Capabilities::none().with_cache_control(self)
        } else {
            Capabilities::none()
        }
    }
}

fn script(prompt: &str) -> String {
    if prompt.contains("<change>") {
        let id = ["d", "c"]
            .into_iter()
            .find(|id| prompt.contains(&format!("also make {id}")))
            .unwrap_or("b");
        serde_json::json!({
            "kind": "delta",
            "add": [{
                "id": id,
                "instruction": format!("make {id} WRITE src/{id}.rs"),
                "acceptance": format!("src/{id}.rs exists"),
                "writes": [{"path": format!("src/{id}.rs")}],
                "depends_on": ["a"],
                "tools": ["write_file"],
            }],
        })
        .to_string()
    } else {
        plan_json(&[c("a", "make a", "src/a.rs", &[])])
    }
}

fn marked(q: &Request) -> bool {
    q.messages.iter().flat_map(|m| m.content.iter()).any(|b| {
        matches!(
            b,
            Content::Text {
                cache_breakpoint: true,
                ..
            }
        )
    })
}

/// A finished run `r1`, then three changes at `times` (milliseconds on the
/// run's clock, which the change's receipt time follows). Returns each delta
/// request the planner got.
async fn run(layout: bool, marks: bool, times: [i64; 3]) -> Vec<Request> {
    let dir = tempfile::tempdir().expect("dir");
    std::fs::create_dir_all(dir.path().join("src")).expect("src");
    let p = Parts::open(
        dir.path(),
        script,
        |_| SERVES.into(),
        Duration::from_millis(5),
    );
    let route = Arc::new(Route {
        inner: p.planner.clone(),
        marks,
        seen: Mutex::new(Vec::new()),
    });
    let clock = Arc::new(ManualClock::new(times[0]));
    let build = |clock: Arc<ManualClock>| {
        let mut services = p.services(WINDOW);
        services.decomposer = Arc::new(ModelDecomposer(ModelCaller {
            reasoning: Default::default(),
            provider: route.clone(),
            ledger: p.ledger.clone(),
            model: PLAN_MODEL.into(),
            max_output_tokens: 2_000,
            agent: "orchestrator:r1".into(),
            task: "r1".into(),
        }));
        services.clock = clock;
        let config = OrchestratorConfig {
            cache_layout: layout,
            lease_ttl_ms: 300,
            ..config()
        };
        Orchestrator::new(config, services).expect("orchestrator")
    };
    let first = build(clock.clone())
        .start(RunId::new("r1"), "make a".into())
        .await
        .expect("first run");
    assert_eq!(first.status, RunStatus::Completed, "{first:?}");
    for (text, at) in ["also make b", "also make c", "also make d"]
        .into_iter()
        .zip(times)
    {
        clock.set(at);
        let received = Received {
            instant: std::time::Instant::now(),
            at_ms: at,
        };
        let report = tokio::time::timeout(
            Duration::from_secs(60),
            build(clock.clone()).reopen(RunId::new("r1"), ChangeInput::change(text, received)),
        )
        .await
        .expect("finished within 60 s")
        .expect("reopen");
        assert_eq!(report.status, RunStatus::Completed, "{report:?}");
    }
    let seen = route.seen.lock().expect("lock").clone();
    seen.into_iter()
        .filter(|q| {
            q.messages
                .iter()
                .flat_map(|m| m.content.iter())
                .filter_map(Content::as_text)
                .any(|t| t.contains("<change>"))
        })
        .collect()
}

const MINUTE: i64 = 60_000;

/// On, with a route that takes marks: the first change marks the plan, the
/// second (a minute later) does, the third (ten minutes after that) does not.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_plan_mark_follows_the_cache_lifetime_on_the_runs_clock() {
    let t = 1_000_000;
    let deltas = run(true, true, [t, t + MINUTE, t + 11 * MINUTE]).await;
    assert_eq!(deltas.len(), 3);
    let marks: Vec<bool> = deltas.iter().map(marked).collect();
    assert_eq!(
        marks,
        [true, true, false],
        "first, within, and past the lifetime"
    );
    for q in &deltas {
        assert_eq!(q.cache_key.as_deref(), Some("run:r1"));
        assert_eq!(q.system.len(), 1, "the system prompt is not marked");
        assert!(!marked_system(q));
    }
}

fn marked_system(q: &Request) -> bool {
    q.system.iter().any(|b| {
        matches!(
            b,
            Content::Text {
                cache_breakpoint: true,
                ..
            }
        )
    })
}

/// Off (the default and every benchmark pin): no mark and no key, whatever
/// the route takes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn with_the_layout_off_no_request_carries_a_mark_or_a_key() {
    let t = 1_000_000;
    let deltas = run(false, true, [t, t + MINUTE, t + 2 * MINUTE]).await;
    assert_eq!(deltas.len(), 3);
    for q in &deltas {
        assert!(!marked(q), "no mark with the layout off");
        assert!(q.cache_key.is_none(), "no key with the layout off");
        assert_eq!(q.messages[0].content.len(), 1, "one block, as before");
    }
}

/// A route that takes no marks (an OpenAI-compatible one) completes the same
/// run: the key goes out, the plan block is stable, no mark is made.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_route_that_takes_no_marks_gets_the_key_and_no_mark() {
    let t = 1_000_000;
    let deltas = run(true, false, [t, t + MINUTE, t + 2 * MINUTE]).await;
    assert_eq!(deltas.len(), 3);
    for q in &deltas {
        assert!(!marked(q), "no mark where the route takes none");
        assert_eq!(q.cache_key.as_deref(), Some("run:r1"));
    }
}

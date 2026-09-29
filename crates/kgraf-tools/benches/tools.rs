//! Budgets: apply-patch's parse-plus-apply cost, and a trivial
//! sandboxed shell call's launch overhead, so a later regression is measured
//! against a recorded number rather than a feeling.
//!
//! Calls run the same path a worker would: through `ApprovalChain::review` and
//! `ToolRegistry::execute`, since [`CallPermit`] has no public constructor
//! (that is the seam `tests/compile_fail` asserts) and a benchmark crate is
//! outside this crate for visibility purposes just like a real caller.

use std::sync::Arc;
use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use kgraf_tools::approval::{ApprovalChain, MemoryApprovalLog, PendingCall, Verdict};
use kgraf_tools::execpolicy::{Decision, ExecPolicy};
use kgraf_tools::{
    ApplyPatchTool, CancellationToken, DiscoveredTools, Exposure, ShellTool, ToolContext,
    ToolOutput, ToolRegistry, ToolView,
};
use serde_json::{Value, json};
use tokio::runtime::Runtime;

fn ctx(root: std::path::PathBuf) -> ToolContext {
    ToolContext {
        worker: "bench-worker".into(),
        task: "bench-task".into(),
        workspace_root: root,
        cancel: CancellationToken::new(),
        progress: kgraf_tools::ProgressSink::default(),
    }
}

/// Runs `tool` by name through approval and the registry, the only path that
/// ever produces a [`kgraf_tools::CallPermit`].
async fn run(registry: &ToolRegistry, tool: &str, input: Value, ctx: &ToolContext) -> ToolOutput {
    let discovered = DiscoveredTools::new();
    let view = ToolView {
        registry,
        discovered: &discovered,
    };
    let call = PendingCall {
        call_id: "bench".into(),
        tool: tool.into(),
        input,
        worker: ctx.worker.clone(),
        task: ctx.task.clone(),
    };
    let mut log = MemoryApprovalLog::default();
    let verdict = ApprovalChain::new(vec![])
        .review(view, call, &mut log)
        .await
        .expect("review");
    let Verdict::Run(approved) = verdict else {
        panic!("bench call was not approved: {verdict:?}");
    };
    registry.execute(approved, ctx).await.expect("execute")
}

/// A realistic-sized update: one context-anchored replacement per line of a
/// ~50 line file, so the fuzzy-match pass in `seek_sequence` does real work.
fn build_patch(line_count: usize) -> (String, String) {
    let mut original = String::new();
    let mut patch = String::from("*** Begin Patch\n*** Update File: bench.txt\n");
    for i in 0..line_count {
        original.push_str(&format!("line {i}\n"));
        patch.push_str(&format!("@@\n-line {i}\n+line {i} updated\n"));
    }
    patch.push_str("*** End Patch");
    (original, patch)
}

fn apply_patch_bench(c: &mut Criterion) {
    let rt = Runtime::new().expect("runtime");
    let mut registry = ToolRegistry::new();
    registry
        .register_builtin(Arc::new(ApplyPatchTool), Exposure::Prefix)
        .expect("register");
    let (original, patch) = build_patch(50);

    c.bench_function("apply_patch/apply_50_line_update", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let dir = tempfile::tempdir().expect("tempdir");
                std::fs::write(dir.path().join("bench.txt"), &original).expect("seed");
                let input = json!({ "patch": patch });
                let start = std::time::Instant::now();
                let out = rt.block_on(run(
                    &registry,
                    "apply_patch",
                    input,
                    &ctx(dir.path().to_path_buf()),
                ));
                total += start.elapsed();
                assert!(!out.is_error);
            }
            total
        })
    });
}

fn shell_bench(c: &mut Criterion) {
    let rt = Runtime::new().expect("runtime");
    let mut registry = ToolRegistry::new();
    let (shell_tool, _hook) = ShellTool::new(ExecPolicy::new(Decision::Allow));
    registry
        .register_builtin(Arc::new(shell_tool), Exposure::Prefix)
        .expect("register");
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();

    // Skip on a platform (or nested sandbox) where the runner cannot launch;
    // a benchmark that always errors would report a meaningless near-zero time.
    let probe = rt.block_on(run(
        &registry,
        "shell",
        json!({"command": ["/bin/echo", "probe"]}),
        &ctx(root.clone()),
    ));
    if probe.is_error {
        eprintln!("skipping shell benchmark: sandboxed execution unavailable here");
        return;
    }

    c.bench_function("shell/sandboxed_echo", |b| {
        b.iter(|| {
            let out = rt.block_on(run(
                &registry,
                "shell",
                json!({"command": ["/bin/echo", "hi"]}),
                &ctx(root.clone()),
            ));
            assert!(!out.is_error);
        })
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(20).measurement_time(Duration::from_secs(8));
    targets = apply_patch_bench, shell_bench
}
criterion_main!(benches);

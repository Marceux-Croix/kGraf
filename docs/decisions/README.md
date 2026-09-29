# Decision records

Durable, standalone records of significant project decisions and their reasoning. Each record
captures the context, the options considered, and the consequences, so a reader can revisit a
decision without re-deriving it.

These are ADR-style and immutable: if a decision is later reversed, a new record supersedes the
old one rather than editing it.

| # | Decision | Status |
|---|----------|--------|
| [0001](0001-native-agent-loop.md) | Native agent loop against provider APIs, not a CLI wrapper | Accepted |
| [0002](0002-rust-tauri-local-desktop.md) | Rust core in a desktop app, local only, all platforms | Accepted; shell superseded by 0054 |
| [0003](0003-graph-is-the-only-surface.md) | The graph is the only work surface; no kanban | Accepted |
| [0004](0004-single-orchestrator.md) | Single orchestrator with full directional control | Accepted; amended by 0067 |
| [0005](0005-internal-envelope-and-protocols.md) | Internal envelope; no agent wire protocol in v1 | Accepted; amended by 0051, 0067 |
| [0006](0006-provider-layer-shape.md) | Hand-rolled provider clients; a thin trait plus capability traits | Accepted; extended by 0041 |
| [0007](0007-persistence-and-event-spine.md) | Durable facts, derived status, change-log event spine | Accepted |
| [0008](0008-cost-position.md) | kGraf makes no token-savings claim | Accepted |
| [0009](0009-disjoint-state-write-discipline.md) | Workers read widely, write disjointly | Accepted |
| [0010](0010-galaxy-ui-stack.md) | A server-computed, cached, anchored graph layout | Accepted; renderer superseded by 0054 |
| [0011](0011-sandboxing-per-platform.md) | Per-platform sandboxing, honestly labeled, no admin required | Accepted |
| [0012](0012-heterogeneous-specialization-primary.md) | Heterogeneous specialization primary; redundancy secondary | Accepted |
| [0013](0013-document-corpus-seeds-the-graph.md) | The whole document corpus seeds the graph; no format imposed | Accepted |
| [0014](0014-work-node-hierarchy-and-execution-control.md) | Recursive work nodes, derived progress and blocking | Accepted |
| [0015](0015-dynamic-graph-and-local-perturbation.md) | The graph is dynamic; stability means local perturbation | Accepted |
| [0016](0016-scheduler-scope-is-the-whole-work-graph.md) | The scheduler's scope is the whole work graph, not one item | Accepted |
| [0017](0017-concurrency-limits-and-settings.md) | Bounded autonomy, a concurrency cap, a settings surface | Accepted; spend-cap parts superseded by 0080 |
| [0018](0018-continuous-merge-and-gate-nodes.md) | Continuous merge, with quality gates as first-class work nodes | Accepted; conflict handling amended by 0073 |
| [0019](0019-gates-at-hierarchy-joins-and-tests-in-done.md) | Every major node carries a gate, and done includes tests | Accepted |
| [0020](0020-cochange-is-the-collision-relation.md) | Co-change is the collision relation, weighted and never thresholded | Accepted; refined by 0022 |
| [0021](0021-cold-start-policy.md) | Cold start: a weak prior, optimistic dispatch, a concurrency ramp | Accepted |
| [0022](0022-stop-predicting-lock-at-runtime.md) | Lock collisions at runtime; prefer dissimilar tasks; speculate, then gate | Accepted |
| [0023](0023-the-coordination-curse.md) | Solving collisions does not make concurrent agents work | Accepted |
| [0024](0024-the-orchestrator-is-the-hypothesis.md) | The orchestrator is the hypothesis, tested with independent arms | Accepted |
| [0025](0025-scoping-depth-and-chunk-size.md) | Scope deeply on interfaces; chunk size has an optimum, not a direction | Accepted |
| [0026](0026-agent-communication-is-durable-prose.md) | Contracts are signature plus test; agent communication is durable prose | Accepted |
| [0027](0027-contracts-are-amended-or-superseded.md) | An inadequate contract is marked, then amended or superseded | Accepted |
| [0028](0028-no-pruning.md) | Conversation files are never pruned | Accepted |
| [0029](0029-agents-may-challenge-a-contract.md) | Agents may challenge a contract; the orchestrator reviews and decides | Accepted |
| [0030](0030-mechanical-classification-and-early-detection.md) | Classification is mechanical; checks fire before the gate | Accepted |
| [0031](0031-closing-the-vacuity-and-handshake-holes.md) | Vacuity runs against the stub and is two-sided; the handshake states assumptions | Accepted |
| [0032](0032-search-is-primary-navigation.md) | Search is primary navigation; results are a graph selection | Accepted |
| [0033](0033-dependency-is-observed-not-predicted.md) | Dependency is observed and subscribed to, not predicted | Accepted |
| [0034](0034-one-text-surface.md) | One text surface, fed by selection or search, routing back to the graph | Accepted; amended by 0063 |
| [0035](0035-challenge-escalation.md) | The orchestrator may overturn itself; a re-raised challenge escalates to the user | Accepted |
| [0036](0036-what-counts-as-a-clear-error.md) | An error is clear when it checks against a recorded artifact | Accepted |
| [0037](0037-rejections-name-their-artifact.md) | A rejection names its artifact, or states plainly that it is judgment | Accepted |
| [0038](0038-call-resolution-is-proximity-scoped-and-fanout-bounded.md) | Call resolution is name-based, proximity-scoped, fan-out bounded | Accepted; refined by 0065 |
| [0039](0039-accounting-is-enforced-by-an-unforgeable-call-token.md) | A provider call requires an unforgeable ledger token | Accepted |
| [0040](0040-type-level-invariants-get-compile-fail-tests.md) | A type-level invariant gets a compile-fail test | Accepted |
| [0041](0041-openai-compatible-is-two-protocols-and-vendors-live-in-a-catalog.md) | An OpenAI-compatible route is two protocols; vendors live in a catalog | Accepted |
| [0042](0042-a-decider-seam-cpu-first-wire-format-targeted-calibration-gated.md) | A decider seam: CPU-first, wire-format targeted, accepted on calibration | Accepted; narrowed by 0045 |
| [0043](0043-one-shared-prefix-many-private-slices.md) | Workers share one byte-identical prefix; slices stay private | Accepted |
| [0044](0044-an-adapter-is-verified-against-a-running-speaker.md) | An adapter is verified against a running speaker, not its own mock | Accepted |
| [0045](0045-the-local-decider-loses-to-the-frontier-and-the-seam-stays.md) | A local decider loses on this task; the seam stays, no weights ship | Accepted; diagnosis amended by 0046 |
| [0046](0046-the-local-decider-negative-is-a-category-error.md) | A negative result on the decider is a category error, not a data gap | Accepted; causal claim withdrawn by 0047 |
| [0047](0047-the-local-decider-question-was-answered-and-the-seam-is-rescoped.md) | The decider question was answered before it ran; the seam is re-scoped, no weights ever | Accepted; supersedes 0042's acceptance premise and 0045's robustness argument |
| [0048](0048-deadlock-is-prevented-by-the-registry-and-arbitrated-by-the-orchestrator.md) | A registry makes deadlock unconstructible; the orchestrator arbitrates blocks | Accepted |
| [0049](0049-file-leases-over-serialize-and-sub-file-leases-need-stable-identity.md) | File leases over-serialize; sub-file leases need stable identity | Accepted; replaces 0022's serialization criterion |
| [0050](0050-sub-file-leases-are-keyed-on-hierarchical-node-ids.md) | A sub-file lease is keyed on a hierarchical node id | Accepted; follows 0049 |
| [0051](0051-single-user-and-hosted-mcp-servers-are-allowed.md) | Single-user; hosted external tool servers are allowed | Accepted; amends 0005 |
| [0052](0052-scheduled-runs-are-in-scope-with-a-background-process.md) | Scheduled runs are in scope, with a background process | Accepted |
| [0053](0053-instructions-and-skills-are-ingested-from-the-user-s-existing-agents.md) | Instructions and skills are ingested from the user's existing agents | Accepted |
| [0054](0054-a-native-rust-shell-that-runs-without-a-gpu.md) | A native Rust shell that runs without a GPU | Accepted; supersedes the shell half of 0002 and the renderer half of 0010 |
| [0055](0055-reuse-existing-code-before-writing-new-code.md) | Reuse existing code before writing new code | Accepted; amends 0054; amended by 0061 |
| [0056](0056-long-running-tasks-resume-from-a-run-state-record.md) | Long-running tasks resume from a run-state record | Accepted |
| [0057](0057-compaction-never-rewrites-and-runs-re-ground-on-the-original-instruction.md) | Compaction never rewrites; a resumed run re-grounds on the original instruction | Accepted; refines 0056 |
| [0058](0058-the-galaxy-layout-stays-solver-free.md) | The graph layout stays solver-free; clusters grow by size bucket | Accepted; amends 0010, 0015 |
| [0059](0059-the-ledger-is-an-event-sourced-hash-chain-with-an-external-tip.md) | The ledger is an event-sourced hash chain with an external tip | Accepted |
| [0060](0060-each-worktree-builds-into-its-own-target-directory.md) | Each worktree builds into its own target directory | Accepted |
| [0061](0061-windows-msi-via-wixl.md) | The Windows installer uses an open-source packaging tool, not a Rust-native one | Accepted; amends 0055 |
| [0062](0062-rename-to-kgraf.md) | The project is renamed kGraf; old state migrates on first use, history is never rewritten | Accepted |
| [0063](0063-one-live-activity-card-routing-back-to-the-graph.md) | One live activity card is permitted, and every row routes back to the graph | Accepted; amends 0034 |
| [0064](0064-presumed-work-nodes-from-the-roadmap.md) | Presumed work nodes come from a project's own planning documents, and are adopted, never duplicated | Accepted |
| [0065](0065-calls-stay-in-their-language-extractors-are-stamped-a-tags-tier.md) | A call resolves only within its own language family; each extractor is stamped | Accepted; refines 0038 |
| [0066](0066-a-project-switcher-in-the-guide.md) | Several projects open in one window, switched from the navigation rail | Accepted |
| [0067](0067-every-agent-is-an-addressable-session.md) | Every agent is an addressable session; subagents, connections and a per-machine parallelism limit | Accepted; supersedes 0017's concurrency defaults; amends 0004, 0005, 0033, 0034 |
| [0068](0068-large-graphs-fold-children-into-parents.md) | Large graphs fold children into their parents under a visible-node budget | Accepted |
| [0069](0069-local-diagnostics-clarifies-no-telemetry.md) | Local-only diagnostics clarifies "no telemetry" | Accepted |
| [0070](0070-color-vision-modes-live-only-in-kgraf.md) | Color vision modes live in kGraf's own theme code, chosen by measurement | Accepted |
| [0071](0071-transcript-secret-scrubbing.md) | Transcripts and traces are scrubbed of secrets before they are written, shown, or sent to a model | Accepted |
| [0072](0072-cross-process-rate-limits-sqlite-gcra.md) | Cross-process rate limits are a shared, disk-backed store, never a silent per-process fallback | Accepted |
| [0073](0073-merge-conflicts-get-a-resolver-worker.md) | A merge conflict dispatches a resolver worker before a plain re-dispatch | Accepted; amends 0018 |
| [0074](0074-speculative-merge-reports-and-never-delays.md) | Speculative merge reports a conflict early and never delays a merge | Accepted |
| [0075](0075-routing-by-coupling-and-measured-capability.md) | Vendor and model routing by coupling and measured capability, opt-in | Accepted |
| [0076](0076-consumers-write-a-test-of-what-they-expect.md) | Each consumer of an interface writes and gates its own test of what it expects | Accepted |
| [0077](0077-results-carry-the-producer-s-reasons.md) | A worker's result carries its own written reasons, for a dependent worker to read | Accepted |
| [0078](0078-handshake-mismatch-diagnosis-and-the-checks-default.md) | The handshake comparator reads qualified names and signatures; checks stay off by default | Accepted |
| [0079](0079-a-cross-vendor-review-at-the-interface.md) | A producer's delivery gets one review from a model on a cross-vendor consumer's route, off by default | Accepted |
| [0080](0080-kgraf-does-not-limit-spend.md) | kGraf does not limit spend | Accepted; supersedes 0017's spend-cap parts |
| [0081](0081-stubs-land-on-the-integration-branch-first.md) | An interface's placeholder implementation lands on the shared branch before any party is dispatched | Accepted |
| [0082](0082-at-most-two-coupled-contracts-in-flight.md) | At most two contracts of one coupling group run at the same time | Accepted |
| [0083](0083-host-writes-never-follow-a-link-and-merge-back-holds-the-declared-writes.md) | Host writes never follow a symbolic link, and a merge holds an attempt to only what it declared | Accepted |
| [0084](0084-workers-run-an-allowlist-of-build-and-test-commands-in-the-sandbox.md) | Workers run an allowlist of build and test commands in the sandbox, with the network off | Accepted |
| [0085](0085-retry-route-diversity.md) | A retry of an unpinned contract uses a different route, and a recorded route is not a pin | Accepted |
| [0086](0086-empty-attempts-checked-against-a-placeholder.md) | An empty attempt is refused when its declared write target is still a placeholder, and every accepted empty attempt is reported | Accepted |
| [0087](0087-dependencies-fetched-on-the-host-with-consent.md) | Dependencies are fetched on the host, outside the sandbox, with the user's per-project consent | Accepted |

# Decision records

Durable, standalone records of significant project decisions and their reasoning. Each record
gives the context, the options considered and the consequences, so a reader can revisit a
decision without re-deriving it.

Records are dated. A record is not rewritten: a change is an appended, dated amendment, and a
decision that is reversed gets a new record that supersedes the old one. A long record keeps
its decision first and its amendments after it, oldest to newest. A record that opens with a
**Current state** block (0030, 0084, 0094, 0097) says what the code does now, with an index of its
amendments. For any other record, read its first section and its newest amendment. A header line says what changed it:
`Amended by`, `Superseded by` (in whole or in part), `Amends`, `Supersedes`.

## Start here

The records hold about 200,000 words. These twelve give the shape of the project. Read them
in this order.

1. [0001](0001-native-agent-loop.md): kGraf calls provider APIs directly, with its own agent loop.
2. [0003](0003-graph-is-the-only-surface.md): the graph is the only work surface; there is no board.
3. [0004](0004-single-orchestrator.md): one orchestrator directs all workers.
4. [0009](0009-disjoint-state-write-discipline.md): workers read widely and write to disjoint places.
5. [0023](0023-the-coordination-curse.md): why preventing collisions does not by itself make concurrent agents work.
6. [0067](0067-every-agent-is-an-addressable-session.md): every agent is a session that can be messaged; the parallelism limit.
7. [0008](0008-cost-position.md): kGraf makes no claim to save tokens.
8. [0080](0080-kgraf-does-not-limit-spend.md): kGraf never refuses work for want of budget; accounting stays complete.
9. [0089](0089-models-of-every-kind-are-routes.md): a model of any kind is a route; the orchestrator decides its use.
10. [0011](0011-sandboxing-per-platform.md): sandboxing per platform, honestly labeled. On Windows, read [0097](0097-windows-commands-run-as-a-dedicated-sandbox-user.md) next.
11. [0084](0084-workers-run-an-allowlist-of-build-and-test-commands-in-the-sandbox.md): what a worker may run, and where.
12. [0094](0094-permission-modes-bypass-limits-grouped-requests-and-plan-review.md): permission modes, and the limits that hold in every mode.

## Status at a glance

Active means no later record changed it. Where a record changes an earlier one, both rows say so.

| # | Decision | Status |
|---|----------|--------|
| [0001](0001-native-agent-loop.md) | Native agent loop against provider APIs, not a CLI wrapper | Active |
| [0002](0002-rust-tauri-local-desktop.md) | Rust core in a desktop app, local only, all platforms | Partly superseded by 0054 |
| [0003](0003-graph-is-the-only-surface.md) | The graph is the only work surface; no kanban | Amended by 0017 |
| [0004](0004-single-orchestrator.md) | Single orchestrator with full directional control | Amended by 0026, 0067 |
| [0005](0005-internal-envelope-and-protocols.md) | Internal envelope; no agent wire protocol in v1 | Amended by 0051, 0067 |
| [0006](0006-provider-layer-shape.md) | Hand-rolled provider clients; a thin trait plus capability traits | Amended by 0041 |
| [0007](0007-persistence-and-event-spine.md) | Durable facts, derived status, change-log event spine | Active |
| [0008](0008-cost-position.md) | kGraf makes no token-savings claim | Active |
| [0009](0009-disjoint-state-write-discipline.md) | Workers read widely, write disjointly | Partly superseded by 0012 |
| [0010](0010-galaxy-ui-stack.md) | A server-computed, cached, anchored graph layout | Partly superseded by 0054; amended by 0015, 0058 |
| [0011](0011-sandboxing-per-platform.md) | Per-platform sandboxing, honestly labeled, no admin required | Partly superseded by 0097 |
| [0012](0012-heterogeneous-specialization-primary.md) | Heterogeneous specialization primary; redundancy secondary | Amended by 0026; supersedes part of 0009 |
| [0013](0013-document-corpus-seeds-the-graph.md) | The whole document corpus seeds the graph; no format imposed | Active |
| [0014](0014-work-node-hierarchy-and-execution-control.md) | Recursive work nodes, derived progress and blocking | Amended by 0015, 0025 |
| [0015](0015-dynamic-graph-and-local-perturbation.md) | The graph is dynamic; stability means local perturbation | Amended by 0058; amends 0010, 0014 |
| [0016](0016-scheduler-scope-is-the-whole-work-graph.md) | The scheduler's scope is the whole work graph, not one item | Active |
| [0017](0017-concurrency-limits-and-settings.md) | Bounded autonomy, a concurrency cap, a settings surface | Partly superseded by 0067, 0080; amends 0003 |
| [0018](0018-continuous-merge-and-gate-nodes.md) | Continuous merge, with quality gates as first-class work nodes | Amended by 0019, 0073 |
| [0019](0019-gates-at-hierarchy-joins-and-tests-in-done.md) | Every major node carries a gate, and done includes tests | Active; amends 0018 |
| [0020](0020-cochange-is-the-collision-relation.md) | Co-change is the collision relation, weighted and never thresholded | Active |
| [0021](0021-cold-start-policy.md) | Cold start: a weak prior, optimistic dispatch, a concurrency ramp | Active |
| [0022](0022-stop-predicting-lock-at-runtime.md) | Lock collisions at runtime; prefer dissimilar tasks; speculate, then gate | Amended by 0023, 0049 |
| [0023](0023-the-coordination-curse.md) | Solving collisions does not make concurrent agents work | Amended by 0024; corrects 0022 |
| [0024](0024-the-orchestrator-is-the-hypothesis.md) | The orchestrator is the hypothesis, tested with independent arms | Active; amends 0023 |
| [0025](0025-scoping-depth-and-chunk-size.md) | Scope deeply on interfaces; chunk size has an optimum, not a direction | Active; amends 0014 |
| [0026](0026-agent-communication-is-durable-prose.md) | Contracts are signature plus test; agent communication is durable prose | Active; amends 0004, 0012 |
| [0027](0027-contracts-are-amended-or-superseded.md) | An inadequate contract is marked, then amended or superseded | Amended by 0030 |
| [0028](0028-no-pruning.md) | Conversation files are never pruned | Active |
| [0029](0029-agents-may-challenge-a-contract.md) | Agents may challenge a contract; the orchestrator reviews and decides | Amended by 0030 |
| [0030](0030-mechanical-classification-and-early-detection.md) | Classification is mechanical; checks fire before the gate | Active; amends 0027, 0029 |
| [0031](0031-closing-the-vacuity-and-handshake-holes.md) | Vacuity runs against the stub and is two-sided; the handshake states assumptions | Active |
| [0032](0032-search-is-primary-navigation.md) | Search is primary navigation; results are a graph selection | Amended by 0034 |
| [0033](0033-dependency-is-observed-not-predicted.md) | Dependency is observed and subscribed to, not predicted | Amended by 0067 |
| [0034](0034-one-text-surface.md) | One text surface, fed by selection or search, routing back to the graph | Amended by 0063, 0067; amends 0032 |
| [0035](0035-challenge-escalation.md) | The orchestrator may overturn itself; a re-raised challenge escalates to the user | Active |
| [0036](0036-what-counts-as-a-clear-error.md) | An error is clear when it checks against a recorded artifact | Active |
| [0037](0037-rejections-name-their-artifact.md) | A rejection names its artifact, or states plainly that it is judgment | Active |
| [0038](0038-call-resolution-is-proximity-scoped-and-fanout-bounded.md) | Call resolution is name-based, proximity-scoped, fan-out bounded | Amended by 0065 |
| [0039](0039-accounting-is-enforced-by-an-unforgeable-call-token.md) | A provider call requires an unforgeable ledger token | Amended by 0040 |
| [0040](0040-type-level-invariants-get-compile-fail-tests.md) | A type-level invariant gets a compile-fail test | Active; corrects 0039 |
| [0041](0041-openai-compatible-is-two-protocols-and-vendors-live-in-a-catalog.md) | An OpenAI-compatible route is two protocols; vendors live in a catalog | Active; extends 0006 |
| [0042](0042-a-decider-seam-cpu-first-wire-format-targeted-calibration-gated.md) | A decider seam: CPU-first, wire-format targeted, accepted on calibration | Partly superseded by 0047 |
| [0043](0043-one-shared-prefix-many-private-slices.md) | Workers share one byte-identical prefix; slices stay private | Active |
| [0044](0044-an-adapter-is-verified-against-a-running-speaker.md) | An adapter is verified against a running speaker, not its own mock | Active |
| [0045](0045-the-local-decider-loses-to-the-frontier-and-the-seam-stays.md) | A local decider loses on this task; the seam stays, no weights ship | Partly superseded by 0047; amended by 0046 |
| [0046](0046-the-local-decider-negative-is-a-category-error.md) | A negative result on the decider is a category error, not a data gap | Amended by 0047; amends 0045 |
| [0047](0047-the-local-decider-question-was-answered-and-the-seam-is-rescoped.md) | The decider question was answered before it ran; the seam is re-scoped, no weights ever | Active; supersedes part of 0042, 0045; amends 0046 |
| [0048](0048-deadlock-is-prevented-by-the-registry-and-arbitrated-by-the-orchestrator.md) | A registry makes deadlock unconstructible; the orchestrator arbitrates blocks | Amended by 0049 |
| [0049](0049-file-leases-over-serialize-and-sub-file-leases-need-stable-identity.md) | File leases over-serialize; sub-file leases need stable identity | Active; amends 0022, 0048 |
| [0050](0050-sub-file-leases-are-keyed-on-hierarchical-node-ids.md) | A sub-file lease is keyed on a hierarchical node id | Active; follows 0049 |
| [0051](0051-single-user-and-hosted-mcp-servers-are-allowed.md) | Single-user; hosted external tool servers are allowed | Active; amends 0005 |
| [0052](0052-scheduled-runs-are-in-scope-with-a-background-process.md) | Scheduled runs are in scope, with a background process | Active |
| [0053](0053-instructions-and-skills-are-ingested-from-the-user-s-existing-agents.md) | Instructions and skills are ingested from the user's existing agents | Active |
| [0054](0054-a-native-rust-shell-that-runs-without-a-gpu.md) | A native Rust shell that runs without a GPU | Amended by 0055; supersedes part of 0002, 0010 |
| [0055](0055-reuse-existing-code-before-writing-new-code.md) | Reuse existing code before writing new code | Amended by 0061; amends 0054 |
| [0056](0056-long-running-tasks-resume-from-a-run-state-record.md) | Long-running tasks resume from a run-state record | Amended by 0057 |
| [0057](0057-compaction-never-rewrites-and-runs-re-ground-on-the-original-instruction.md) | Compaction never rewrites; a resumed run re-grounds on the original instruction | Amended by 0095; amends 0056 |
| [0058](0058-the-galaxy-layout-stays-solver-free.md) | The graph layout stays solver-free; clusters grow by size bucket | Active; amends 0010, 0015 |
| [0059](0059-the-ledger-is-an-event-sourced-hash-chain-with-an-external-tip.md) | The ledger is an event-sourced hash chain with an external tip | Active |
| [0060](0060-each-worktree-builds-into-its-own-target-directory.md) | Each worktree builds into its own target directory | Active |
| [0061](0061-windows-msi-via-wixl.md) | The Windows installer uses an open-source packaging tool, not a Rust-native one | Active; amends 0055 |
| [0062](0062-rename-to-kgraf.md) | The project is renamed kGraf; old state migrates on first use, history is never rewritten | Active |
| [0063](0063-one-live-activity-card-routing-back-to-the-graph.md) | One live activity card is permitted, and every row routes back to the graph | Active; amends 0034 |
| [0064](0064-presumed-work-nodes-from-the-roadmap.md) | Presumed work nodes come from a project's planning documents and are adopted, never duplicated | Active |
| [0065](0065-calls-stay-in-their-language-extractors-are-stamped-a-tags-tier.md) | A call resolves only within its own language family; each extractor is stamped | Active; amends 0038 |
| [0066](0066-a-project-switcher-in-the-guide.md) | Several projects open in one window, switched from the navigation rail | Active |
| [0067](0067-every-agent-is-an-addressable-session.md) | Every agent is an addressable session; subagents, connections, a parallelism limit | Amended by 0095; supersedes part of 0017; amends 0004, 0005, 0033, 0034 |
| [0068](0068-large-graphs-fold-children-into-parents.md) | Large graphs fold children into their parents under a visible-node budget | Active |
| [0069](0069-local-diagnostics-clarifies-no-telemetry.md) | Local-only diagnostics clarifies "no telemetry" | Active |
| [0070](0070-color-vision-modes-live-only-in-kgraf.md) | Color vision modes live in kGraf's own theme code, chosen by measurement | Active |
| [0071](0071-transcript-secret-scrubbing.md) | Transcripts and traces are scrubbed of secrets before they are written or shown | Active |
| [0072](0072-cross-process-rate-limits-sqlite-gcra.md) | Cross-process rate limits are a shared disk-backed store, never a per-process fallback | Active |
| [0073](0073-merge-conflicts-get-a-resolver-worker.md) | A merge conflict dispatches a resolver worker before a plain re-dispatch | Active; amends 0018 |
| [0074](0074-speculative-merge-reports-and-never-delays.md) | Speculative merge reports a conflict early and never delays a merge | Active |
| [0075](0075-routing-by-coupling-and-measured-capability.md) | Vendor and model routing by coupling and measured capability, opt-in | Active |
| [0076](0076-consumers-write-a-test-of-what-they-expect.md) | Each consumer of an interface writes and gates its own test of what it expects | Active |
| [0077](0077-results-carry-the-producer-s-reasons.md) | A worker's result carries its own written reasons, for a dependent worker to read | Active |
| [0078](0078-handshake-mismatch-diagnosis-and-the-checks-default.md) | The handshake comparator reads qualified names and signatures; checks stay off by default | Active |
| [0079](0079-a-cross-vendor-review-at-the-interface.md) | A cross-vendor consumer's route reviews a producer's delivery once, off by default | Active |
| [0080](0080-kgraf-does-not-limit-spend.md) | kGraf does not limit spend | Active; supersedes part of 0017 |
| [0081](0081-stubs-land-on-the-integration-branch-first.md) | An interface's placeholder implementation lands on the shared branch before dispatch | Active |
| [0082](0082-at-most-two-coupled-contracts-in-flight.md) | At most two contracts of one coupling group run at the same time | Active |
| [0083](0083-host-writes-never-follow-a-link-and-merge-back-holds-the-declared-writes.md) | Host writes never follow a symbolic link, and a merge holds an attempt to only what it declared | Active |
| [0084](0084-workers-run-an-allowlist-of-build-and-test-commands-in-the-sandbox.md) | Workers run an allowlist of build and test commands in the sandbox, with the network off | Active |
| [0085](0085-retry-route-diversity.md) | A retry of an unpinned contract uses a different route, and a recorded route is not a pin | Active |
| [0086](0086-empty-attempts-checked-against-a-placeholder.md) | An empty attempt is refused when its declared write target is still a placeholder | Active |
| [0087](0087-dependencies-fetched-on-the-host-with-consent.md) | Dependencies are fetched on the host, outside the sandbox, with the user's per-project consent | Active |
| [0088](0088-worker-worktrees-deleted-after-use-unless-kept.md) | Worker worktrees and their builds are deleted after use unless the user chooses to keep them | Active |
| [0089](0089-models-of-every-kind-are-routes.md) | Models of every kind are routes; the orchestrator decides their use | Active |
| [0090](0090-google-adapters.md) | Google routes: a native Gemini adapter, Gemini and Anthropic on Vertex AI, all unverified | Active |
| [0091](0091-bedrock-converse-adapter.md) | AWS Bedrock is reached through the Converse operations, signed with SigV4 | Active |
| [0092](0092-azure-openai-transport.md) | Azure OpenAI is a transport option of two adapters, with the deployment-to-model mapping checked | Active |
| [0093](0093-a-worker-reports-a-shortfall-and-the-orchestrator-grants-within-the-operator-ceiling.md) | A worker reports a permission or instruction shortfall; the orchestrator grants, amends or asks | Partly superseded by 0094 |
| [0094](0094-permission-modes-bypass-limits-grouped-requests-and-plan-review.md) | Permission modes, grouped requests and plan review before dispatch | Active; supersedes part of 0093 |
| [0095](0095-a-worker-that-cannot-hand-back-is-compacted-once-by-the-runtime.md) | A worker that cannot hand back is compacted once by the runtime, with a recall tool | Active; amends 0057, 0067 |
| [0096](0096-the-round-budget-is-a-progress-checkpoint.md) | The round budget is a progress checkpoint, not a wall | Active |
| [0097](0097-windows-commands-run-as-a-dedicated-sandbox-user.md) | On Windows, sandboxed commands run as a dedicated local sandbox user | Active; supersedes part of 0011 |
| [0098](0098-one-reasoning-level-setting-covers-every-model-call.md) | One reasoning-level setting covers every model call | Active |
| [0099](0099-replace-in-file-is-an-edit-tool-that-needs-no-patch-format.md) | `replace_in_file` is an edit tool that needs no patch format | Active |
| [0100](0100-the-plan-is-a-living-work-graph.md) | The plan is a living work graph, and a run is a working session | Proposed |

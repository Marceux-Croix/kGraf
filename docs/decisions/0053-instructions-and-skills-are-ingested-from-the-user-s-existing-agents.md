# Decision 0053: Instructions and skills are ingested from the user's existing agents

**Status:** Accepted
**Related:** decision 0043

## Decision

1. **kGraf's skill format is the Agent Skills `SKILL.md` format.** Each skill is a directory
   holding YAML frontmatter, a markdown body, and optional bundled files. The format is not
   reinvented.

2. **Global instruction sets are markdown files in the style of a project-level instructions
   file.** That covers the equivalent files used by several other coding agents. They are read
   as ordered layers, from the user's global settings down to the specific project, each
   carrying a record of its own source path.

3. **kGraf unifies what the user's other agents already have into one harness.** Automatic
   discovery reads the well-known local locations of the coding agents the user already runs.
   An explicit import tool ingests a folder the user points it at, covering any location
   automatic discovery does not know about.

4. **Unification never silently picks a winner.** Byte-identical skills found in several
   places are merged, and every source location is kept on record. Two skills with the same
   name and different content are reported as a conflict, with both sources shown; the one
   used is the one the user explicitly chooses. Overlapping instruction layers are kept in
   order, with their provenance; they are never concatenated without recording where each part
   came from.

5. **Instructions stored only on a vendor's own server are not fetched by default.** No
   documented public API was found, among the vendors surveyed, for reading a user's
   account-level instructions or skills; that check was not exhaustive. Until such an API is
   documented and verified against a real, running server, server-held material is imported by
   exporting it to a folder locally and using the import tool.

## How ingested material is used

- **The skill catalog enters the shared prefix** described in decision 0043. Catalog entries
  (name and description only) are byte-identical for every worker on the same vendor and
  model. Full skill bodies are loaded on demand into a worker's private context.
- **Frontmatter controls are enforced by the runtime, not by the prompt.** A skill's declared
  allowed-tools list filters the worker's available tools directly; a skill marked as
  disabling model invocation makes its own tool refuse to run. A control that only asks the
  model to comply, rather than being enforced by the runtime itself, was already rejected for
  write leases and is rejected here for the same reason.
- **Skill-injected messages are tagged**, so nothing downstream mistakes a skill's own body
  text for something the user said directly.

## Consequences

The importer can be built as a self-contained tool that needs no full agent runtime; its
output is an inventory that the rest of the system consumes.

Discovery paths for other agents' local file locations are kept as a single data table, so
supporting a new agent is adding a row, not a code change.

**Reactivation trigger.** A vendor documenting a public API for account-level instructions or
skills. Then a fetcher is added, verified against that vendor's real, running service.

## Amendments (2026-09-30)

- **The controls reach the worker loop.** A worker environment is built with the project's
  skills, and the frontmatter controls are enforced on its tool calls:
  - **Source.** A run reads the project's inventory (`.kgraf/inventory.json`, written by
    `kgraf instructions scan`) once, when it is built, so every environment of the run
    carries the same catalog and the shared prefix stays byte-identical per vendor and
    model (decision 0043). A missing inventory means no skills; an inventory that cannot be
    read fails the run.
  - **Activation.** A worker environment with at least one skill the model may invoke gets a
    `skill` tool and lists those skills (name and description) in the shared prefix. The tool
    refuses an unknown skill and a skill marked `disable-model-invocation`; such a skill is
    not listed. A successful activation returns the body as the tool result, in a
    `<skill_instructions>` block tagged with the skill's name and content hash.
  - **Enforcement.** Each worker's approval chain carries a policy bound to that worker. An
    activated skill with `allowed-tools` narrows it, and a call outside the allowed set is
    refused with the skill's name in the result the model reads. The policy is checked at
    approval and again when each call starts: a round's calls are all approved before any
    runs, and the `skill` call serializes against the rest of its round, so a call issued
    after an activation in the same round is refused too. The `skill` tool is itself subject
    to an active restriction. Several active skills intersect.
  - **Resume.** A resumed worker re-activates, from its transcript, every skill its earlier
    segments activated, so a restart does not lift a restriction. Each is re-activated only
    at the content hash its activation result recorded: a skill edited, removed or made not
    model-invocable since refuses the resume, naming the skill and the two ways on (restore
    the skill as it was, or start a new run). Resuming on the current version instead would
    change the restriction under a worker mid-task.
  - **Subagents.** A subagent a restricted worker spawns inherits the restrictions its parent
    holds at that moment, whatever route the child runs on, and its own activations narrow
    further. The inherited set (skill, hash, `allowed-tools` entries) is kept in the spawn
    record, so a restarted child is restricted the same way without the inventory. A spawn
    from a worker whose policy is not known yet is refused, never started unrestricted.
  - **Write scopes.** The `skill` call orders against the rest of its round and declares
    no writes, so a worker with a write scope or a fence (every orchestrated worker and
    subagent) can activate a skill; declaring it as a call that may write anywhere refused
    it there.
  - **Denials.** A call refused when it starts is recorded as a denial at approval is: the
    same result prefix, and counted in the worker's denials.
  - **Catalog text.** A skill's name and description are written into the shared prefix on
    one line each, with line breaks and other control characters escaped, so a description
    cannot write lines that read as further catalog entries.
  - **Benchmark runs.** A Terminal-Bench bridge run (`--tbench-bridge-socket`) reads no
    skills, so a benchmark keeps the prefix and tool list its recorded runs were measured
    with; an inventory in its state directory is reported on stderr as not read.
  - **Inventory writes.** `kgraf instructions` writes the inventory to a temporary file and
    renames it into place, so a run reading it never sees a partial file.
  - **Unchanged without skills.** With no inventory, or none the model may invoke, no tool is
    added and the catalog is empty, so the prefix digest does not change.
  - **Where skills can be activated.** Workers only, through the `skill` tool. The
    orchestrator's own model calls (intake and planning) carry no tools, so no skill can be
    activated there. A brief that names skills is still refused.

## Amendment: a skill's refusal comes first and is carried by a shortfall report

- **A skill's refusal comes first.** When a permission hook (the command policy, the
  permission mode) would ask about or deny a call that an active skill's `allowed-tools`
  also refuses, the skill's refusal is the one recorded and shown, and it names the hook's
  verdict too ("... not permitted by skill `x`'s allowed-tools; `shell-command-policy`
  would also have asked to run it"). Nobody is asked about a call that cannot run. Before,
  the ask came first; in a run nobody attends it became a denial that never named the
  skill, so the worker could not tell what restricted it.
- **Reporting a shortfall is never restricted.** `report_shortfall` runs nothing and every
  policy refusal tells the worker to call it, so no skill's `allowed-tools` restricts the
  built-in (an MCP tool of that name is restricted as any other). The `skill` tool stays
  restricted, as above.
- **The report carries the skill.** Each attempt's skill session is registered in a
  run-wide directory that `report_shortfall` reads. A permission report whose command
  (as a `shell` subject) or tool the reporting worker's active skill refuses carries that
  refusal to the orchestrator, which holds the contract with the skill's name: no
  permission grant lifts a skill's `allowed-tools`, so a grant would only start an attempt
  the same skill refuses again.

## Amendment (2026-10-02): a skill's refusal of a shell call is a refusal of the tool

- **The tool, not the program.** `allowed-tools` restricts tools, so a skill's refusal of a
  shell call is recorded as a refusal of the `shell` tool (`RefusedCall::whole_tool`), with the
  program the call ran kept beside it. A report of scope `tool` naming `shell`, or of scope
  `command` naming the program, grounds in it and is held naming the skill. The reason's text
  after the skill's (another hook's verdict, a rule it quotes) never renames it to a program
  (decision 0094, amended 2026-10-02).
- **The recorded refusal names the skill.** A refusal the policy writes under
  `skills:allowed-tools` gives the skill's name (``... not permitted by skill `NAME`'s
  allowed-tools``), and the hold reads it from there when the worker's skill session is gone.
- **Activation in the same round.** A round's calls are reviewed before any of them runs, so a
  call made in the round that activates a skill is judged by the other policies only. When one
  of them refused it, a later report of a tool the now-active skill refuses is held naming the
  skill.

## Amendment (2026-10-02): a skill that lists a tool no worker has is refused

- **The rule.** When the model activates a skill, every `allowed-tools` entry's tool name
  must match a tool the worker has: a built-in, or an MCP tool of a connected server (the
  scope in parentheses is not part of the name). An entry that matches none refuses the
  activation. The refusal names each unknown entry, says which real tool does the same job
  when the entry is a name coding agents commonly use (`Bash` is `shell`, `Read` is
  `read_file`, `Edit`, `Write` and `write_file` are `apply_patch`, `Grep` and `Glob` are
  `search_files`, `LS` is `list_directory`), and lists the tools the worker has. The worker's
  policy is not narrowed and the skill's instructions are not injected.
- **Why a refusal and not a notice.** An entry that names no tool cannot mean what its
  author wrote. A worker kept under the rest of the list lacks tools the skill's own text
  tells it to use, and in practice reads the "only these tools may be called" reply and stops.
  A refusal reaches the worker in the same call, with the real names, so it can report the
  shortfall or the user can fix the list. Claude Code ignores an unknown name in
  `allowed-tools` because there the list only pre-approves tools and restricts nothing. Here
  the list restricts (above), so an ignored name is a silently wrong restriction, and fail
  loud wins.
- **Where it is checked.** The tool set is known only when the worker environment is
  complete, after MCP servers connect, so the host binds it to the skill session then
  (`SkillSession::bind_tools`). `kgraf instructions scan` does not check names: it does not
  know which MCP servers a worker will have. `report_shortfall` is always a known name. A
  resumed worker re-activates its skills through the same check.

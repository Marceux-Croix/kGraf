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

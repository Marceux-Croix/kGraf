# Decision 0099: `replace_in_file` is an edit tool that needs no patch format

**Status:** Accepted. Built.
**Deciders:** the project maintainer.
**Related:** decisions 0043, 0053, 0084, 0094, 0095

This record exists so the tool is not re-argued. If a future contributor asks "why does a
second edit tool exist when `apply_patch` repairs so much," or "why not tell the model to
write whole files," the reasoning is here.

---

## Decision

1. **Every worker has one more built-in tool, `replace_in_file(path, old, new)`.** It replaces
   exactly one occurrence of `old` in the file at `path` with `new`. There are no line prefixes
   and no context lines. Overlapping occurrences are counted, so `aa` in `aaa` is two.
2. **Zero or several occurrences is an error that gives the count.** For several, it names the
   first line numbers and says to add a neighboring line to `old` and `new`. For none, it
   names the nearest candidate, using the window scoring `apply_patch` uses for its own
   mismatch hints: a line that matches except for whitespace, the first line that disagrees,
   or that nothing resembles `old`.
3. **An empty `old` creates a file that does not exist**, with `new` as its contents. If the file
   exists, the call fails and points to `apply_patch` (Delete File plus Add File). If `old` is
   not empty and the file does not exist, the call fails and says to use an empty `old`.
4. **It writes through the function `apply_patch` writes with**, as an Add File of the whole new
   contents. The workspace confinement, the protected-name guard, the hard-link refusal and the
   write are therefore the same code. The call declares the one written path, so the worker's
   write scope, its fences and the permission rules judge it as they judge a patch header.
5. **Protected paths are refused here, with a pointer to `apply_patch`.** `.git`, kGraf's own
   state (the ledger and the trace included) and the files Bypass admits to `apply_patch`
   (`.gitattributes`, `.gitmodules`, `.githooks`) are refused in every mode. `apply_patch`
   snapshots and checks such a write; one place for that is enough.
6. **The syntax check runs on every edit.** `apply_patch` runs it only after a
   whitespace-tolerant match. This tool matches text exactly, so it has no tolerant match to
   trigger the check, and a replacement made without context is the edit most likely to break
   indentation. The check keeps its rule: it refuses only when the file parsed before and does
   not parse after (Python, YAML).
7. **Permission names.** `replace_in_file` is a built-in tool name, a path tool (a rule's
   pattern matches its `path`), and an edit tool for Accept edits and Auto's path allow-set. A
   rule or allow entry on `apply_patch`, including the Claude Code names read as it (`Edit`,
   `Write`, `MultiEdit`, `NotebookEdit`), also covers `replace_in_file`, so a deny rule on a path
   cannot be sidestepped by the other tool. A rule on `replace_in_file` covers only it. A skill's
   `allowed-tools` treats it as a tool of its own: a list without it refuses it.
8. **Hints point at it.** The whole-file hint after three failed patches on one file, and the
   refusal of a shell command that only writes an existing file, name `replace_in_file` for a
   small edit, when the worker has the tool. The system prompt says "Change files with
   apply_patch, or with replace_in_file for one small edit."

## Context

Mining the worker logs of eight scenario rounds found that 1,970 of 3,677 real-model
`apply_patch` calls failed (54%), and the rate had not moved in 12 rounds. All 21 workers that
stalled ended in a patch-failure loop. 241 attempts had three or more patch failures in a row.
34% of `read_file` calls were a re-read of a file after a failed patch. The repairs of decision
0084 removed failure classes one at a time, and the rate stayed. What the failed calls share is
that the format asks the model for line prefixes and context lines copied exactly, and a model
that gets one wrong tries the same shape again.

## Options considered

- **Only hints.** Longer error text, an example after the second failure and the whole-file
  route after the third (already built, 0084). Rejected as the only measure: the rate did not
  move while those were added, because each hint teaches the same format again.
- **More parser repairs.** Three more are built with this decision (0084, amended 2026-10-12:
  git's no-newline marker, a blank Add File line among `+` lines, a bare Update line read as
  context). Kept, but each removes one class; none removes the need for exact context.
- **Tell the model to write whole files (`write_file`).** A whole-file write has no context to
  match, but it makes the model resend every line of a large file for a one-line change, risks
  dropping lines it did not mean to touch, and needs the same scope, protected-path and syntax
  checks as a second write tool anyway. The whole-file route stays for the case it fits, a file
  that keeps failing, through Delete File plus Add File.
- **A line-number edit tool.** Rejected: line numbers go stale after any other edit and a model
  counts them badly.
- **One tool, as built.** An old-text and new-text replace needs nothing the model must get
  right except text it can copy from the file it read, and its failure says exactly what was
  wrong (a count). It adds one tool, so the prefix changes once; a second tool for creating
  files was not added, since an empty `old` covers it.

## Consequences

- Every worker's tool list and system prompt change, so the shared prefix digest changes once
  (0043). A worker transcript recorded by an earlier build cannot be resumed (as in 0095).
- The syntax check can refuse an exact edit that passes through a state that does not parse. The
  error says so and tells the model to make the whole change in one edit. Reversing point 6
  would mean the check never runs for this tool.
- Whether the tool lowers the failure rate is not known from this record. It is measured in the
  next scenario rounds by the share of edit calls that fail, split by tool, and by the length of
  failure runs. If workers use `replace_in_file` and `apply_patch` the same number of times as
  before, with the same failures, this decision should be revisited.

# Decision 0062: The product is renamed kGraf, and what the earlier name left keeps working

**Status:** Accepted
**Related:** decision 0059 (the ledger's hash chain and external tip); decision 0056 (run-state
records); decision 0061 (the Windows MSI)

This record exists so the name, its spelling rules, and the migration contract are not
re-argued, and so a future contributor knows which occurrences of the product's earlier name
in the codebase are deliberate.

## Decision

The product is **kGraf**: lowercase k, capital G, in prose, titles, window and tray labels, and
installer names. Code and paths are lowercase:

| Was (earlier version) | Is (kGraf) |
|---|---|
| binaries `haustoria`, `haustoria-app` | `kgraf`, `kgraf-app` |
| crates `crates/haustoria-*`, libraries `haustoria_*` | `crates/kgraf-*`, `kgraf_*` |
| project state directory `.haustoria/` | `.kgraf/` |
| environment variables `HAUSTORIA_*` | `KGRAF_*` |
| MCP OAuth keychain service `haustoria-mcp-oauth` | `kgraf-mcp-oauth` |
| settings and log directories `Haustoria` (`haustoria` on Linux) | `kGraf` (`kgraf`) |
| macOS bundle id and app name | `com.marceux-croix.kgraf`, `kGraf.app` |
| Debian package `haustoria` | `kgraf` (with `Replaces`, `Conflicts`, `Provides: haustoria`) |
| worktree lock tag `haustoria:v1`, marker `.haustoria-workspaces` | `kgraf:v1`, `.kgraf-workspaces` |
| attempt refs `refs/haustoria/attempts/*`, integration branches `haustoria/<run>` | `refs/kgraf/...`, `kgraf/<run>` |

**Migration rules.** Every move fails loud and prints one notice.

1. **State directory.** On first use of a project, an existing `.haustoria/` directory with no
   `.kgraf/` already present is renamed to `.kgraf/`. A lock and pending-mark file next to the
   state directory serializes concurrent first uses: every decision is taken under the lock,
   its mark is cleared before it is removed, and a lock taken on a file that has since been
   removed is retried against the file's own identity, so exactly one of many concurrent first
   uses performs the move and the rest find nothing to do. The lock file keeps default file
   sharing on Windows, so its holder can remove it while a waiter still holds it open, and a
   failed removal is reported. A killed move keeps its pending mark and finishes on the next
   use. The lock file is opened without following a symbolic link and must be a regular file.
   The Windows lock path was compiled and its tests built, but not run on real Windows
   hardware, since none was available during development; it remains on the hardware
   verification checklist.
2. **Refusals, all checked before the rename, so a refused move changes nothing:** both
   directories already present; a symbolic link found where the move needs to write or lock (a
   JSON file, a temporary file in the state directory, the graph database, a hold file, or the
   lock file itself), since a repository could ship such a link to aim a write outside the
   project; git missing while the old directory holds registered worktrees, or git failing for
   any reason other than "not a git repository," since a move without its accompanying repairs
   would leave each worktree unregistered at its new path, where startup cleanup could delete it
   along with any uncommitted work; and a process belonging to the earlier version still
   actively working there (holding a live worktree lock or a worktree hold file).
3. **What points into the old path is repaired.** Every moved worktree is repaired through
   git's own worktree-repair mechanism, and a worktree already gone has its registration
   rewritten directly; every worktree's link to its main repository and the reverse
   registration are checked to agree and neither names the old path, and a repair that did not
   take fails and keeps the pending mark. Attempt refs move to the new namespace. Absolute
   paths stored in the graph store's metadata (run records, schedules) and in the state
   directory's own JSON files are rewritten in place, keeping their layout and permissions, and
   only where the old path appears whole, never inside a longer path such as a backup copy's.
   Files are written through a new temporary file created without following a link. Anything
   still naming the old path afterward (an alternate spelling, a relative path) is listed in
   the migration notice. The project's own git-ignore file gains entries for both the new and
   the old state directory names and the migration lock.
4. **The state directories are protected under both names.** The runtime refuses a worker
   write to the current state directory, the earlier one, or the migration lock, under any
   spelling variation (case, Unicode, trailing dots, Windows short names), and the merge-back
   step applies the same rule, so a worker cannot land an old-named state directory in a
   checkout and cause the next start to refuse the project.
5. **History is never rewritten.** The accounting ledger moves as a plain file together with
   its external tip anchor beside it: no row changes, so its hash chain verifies after the move
   exactly as it did before. Run transcripts and traces keep the paths they originally recorded.
   Integration branches belong to the user, may be pushed, and are not renamed; a resumed run
   keeps using its originally named branch.
6. **Compatibility reads.** The new-named environment variable is read first; when only the
   old-named one is set, it is used, with one warning per variable per process naming the new
   variable to use instead; when both are set, the new one wins and the old one is reported as
   ignored. A placeholder referencing the old environment variable name inside an MCP server
   configuration still expands correctly, and the corresponding value is set under both the old
   and new variable names for MCP servers. A logging directive targeting the old-named log
   namespace, with or without a level, is read as the new-named one, with a notice. Locks under
   the former tag are read as belonging to kGraf; a marker under the former name is adopted.
7. **OS-level names.** An authentication token stored under the earlier keychain service name
   is moved on first read, under a per-server lock that every kGraf keychain write also takes,
   so a concurrent token refresh never lands between the move's checks and its copy: the old
   entry is read, the new one is checked again in case a refresh already wrote it (in which case
   the refreshed value wins and the old entry is simply removed), the new value is written and
   read back to confirm, and only then is the old entry deleted; any failure before that point
   keeps the old entry and is reported as an error. A process still running the earlier version
   takes no such lock, so that mixed-version case keeps a narrow window between the re-read and
   the copy. The shell's settings and log directories move on first launch; where both already
   exist, the new one is used and the notice states that the old one is ignored, and a launch
   that loses a race to a concurrent migration is not treated as an error. The Windows installer
   keeps its upgrade identity, so installing kGraf is treated as a major upgrade that removes
   the earlier version's folder, PATH entry, Start Menu folder, and registry value; every
   component identifier was regenerated, since each installed file's key path changed. On Linux
   the window's application id is `kgraf` for both Wayland and X11 window-manager
   identification, and the desktop entry's startup window-class field matches it.

## Context

The product was renamed on 2026-09-27. The first candidate name, "Graf," was rejected: it is
one letter from a popular, unrelated tool in the same category (code graphs for coding agents)
that installs its own similarly named command, its own dotted config folder, its own prefixed
MCP tool names, and its own prefixed environment variables, so a small typo could run the
wrong tool entirely. "kGraf" keeps the graph sense of the name without sitting one keystroke
away from that other tool's names.

A project used under the earlier name holds state that cost real effort to produce: the
accounting ledger, run records, transcripts, and live git worktrees registered by absolute
path. A rename that only changed the directory name in source code would have stranded all of
it.

## Alternatives considered

- **Leave the old names in place indefinitely** alongside the new ones. Rejected: having two
  names forever creates a second place to look for the same thing.
- **Symlink the old state-directory name to the new one.** Rejected: a symbolic link inside a
  repository is itself a sandbox and merge-back hazard, and it would make the "both directories
  exist" refusal condition ambiguous.
- **Rewrite the accounting ledger's stored paths directly.** Rejected: ledger rows are
  hash-chained, so rewriting them would break the chain or require re-signing all of history.
  The ledger holds no path that any lookup actually needs, so it moves unchanged instead.
- **Rename integration branches to match.** Rejected: they belong to the user and may already
  be pushed elsewhere. Lookup is made tolerant of the old branch name instead.
- **Change the domain separator embedded in the Windows sandbox capability identifier.** Kept
  unchanged: it is already part of every capability grant already made on a user's disk, and a
  new one would add a redundant permission entry per project root for no benefit.

## Consequences

Deliberate occurrences of the product's earlier name remain in the codebase: the compatibility
constants and legacy-name handling code and its tests, the packaging compatibility lines, this
record itself, and every historical record and evidence file from before the rename. The
project's GitHub repository keeps its earlier name until it is separately renamed.

**Known limit: an in-flight worker does not resume across the rename.** A worker's system
prompt names the product, so a transcript recorded before the rename has a different prefix
than one recorded after, and the runtime refuses to continue such a transcript rather than
silently continue a conversation the model never actually had. The refusal names the rename and
this record; a prefix snapshot that kept the digest matching the earlier name is recognized as
valid for that purpose. Finish or stop any running work before upgrading across the rename.

The Linux shell now builds its windowing dependency with explicit Wayland and X11 support, so
the application id reaches the window system correctly.

Tests cover the failure cases: a git failure that would skip a repair, a symbolic link in
the old directory, merge-back of the state directory names, and a lock under concurrent
access.

**Known limit: an older process that holds no worktree** (for example, a background indexing
process left over from the earlier version) is not detected before the move happens. On
Unix-like systems it keeps writing through its already-open file handles; on Windows the
rename fails loudly while such a process is running.

macOS system permission grants given to the earlier version's app bundle do not carry over to
the new one, and an already-installed copy of the earlier version's app stays in place beside
the new one until the user removes it manually. The app is not sandboxed, so no container
directory needs to move.

Leftover empty scratch directories under the earlier name are not removed automatically; they
hold nothing once a run ends.

**Reactivation.** A later rename follows the same rules, using this record's compatibility
constants as the model. The compatibility-reading behavior can be removed once no user is
expected to still hold state from before the rename; that removal needs its own record.

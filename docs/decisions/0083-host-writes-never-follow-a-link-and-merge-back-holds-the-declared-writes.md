# Decision 0083: Host writes never follow a symbolic link, and a merge holds an attempt to only what it declared

**Status:** Accepted
**Related:** decisions 0018, 0026, 0028, 0067, 0073

## Context

A security review found that kGraf's own host process, when writing into a project's working
copy on a worker's behalf, would follow a symbolic link out of that working copy and write to
wherever the link pointed, including files entirely outside the project. A link could arrive
in the working copy from the user's own repository or from an earlier worker's own commit, and
kGraf's merge process did not refuse a commit that introduced or changed such a link, nor did
it refuse a commit that touched files outside what it had actually declared it would write.

## Decision

**Every write kGraf's own host process makes into a tree whose contents a worker or a user's
repository controls goes through one shared, hardened path.** Each step of the path, including
the final file itself, is opened without ever following a symbolic link; a link found at any
point is refused outright, naming it, and is never silently replaced or written through.

**A merge refuses any symbolic link an attempt adds, retargets, or turns a plain file into.**
Deleting a link, or leaving one that was already there before the attempt, is still allowed.

**A merge holds an attempt to exactly the files it declared it would write.** A change to any
other path causes the whole attempt to be refused, naming the unexpected path; an attempt
committed with no declared scope at all is refused outright, and two attempts's declared paths
that a real filesystem would treat as the same file (for example, differing only in letter
case) are also refused as a collision.

## Consequences

- A project whose own repository already contains symbolic links continues to work normally:
  a host write that would cross one simply fails loudly, naming the link, without touching
  anything.
- A worker cannot deliver a new symbolic link through the normal merge path at all; only the
  project's owner can add one directly.
- This protection is not yet available on Windows, where the underlying filesystem check this
  relies on does not yet exist; host writes on Windows that would need it fail loudly instead
  of proceeding unprotected.

## Alternatives considered

- **Replace a symbolic link found mid-write with an ordinary file instead of refusing it.**
  Rejected: this would silently rewrite something a worker or the project's own repository
  deliberately placed there; refusing loudly and naming the problem is preferred over a silent
  rewrite.
- **Check a link's target only by inspecting its literal path text, without resolving it fully
  against the filesystem.** Rejected: this check is unsound once more than one link is
  involved, since a chain of links can resolve to a location a purely textual check would
  never notice.
- **Leave declared-write enforcement entirely to the verification step that already runs
  earlier in the pipeline.** Rejected: that earlier check is the first line of defense, but a
  second, independent check inside the merge step itself catches the case where the earlier
  one was skipped or bypassed by mistake.

## Amendment (2026-10-07): the protection now covers Windows

The Windows limitation above is replaced: host writes and removals on Windows no longer fail
with "not supported". Each step of a path is opened by itself, relative to the folder opened
before it, without ever following what is there (`NtCreateFile` with a root directory handle and
`FILE_OPEN_REPARSE_POINT`). A symbolic link, a junction or any other reparse point found at any
step is refused, naming it; a file with more than one hard link is refused; a name Windows
programs could not open by path (a reserved device name, a trailing dot or space, a stream
suffix) is refused. A removal deletes through the handle it opened, so a link or junction is
only ever removed itself, never what it points to.

**Note (2026-10-07).** On Windows the guarantee is that no host write or removal lands outside
what the worker can already write, not that it never leaves the tree. A worker may delete in
its own tree, so it can move a folder the host holds open into a place it can write (its own
temporary folder); a write through that open folder then lands there, which the worker could
have written itself. It never lands where the worker cannot write.

The walk also sets `OBJ_DONT_REPARSE` on every open, so a held folder turned into a junction in
place is never resolved through; a Windows release that rejects the flag gets an error naming
it, never an open without it. Each name opened must be the long name of what it reached, other
than by case, so a short (8.3) name of a protected file is refused. Folders on the way to a
removal are opened to list and enter only, so another process's current folder on the path does
not fail the removal; the empty-parent clean-up checks it deletes the same folder it walked
through. A read-only file with more than one hard link is never changed to delete it.

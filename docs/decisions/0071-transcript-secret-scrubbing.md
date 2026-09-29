

## Amendment (2026-09-29): a cleared value leaves the write-ahead log

Clearing a stored record asks the database to truncate its write-ahead log without waiting for
readers, since a waiting checkpoint stalled every writer while a viewer read. With a long-lived
reader that left the cleared value in the log until the last connection closed. Now, when the
non-waiting attempt is blocked, one background thread per store retries it every 200 ms for at
most 60 seconds. A failed retry returns at once, so no attempt holds the writer lock. A reader
that outlasts the bound is written to the local diagnostics file (code KG-0208). The waiting
checkpoint at the end of a run stays. Removal from the files remains best effort, delayed only
by a reader.

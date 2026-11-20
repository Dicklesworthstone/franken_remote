# Folder synchronization transaction engine

`fr_files::sync` implements bounded local reconciliation and ATP-verified,
no-overwrite publication of regular files. It emits validated `SyncJobState`
records. It is not yet connected to the shipping host/client file channel:
this API and its local filesystem tests do not establish remote folder sync,
reconnect resumption, or live tailnet qualification. Plan §15.6 and bead
`fr-p4-folder-sync-p6i` continue to own that integration.

## Source and destination custody

Each round verifies that the supplied root paths still identify the original
`DropDirectory` owners. Directory enumeration, hashing, transfer reads and
publication then use those retained descriptors. Replacing a pathname cannot
redirect either side of the round. A changed root is refused before transfers.

The scanner bounds inspected directory entries before collecting names,
including private staging entries. It checks authority during enumeration and
before and after each bounded hashing read. Regular-file opens are nonblocking
and do not follow symlinks, so replacing an entry with a FIFO cannot strand the
disk owner waiting for a writer. File identity, size and modification metadata
remain tied to the scanned manifest; growth, replacement and changed contents
are refused before publication, including an explicit EOF check.

The complete plan also projects each destination's resulting entry count and
regular-file bytes before the first publication. Private staging names consume
entry capacity. A successful unchanged round cannot grow a destination beyond
the same limits that its next scan will enforce.

`scan_directory_checked` exposes the same bounded cancellation checks to
callers that only need a manifest. `scan_directory` is the local convenience
form without session authority. Disk operations still belong on a supervised
disk worker; callbacks do not make an OS filesystem call interruptible.

Directory scanning and round execution currently require Unix descriptor
operations. Other targets return `UnsupportedPlatform` before I/O. Windows
regular-file receiving through its existing retained-handle owner remains
separate; there is no pathname-based folder-sync fallback.

## Committed results and repeated conflicts

A failed round returns every already-committed publication in
`SyncRoundFailure.published`, including its durability result. Cancellation
removes unfinished staging; it does not roll back files already published.
Receipts and terminal progress counters describe the same committed prefix.
The failure also returns the baseline advanced by those publications, so a
newly authorized retry can finish the remaining work without duplicating a
conflict copy committed before cancellation.

`SyncRoundFailure.cleanup_failed` separately preserves failed staging cleanup
or reservation accounting. Such a result retains the original stop cause and
committed receipts but emits `Failed` / `UnknownEffect`, never a clean cancelled
result. Any original worker cleanup witness remains attached. Public local
callers share a witness retained by the original directory owner, so creating
another round or clone does not replace a known failed witness. Retained quota
is not released by reporting cancellation.

The returned baseline records each version delivered under a keep-both name.
The next round neither generates another random copy of the same version nor
echoes the conflict copy back to its original source. A new source revision
still transfers once, and deleting or editing a received copy does not replay
the previously acknowledged version. Baseline entries are pruned when their
names disappear from both snapshots; kept-version history is bounded by the
current source names. Callers retain a baseline per configured job.

## Regression coverage

`cargo test -p fr-files --test folder_sync --locked` exercises real local
filesystems: two-way transfer, repeated concurrent edits, local deletion,
partial-publication receipts, cancellation during hashing, scan bounds,
descriptor-pinned root replacement, source growth, FIFO refusal, unsafe
entries and sanitized debug output. These checks do not substitute for the
remaining remote runtime and platform acceptance gates.

`folder_sync_limits` covers both projected bounds, private staging and exact
boundary convergence. The receive library test also plants an actual poisoned
reservation during cancellation, checks retained publication receipts and
cleanup evidence, and refuses a new round under that failed original owner.

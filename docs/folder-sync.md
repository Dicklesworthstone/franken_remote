# Folder synchronization

`fr_files::sync::live` connects an explicitly configured host folder to one
explicitly accepted native controller download. It repeatedly delivers new
source versions through the existing ATP sender, receiver and disk worker.
`fr_files::sync` also retains the local reconciliation API described below.
Plan §15.6 and bead `fr-p4-folder-sync-p6i` own the complete feature; the native
one-way integration is tracked separately by `fr-p4-folder-sync-p6i.1`.

## Run one explicit native job

On a Linux or macOS host, add these options to the existing configured host
command (retain its normal capture, policy, worker and tailnet options):

```sh
frd run --once --input-agent /absolute/fr-input-agent \
  --sync-source /private/source --sync-job 41
```

On the controlling native client, explicitly choose the matching job and a
private local destination:

```sh
fr connect NODE_ID --control --experimental-native --display only \
  --receive /private/destination --receive-conflict keep-both --sync-job 41
```

Both roots must pass the existing descriptor-pinned directory checks. The
positive job ID is configured locally at each endpoint; a peer cannot create a
host job or supply a host path. Ordinary file-receive consent never accepts a
folder job. The host source excludes `--send` and `--send-control`; independent
controller uploads can still coexist on a positively negotiated duplex lane.

The host advertises only its configured ID. Scanning starts after matching
controller acceptance on the original granted download. The optional
`file-folder-sync` capability is selected by both endpoints; older or
unselected peers receive no fallback folder transfer. `SyncJobState` records
share the existing bounded file queue, byte accounting and original deadlines.
They carry progress and scope, never local paths or file contents.

The fullest combination of folder sync, duplex file transfer, clipboard and
audio playback uses all 24 negotiated capability slots. For this explicit
combination, optional decoder-load reporting yields its slot to synchronization;
source, transport and capture-credit pacing continue. Requested audio,
clipboard, both file directions and required control/privacy capabilities stay
selected. A peer that requires decoder-load reporting cannot use this fallback.

### Bounds and delivery semantics

The live job scans at five-second intervals after the preceding round has
settled. Each scan allows at most 32 flat regular files, 64 MiB per file and
256 MiB of source content. Subdirectories, symlinks and special files refuse the
job. Across all rounds, the live job allows 64 attempted file versions and
1 GiB of attempted content; the receiver's independently selected lower limits
still apply. The controller charges each announced round against those job
limits before accepting its file offers. Each decoded ATP offer must also fit
the remaining bytes of the round and the per-file ceiling before the receiver
can queue a staging operation. Scanning uses one supervised worker and one bounded result. Each
round has a five-minute deadline, and each retained state record keeps its
original one-second delivery deadline, capped by the active round. The
controller starts its own monotonic round timer on the admitted scanning
record. That deadline also fences its disk worker at publication and remains
active until the settlement acknowledgment drains. A round does not start while prior ATP,
publication proof, source cleanup or job-state custody remains outstanding.

Only actual verified publication receipts advance the source-version ledger.
A file changed between scanning and source preparation is refused before its
offer, so a different version cannot be credited to the scanned manifest.
Occupied destination names always preserve both versions. Editing or removing
a received copy does not replay a previously acknowledged source version; a
later host change remains eligible for a new delivery. `Settled` confirms the
round's deliveries, rather than asserting equality of the two directories.

Cancellation, lost authority, expiry or disconnect ends the original job.
Already published files remain published, with their durability and cleanup
results retained separately; unfinished staging is cancelled under the existing
worker owner. State acknowledgements wait for disk/proof/network drain and
never turn a failed cleanup or uncertain durability result into success.
Neither the job, its accepted destination nor its ledger is replayed by a
reconnecting viewer. Start a new explicit job to authorize another session.

This path is one-way host-to-controller delivery of flat regular files.
Bidirectional remote reconciliation, recursive jobs, persistent jobs across
reconnects and native desktop/live-tailnet qualification remain separate work
under the parent bead. The existing local two-way engine is not evidence that
those remote features are complete.

## Local reconciliation engine

The local API performs bounded reconciliation and ATP-verified, no-overwrite
publication with both directory owners in one process. Its local filesystem
tests establish that scope independently of the native transport tests.

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

`cargo test -p fr-files --test remote_folder_sync --locked` drives the real
native UDP/TLS file attachment and independent source/destination disk owners.
The target includes repeated changes, a full 32-file first round followed by
another version, keep-both preservation, local edit/delete without replay,
wrong job IDs, replacement connections, cancellation cleanup and source mutation
between scan and preparation. Production daemon/viewer regressions additionally
exercise repeated jobs while input continues and refuse ordinary download
consent before consuming a host's shared job selection. Current execution and
platform qualification results are recorded in
[implementation status](../IMPLEMENTATION_STATUS.md#explicit-native-one-way-folder-jobs-2026-11-23).

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

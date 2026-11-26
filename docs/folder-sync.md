# Folder synchronization

`fr_files::sync::live` connects an explicitly configured host folder to one
explicitly accepted native controller source or destination. One-way jobs can
send in either direction. They repeatedly deliver new source versions through
the existing ATP sender, receiver and disk worker.
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

The Linux and macOS native app can accept the same job through its normal
machine picker:

```sh
fr app --receive /private/destination --receive-conflict keep-both --sync-job 41
```

The picker notice names the job and explains that new source versions will be
received throughout this connection. Choose **View and control**, then
**Receive files**. The app consumes this explicit job and destination before
launching the selected viewer, including a declined choice or failed launch;
neither is saved or reused by another connection. Without `--sync-job`, ordinary
receiving consent does not accept a folder job.
When the viewer closes, the app reports the original job's settled rounds,
publications and stop reason alongside the file receipts. A mismatched job or
unconfirmed receiver cleanup stops the app instead of opening another picker.

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

### Send a controller folder to the host

On a Linux or macOS host, explicitly choose the receiving directory and its
conflict policy for one controlled share:

```sh
frd run --once --input-agent /absolute/fr-input-agent \
  --sync-destination /private/host-destination --sync-job 41 \
  --files-conflict keep-both
```

On the native Linux or macOS controller, accept that ID with a private source:

```sh
fr connect NODE_ID --control --experimental-native --display only \
  --sync-source /private/controller-source --sync-job 41
```

The host advertises its destination job; the controller accepts it before
scanning. The controller never advertises a host job or sends a host path.
Both peers must select the independent `file-folder-sync-upload` capability
on the original upload attachment. Flat jobs use v1 and tree jobs use v2. The older download-only capability cannot
enable this direction, and an unselected peer receives no ordinary-file
fallback. The same bounded source owner, cache, ATP sender and tracked KeepBoth
receiver handle both directions.

This first reverse native job exclusively owns its file direction and the
client's original authority projection. Do not combine the controller source
with ordinary sends, file pickers, drops, local file commands, or downloads.
The host destination excludes `--files`, `--send`, `--send-control`, and
`--sync-source`. These combinations are refused before source admission; they
do not silently discard a selection. Directory clones share one-use job consent,
and a consumed job cannot fall back to ordinary receiving on another session.
This does not yet implement a bidirectional reconciliation job.

### Receive nested directories

On both endpoints, replace `--sync-job 41` with `--sync-tree-job 41` to include
subdirectories. For example, choose the following folder options on the host:

```sh
--sync-source /private/source --sync-tree-job 41
```

On the controlling viewer, select a destination for the same job:

```sh
--receive /private/destination --receive-conflict keep-both --sync-tree-job 41
```

The same option works for controller-to-host jobs and for receiving through
`fr app`. A tree job requires version 2 of the capability for its direction.
A flat selection, a different job ID, or a peer without a matching capability
cannot start tree transfer. A changed top-level directory, including an empty
directory, is delivered as a complete version. KeepBoth preserves an existing
destination under the normal conflict policy. Each inspected descendant
counts toward the source entry limit. Flat jobs keep their original scope.

### Bounds and delivery semantics

The live job scans at five-second intervals once all currently observed changes
have been delivered. Deferred pages start another bounded round immediately
after the preceding page settles, and recheck the source under current authority.
Each scan allows at most 256 directory entries (including private staging
entries), 64 MiB per selected item and 256 MiB of source content. Flat jobs
refuse subdirectories; all jobs refuse symlinks and special files. A delivery round selects at most
32 changed files, so a larger directory progresses through bounded pages without
retaining a larger batch of open sources. Selection continues after the last
name in the preceding settled page and wraps once; frequently edited early names
cannot continually displace later files. The cursor advances only after the
original page's publication and cleanup are confirmed and its settlement arrives.

Across all rounds, an explicitly accepted native folder job allows 512 attempted
file versions and 1 GiB of attempted content. This leaves room to deliver the full
256-entry source and subsequent changes. Neither paging nor an empty round resets
these cumulative budgets. Ordinary native file transfers retain their 64-attempt
limit. Folder receivers retain at most 513 content-free results, including the
first terminal refusal; ordinary receivers retain their existing 65-result bound.
The receiver's independently selected lower limits still apply, including those
of older peers; exceeding them is a typed resource refusal, never a quota reset.
The receiver charges each announced round against those job limits before
accepting its file offers. Each decoded ATP offer must also fit
the remaining bytes of the round and the per-file ceiling before the receiver
can queue a staging operation. Scanning uses one supervised worker and one bounded result. Each
round has a five-minute deadline, and each retained state record keeps its
original one-second delivery deadline, capped by the active round. The
receiver starts its own monotonic round timer on the admitted scanning
record. That deadline also fences its disk worker at publication and remains
active until the settlement acknowledgment drains. A round does not start while prior ATP,
publication proof, source cleanup or job-state custody remains outstanding.

A source edited or removed during the pre-transfer scan can be scanned again
within the same round. Premature EOF while hashing a selected file or recursive
tree is classified as a changed source; permission and other I/O errors retain
their original failure. A failed hash cannot publish a partial manifest. The source owner makes at most three scan attempts, separated by
at least 100 ms, after joining each preceding worker. Retries retain the same
job, manifest revision, pinned root, authority and original five-minute deadline.
They neither announce another round nor spend transfer quota. Cancellation,
unsafe entries, permission failures and other faults remain terminal. Continuous
source changes exhaust the bounded attempts and report the last source failure.
Once the transfer manifest has been announced, a changed source remains a
terminal refusal; uncertain or published transfers are never replayed by this
scan retry.

Flat live jobs keep a bounded manifest cache inside the original source owner.
Every scan still enumerates the pinned directory, checks authority and resource
limits, and opens each regular file without following links. Unchanged files
with matching device, inode, size, modification time and change time can reuse
their preceding digests even when another file changes. Changed, new, empty or
coarse-timestamp files use the existing ATP byte/EOF hashing path. Two checked
opens retain the first descriptor through the authority callbacks; uncertain
identity takes the canonical read path. Repeated names from a changing directory
refuse the snapshot instead of confusing its keyed manifest and digest list.

Each partial scan constructs a fresh ordered manifest from the current names
and counts the full size of reused files against the current source limits.
It retains ATP's exact per-entry digests, including content identifiers, so the
combined Merkle calculation uses the same canonical inputs as a full scan.
Deleted or renamed entries cannot accumulate in the cache.

At most three consecutive scans may reuse any digest. The next scan hashes all
content again, including when no versions need delivery or a small sibling
changes on every scan. A partial edit never restarts this verification age.
A scan that reads every file can reset the age because no old digest survives.
Changing the pinned root or limits forces a full scan. Failure, cancellation
or original-job closure discards the cache.

The cache never becomes publication evidence: the sender independently verifies
each selected file against its expected digest. Only confirmed publication and
cleanup advance the version ledger. This reduces repeated source reads for
mixed changed/unchanged directories; it does not make a filesystem snapshot
atomic or establish a measured throughput improvement.

Only actual verified publication receipts advance the source-version ledger.
A file changed between scanning and source preparation is refused before its
offer, so a different version cannot be credited to the scanned manifest.
Occupied destination names always preserve both versions. Editing or removing
a received copy does not replay a previously acknowledged source version; a
later source change remains eligible for a new delivery. `Settled` confirms the
round's deliveries, rather than asserting equality of the two directories.

Cancellation, lost authority, expiry or disconnect ends the original job.
Already published files remain published, with their durability and cleanup
results retained separately; unfinished staging is cancelled under the existing
worker owner. State acknowledgements wait for disk/proof/network drain and
never turn a failed cleanup or uncertain durability result into success.
Neither the job, its accepted destination nor its ledger is replayed by a
reconnecting viewer. Start a new explicit job to authorize another session.

Native jobs select one direction for delivery. Bidirectional remote
reconciliation, persistent jobs across reconnects and native desktop/live-tailnet
qualification remain separate work under the parent bead. The existing local
two-way engine is not evidence that those remote features are complete.

### Recursive atomic directory versions

The live source and destination APIs expose explicit `with_recursive()`
selection. Both owners must make that choice and positively negotiate version
2 of their direction's existing sync capability; a flat selection never gains
recursive authority from its peer. Version 1 retains the existing flat behavior.
Version 2 uses the existing ATP directory profiles without another capability
slot or any change to the job-state record.

Each top-level directory becomes one versioned item. Its complete contents,
including empty files and empty directories, travel through the ordinary ATP
tree sender and publish atomically. For example, an initial `project/` and a
later changed `project/` produce two complete trees under the existing KeepBoth
conflict naming policy. The second delivery does not overwrite or merge the
first, and source deletions do not delete destination data. Top-level regular
files can coexist with these trees in the same bounded delivery page.

The 64 MiB item ceiling applies to the whole selected subtree, and the 256 MiB
scan ceiling includes every subtree's content. The 256-entry scan bound counts
top-level names (including skipped private staging names) and all admitted
subtree descendants. Each individual tree also retains ATP's existing limits
of 64 manifest leaves (files or empty directories), 128 descendant nodes and
depth 8; reserved staging names inside a tree are refused. A page still contains at most 32
top-level items; the original session still allows at most 512 attempted
versions and 1 GiB. A subtree is refused before content beyond its remaining
byte or node budget is read.

Recursive scans hash the actual descendants on every scan; a parent directory's
timestamps do not establish that its child content is unchanged. The version
includes file/tree kind, content size, canonical ATP Merkle root and structural
metadata commitment. This detects an empty file becoming an empty directory
as well as ordinary content changes. Sender preparation independently verifies
that exact version before sending an offer and retains the scanned size as its
read ceiling, so a changed source cannot expand a previously admitted budget.
Only confirmed atomic publication and cleanup advance the version ledger.
Cancellation preserves already published items and removes unfinished private
staging under the original job's owner and deadline.

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

A completed rename with an unconfirmed directory sync returns
`SyncError::DurabilityUnknown` and a terminal `Failed` / `UnknownEffect` state.
The failure retains that file's exact `Publication::DurabilityUnknown` receipt,
the durable prefix and their completed-byte counts. Later files in the same
round are not published. The returned baseline also retains a directional
no-replay receipt for this version, even when its name did not change. A later
explicitly authorized round can deliver remaining files, but editing or removing
the uncertain destination does not resend the original source version.
A genuinely changed source version remains eligible.

`SyncRoundFailure.cleanup_failed` separately preserves failed staging cleanup
or reservation accounting. Such a result retains the original stop cause and
committed receipts but emits `Failed` / `UnknownEffect`, never a clean cancelled
result. Any original worker cleanup witness remains attached. Public local
callers share a witness retained by the original directory owner, so creating
another round or clone does not replace a known failed witness. Retained quota
is not released by reporting cancellation.

The returned baseline records each version delivered under a keep-both name
or with unconfirmed publication durability.
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
another version, larger sources delivered across bounded pages in both directions,
fair progress past newly edited early names, cancellation between pages, refusal
of oversized sources before any offer, keep-both preservation, local edit/delete
without replay,
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
The `receive::publication_tests::sync_durability` library tests exercise both
transfer directions and conflict modes with a per-root, test-only fault at the
post-rename directory-sync boundary. They retain the actual filesystem
publication and validate partial results, authority refusal and no replay after
local edits or deletion. This injected fault is not failing-storage hardware
qualification; execution status is recorded separately.

The sender's `truncation_during_snapshot_hashing_is_a_retryable_source_change`
regression truncates real selected files and tree payloads after the first
bounded content read, checks the typed source-change result, then scans the
settled shorter version. It is source-reviewed but unexecuted as of
2026-11-26 because the local executor disconnected; this is not a passing
runtime or live-tailnet result.

### Reading the native CLI receive report

On Linux and macOS, the receive report for an attached folder job includes a
`folder_job` object. Its `id`, `phase` and `revision` identify the job and
its latest recorded manifest state. `rounds_settled` counts rounds whose
acknowledgement drained. A settled round can contain zero changed files.
`confirmed_publications` counts files actually published across all rounds,
including a file whose durability could not subsequently be confirmed.

A stopped job retains those counts. Read `failure` for the original reason
(for example, `sync_cancelled` or `sync_durability_unconfirmed`) and
`cleanup` for the receiver worker's join outcome. A cancelled job can report
`cleanup: "finished"` after the worker exits with its earlier publications
intact. Ordinary file downloads omit the `folder_job` member.

For a controller-to-host job, the native CLI upload report includes
`upload_folder_job`. It carries the job ID, phase, revision, settled-round count,
confirmed publication/conflict counts, original failure, and source cleanup
status. An empty round can settle without a transfer. These cumulative counts
are independent of the ordinary finite-batch file list. A terminal job retains
already confirmed effects; `cleanup: "unconfirmed"` never claims worker cleanup
succeeded. The host receiver separately retains its publication receipts.

The reverse capability and native offer builders have passed the pinned
`fr-wire --all-targets` test suite (350 tests, including five new upload
capability regressions) and `fr-client --test native_profile` (29 tests,
including both new upload/duplex profile regressions). Workspace
`cargo fmt --check` also passes. The transport, filesystem and native runtime
regressions have been source reviewed but remain unexecuted: the broader
check stopped with compiler SIGKILL inside the unchanged pinned Asupersync
dependency, before reaching those crates. No new live hardware or tailnet
qualification is claimed.

# Native file downloads

A controlling Linux viewer can receive files and portable directories explicitly
selected by the host operator. The transfer uses the existing ATP profiles on
the original session's one-use Files attachment.

## Select a source and destination

On a host already configured for native control, add `--once` and one or more
`--send` selections:

```bash
frd run --software-explicit --display :0 \
  --input-agent /opt/fr/fr-input-agent --once \
  --send /home/alice/report.pdf --send /home/alice/project --json
```

On the client, select an existing private destination and request control:

```bash
fr connect NODE_ID --control --experimental-native --display only \
  --receive /home/bob/received --json
```

The existing tailnet, ingress, executable-trust and display prerequisites in
[README.md](README.md) still apply. The destination must be absolute, owned by
the receiving process's effective user, not writable by other users, and reached
without symlinks. It is pinned by descriptor, so changing the path after startup
does not redirect publication.

Up to eight explicit regular-file or directory selections run in order. Host
selection pins no-follow descriptors before connecting; content reads, directory
traversal and hashing start only after the controlling grant and authenticated
Files attachment. Sources use portable basenames; the viewer cannot browse the
host filesystem or request another source. Empty files, empty roots and nested
empty directories are preserved. Trees that need explicit directory entries
require matching ATP profile-3 acceptance; an older peer refuses that selection
instead of receiving a tree with missing folders. Directory profile limits are in
[PROTOCOL_FILES.md](PROTOCOL_FILES.md).

`--once` is required for a host-selected batch. Cloned host configurations share
one consumable selection. An interrupted session cannot silently send that batch
again to a subsequent viewer. Use a new explicit host invocation to make another
attempt after reviewing the previous result.

The one Files lane has one direction per connection. Host `--send` and `--files`
are mutually exclusive; client `--receive` and `--send` are mutually exclusive.
Clipboard and playback audio keep their own independent opt-ins.

## Choose download limits and conflict behavior

The receiving client owns these choices before it connects. By default it admits
at most **256 MiB per selection** and **1 GiB of declared content per session**.
A selected directory counts as one object, with all of its file content charged
to both limits. To accept larger selections within the existing transfer bounds:

```bash
fr connect NODE_ID --control --experimental-native --display only \
  --receive /home/bob/received \
  --receive-max-file-bytes 2147483648 \
  --receive-max-session-bytes 4294967296 \
  --receive-conflict keep-both --json
```

The numbers are positive byte counts. Omitted values use the native receiver's
defaults; the effective session limit must be at least the effective object
limit. Invalid values and settings without `--receive` refuse before connection.
The per-object limit is checked before staging, and admitted attempts consume
the cumulative session budget even if later cancelled or refused. These options
do not raise queue, chunk, concurrency, rate, source-selection or deadline bounds.

`--receive-conflict reject` is the default. `keep-both` atomically publishes a
colliding file or **whole directory tree** under a bounded `.fr-conflict-` name.
It preserves the existing entry, including directories and symbolic links, and
never merges trees or overwrites a name. A successful renamed publication lets
the remaining explicit selections continue. The finite conflict-name search can
still refuse if all candidates are occupied. Names and paths remain private;
JSON reports actual publication by index, bytes and durability.

## Watch or cancel a download

An accepted `--receive` configuration opens an optional **Download progress**
panel beside the Linux X11 viewer when there is enough room. Its bar shows
bytes privately staged for the current object. Separate counts report actual
publications, durability uncertainty, refusals and uncertain outcomes; cleanup
has its own status. A full bar does not prove that an object was published, and
even an empty object needs a publication receipt.

**Cancel download** requests cancellation through this attempt's original receive
owner. It does not grant another transfer, resume an interrupted item or undo an
earlier publication. Cancellation and cleanup remain subject to the original
transport and disk-owner rules below. Downloads have no pause/resume buttons.

Closing the panel only hides progress; the transfer and desktop continue. The
panel does not take keyboard focus or change the viewer's size or input mapping.
If there is no safe space beside the viewer, or the optional native UI fails,
the accepted transfer remains configured and its terminal results remain
available. A hidden panel is not automatically reopened. See
[native transfer progress](NATIVE_FILE_PROGRESS.md) for the display and input
bounds.

## Authority, publication and results

Both peers must positively select `file-download` version 1. This direction
capability replaces the legacy upload scope in native offers. A source-only
host never advertises an upload directory. Older peers can still negotiate
desktop control and report that download support was not selected.

The host's source owner retains the original controlling input authority.
The client's disk worker holds an opaque authority from its actual
decoder-backed input owner, with a weak lifetime reference and conservative
conversion from the client clock to the original host clock. Original lease,
observation, bounded view-suspension and file-permission checks apply before disk
work and final publication. Numeric session IDs alone cannot create this owner.

The destination stages privately, verifies the ATP content commitments, and
publishes with the existing atomic no-replace operation. A conflict preserves
the existing destination, either refusing or keeping the new object under a
conflict name according to the client's local policy. No received item is
automatically opened or executed.
The sender reports `ControllerPublished` only after validating the receiver's
actual completion proof. Durable publication, uncertain durability, refusal,
interruption before a completion request, and unknown publication remain
distinct outcomes.

The client retains bounded terminal receipts for successive selections;
intermediate Begun/Written progress cannot replace an earlier completion.
Names, paths and content never enter completion diagnostics. Local cancellation
can retire an established file lane while desktop input and renewal continue.
Cancellation requested before attachment completes settles that original
exchange and starts no client disk worker.

Both endpoints retain their original disk owners through teardown.
`reap_files` uses an independently live cleanup context and its original absolute
deadline. Expired or abandoned cleanup does not imply rollback or a joined
worker; actual publication receipts remain readable after session closure.
An ordinary permission or lease fence can finish cleanly. Cleanup success
requires the observed join and final cleanup of every owned staging reservation;
failed removal or unreleased accounting remains a cleanup failure. Those results
are independent of a transfer's publication or refusal receipt.

## Read the command result

With `--json`, the terminal session result includes the following download
fields. Inspect these per-object outcomes even when the desktop session itself
ended normally.

| Field | Meaning |
| --- | --- |
| `files_received` | Actual local publications, each with a zero-based `index`, `bytes` and `durable`. A zero-byte object still needs a publication receipt. `durable: false` retains the publication with unconfirmed crash durability. |
| `files_receive_refused` | Indexed refusals or uncertain outcomes with static reason codes, such as `receive_conflict`, `receive_integrity`, `receive_permission` or `publication_unknown`. |
| `files_receive_absence` / `files_receive_ended` | A reason receipt never started, or a retained reason the started receive owner ended. For example, `host_download_unavailable` means the host did not select the download capability. |
| `files_receive_cleanup` | `not_started`, `pending`, `finished` or `failed` describes the disk owner's cleanup. It does not undo a publication. |
| `files_receive_progress` | The last staged-byte sample, if any. Staging progress does not establish publication, even when the staged and total byte counts match. |
| `files_receive_results_truncated` | Additional result evidence was omitted from the bounded report. A missing index cannot establish that the selection had no effect. |
| `files_receive_cancel_requested` | A local cancellation was requested; use the publication and cleanup fields to determine its outcome. |

For example, this excerpt records a durable first selection followed by a
conflicting second selection:

```json
{
  "files_receive_requested": true,
  "files_received": [{"index": 0, "bytes": 42, "durable": true}],
  "files_receive_refused": [{"index": 1, "reason": "receive_conflict"}],
  "files_receive_cleanup": "finished",
  "files_receive_results_truncated": false
}
```

The first publication remains available and the conflicting destination is
preserved. The batch stops on that failure; later selections do not start.
Session cancellation, host revocation and later cleanup errors preserve earlier
publication receipts. Review these results before explicitly selecting a new
batch, because interruption and unknown publication never authorize an automatic
retry.

## Implementation and coverage

The running-controller integration lives in
`session_startup/running/controlled/files/download.rs` and
`session_startup/viewer/controlled/files/download.rs`. Native selection and
content-free status are in `frd::native_files`; the shipped commands use those
same owners.

Regression targets cover real ATP, TLS/UDP, filesystem publication and both
controller loops, plus the isolated native CLI harness in
`crates/frd/tests/native_host_linux_serial/real_downloads.rs`. Their synthetic
admission, Xvfb and input-fixture boundaries do not establish physical-desktop or
live-tailnet qualification. The receive-policy cases cover default conflict
refusal, preserving colliding files and complete trees with keep-both, and
per-object and cumulative byte limits while the original controller remains
usable. Parser tests reject orphaned settings, overflow, zero, duplicate flags,
and attempts to select overwrite/merge/resume. Downloads use full-object transfer; reconnect
resumption, synchronization jobs, graphical pickers and drag-and-drop remain
separate work.

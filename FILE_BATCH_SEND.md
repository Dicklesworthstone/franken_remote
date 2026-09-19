# Bounded multi-file sending

`fr_files::sender::Sender::begin_batch` accepts a `batch::Selection` of already
locally selected descriptors and portable destination basenames. The selection
holds at most 32 descriptors and 8,160 basename bytes. Empty selections, duplicate
basenames, invalid names and overflowing transfer-ID reservations refuse before
any source starts. Selection never opens a path or reads metadata/content.

The existing sender, original completed file lane and monotonically increasing
transfer IDs are retained. Ordinary `service` checks live authorization before
starting at most one selected source per turn. It starts the next only after a
verified durable host publication AND successful cleanup of the previous source.
The original host's cumulative transfer/byte quotas are not reset between files.
No second connection, runtime, bulk lane or wire profile is created.

One absolute batch deadline starts at `begin_batch`, including time before the
first poll and waiting for earlier files. Each disk source receives the earlier
of that deadline and its existing per-file deadline. Renewals, earlier successes
and backpressure do not extend it. The usual independent cleanup still retains
an unfinished original worker; a blocked kernel read is not claimed killed.

`batch_report` exposes content-free receipts in original selection order, the
number of started/unstarted selections, completion and the first stop reason.
Names, paths and file contents are absent. Receipt collection does not imply
atomic publication of the entire batch: earlier files stay published after a
later conflict, cancellation, expiry, source failure or lost proof. Missing proof
is `PublicationUnknown`, not no-effect or permission to retry. Unknown durability
also stops remaining files. No batch automatically resubmits an object.

`cancel` drops unstarted descriptors and stops the original source. Cleanup can
settle its report without a live connection. A report is complete only after the
source's successful cleanup; a source panic remains a cleanup failure. A completed
report is unchanged by subsequent parent cleanup. Single-file result collection
cannot steal a batch receipt. `take_batch_report` releases a completed report;
only a still-live original lane can then accept another explicit selection.

Eleven new tests use actual UDP/TLS, ATP hashing, selected descriptors and real
filesystem publication, alongside the existing native transfer tests. They cover
120,007-byte files and an empty file, queued path replacement, bounds, conflicts,
pre-poll expiry, authority denial, cancellation, source refusal, a lost proof after
real publication, the original deadline across files, and shutdown receipt
retention. All 89 file-crate integration tests pass. The first-party core, wire,
transport and file libraries were rebuilt with pinned nightly-2026-08-31 against
unchanged checksum-verified, compiler/lock-matched upstream CI libraries from
run 35461102442. Strict Clippy passes for the changed library and native tests.
This is not a cold Cargo/full-workspace, live-tailnet or graphical-picker pass.

This explicit send selection is not directory synchronization, resumption,
downloads, a new permission grant, or an implemented command-line file picker.

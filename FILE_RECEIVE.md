# Confined ATP object materialization

`fr-files` implements the Linux storage boundary for plan §15.6 and
`fr-p2-file-transfer-m7o`. It is not a listener, a new transfer protocol, or an
advertised file-transfer capability. Network attachment, send-side selection,
reconnection/resumption, and the native file picker remain unimplemented here.

A locally selected `DropDirectory` pins directory descriptors without following
symlinks, rejects foreign-writable destinations, and shares active-byte and
transfer-count reservations across clones. A peer supplies only one portable
filename, a declared size, and the pinned Asupersync 0.5.0 ATP `ContentId`.
Names are checked by ATP's portable-path validator. `ContentId::streaming`
verifies the object incrementally; no alternate hash or repair algorithm exists.

`PendingFile` accepts sequential chunks of at most 64 KiB, writes a private
0600 temporary file, and rejects overflow, replay, overrun, truncation, and
integrity mismatch. Errors are terminal. `verify` flushes the complete file;
`publish` is a separate atomic no-replace rename so the eventual session worker
can recheck its original authority after slow disk work. Existing files,
directories, and symlinks are never overwritten. Conflicts return `Conflict`;
a later keep-both naming policy must be explicit, not an automatic retry.

Publication followed by a directory-sync failure returns `DurabilityUnknown`,
not a no-effect refusal. Cancellation removes only the private staging name.
A failed cleanup retains its budget charge. Process death can leave a private
staging file requiring local cleanup; it cannot expose a partial destination.
Completed files are not charged to the active-transfer reservation budget.
The selected desktop user and OS remain trusted, as in `SECURITY.md`.

All disk methods must run on an owned disk worker, outside reactor/authority
threads. This storage API alone does not grant observation, control, or files.

Dependencies add direct use of already locked `getrandom` 0.4.3 (random staging
names) and Linux-only `rustix` 1.1.4 (`std`, `fs`, `process`; descriptor-relative
operations). No extra runtime, native library, or newly resolved package is added.

The ten real-filesystem regressions cover multi-chunk/empty objects, path attacks,
integrity and offset failures, racing destinations, root replacement, quotas,
cancellation, and sanitized diagnostics. They pass with nightly-2026-08-31 and
real compiled dependencies retained by repository Actions run 35351324850;
this focused execution is not a full-workspace or live-tailnet qualification.

Strict pedantic Clippy passes for the new library and tests with those same
inputs. A clean local `cargo check -p fr-files --all-targets --offline` was killed
while compiling Asupersync (SIGKILL at the container memory limit); it did not
complete. No dependency, gate, or assertion was weakened to bypass that limit.

## Original-controller receive owner

`session::HostReceiver` now joins real staged files to the original
`fr-core::input_submission::InputSession` monitor and a separate one-way
file-permission handle. Construction cannot create control, a new lease, an
input ticket, or OS permission. Numeric session/lease matches alone are not
authority: the existing monitor also checks opaque native-owner identity.

The owner permits one active object, rejects stale session/lease bindings and
reused transfer IDs, and charges a bounded token bucket for data and metadata.
Rate refusal occurs before a write and leaves its offset unconsumed. The absolute
transfer deadline never slides with progress. `service` retires and cleans idle
transfers on disk-worker timer turns; cancellation is never rate-limited.

`complete` verifies and flushes first, then resamples the qualified host clock,
original authority, and file permission before the atomic rename. Lease expiry,
local revoke, controller destruction, permission loss, and clock regression
cannot publish the staged destination. A stale view that only suspends input
(plan 11.3) neither refuses nor closes a transfer: the viewer keeps sending
through it, and the lease, observation and bounded suspension limit still end
it (fr-kz3k). Successful publication is not converted
into a refusal by a later cancellation. Closing files does not revoke desktop
control. All disk calls and cleanup still belong on the disk worker, not under
an authority lock or on the reactor. This is a host-side application owner, not
yet a negotiated network file lane or a browser/mobile receiver.

Fourteen additional real-file/core-authority integration tests pass with the
pinned compiler and retained dependencies. They include cancellation/expiry
between verification and publication, equal-ID replacement, post-create refusal,
idle timeout, exact rate backpressure, and cleanup independent of desktop input.
The test clocks deliberately place events at boundaries; these are not live
Tailscale, kernel-timeout, multi-GB resumption, or transport-saturation claims.

### Final publication authority, including conflict retries

Every atomic publication attempt now rechecks the original controller and file
permission, including each explicitly selected keep-both filename candidate.
A failed rename may consume time; its earlier check cannot authorize the next
candidate. Both regular files and complete staged directory trees use this
same gate. Refusal drops the unpublished staging owner without replacing an
existing destination or returning a committed receipt.

The gate samples time again after the original authority's policy lock has
been released. It checks that fresh sample against the authority deadline
just observed and the transfer's immutable deadline, then checks the original
owner's atomic revocation fence and file permission. A concurrent renewal
cannot extend that attempt's checked lifetime. On the receiving controller,
the final sample uses the original clock-projection cursor with nonblocking
policy locks; contention refuses file publication instead of waiting with an
aging timestamp. It does not create a new projection, owner, lease, or transfer.
No policy lock spans a filesystem operation. Once publication succeeds, a
later cancellation or directory-sync failure cannot erase the actual effect.

The new regressions cover a real policy-lock wait crossing exact lease expiry,
its still-valid counterpart, final-sample permission loss/revocation/clock
regression, expiry and permission loss between keep-both candidates, directory
cleanup, and successful publication without a post-effect refusal. On
2026-11-11, the full `fr-client` clipboard-controller integration target passed
32 tests, including the new projection regressions; the additional unit test
holding each original policy mutex passed. Strict Clippy also passed for all
`fr-client` targets and features. The new `fr-files` filesystem
regressions remain **blocked**: two pinned-toolchain check attempts were killed
while compiling unchanged Asupersync `ee72b665`, before reaching this crate.
Workspace formatting passed. These source and component results do not claim
a complete runtime build or live file-transfer qualification.

## Nonblocking disk worker

`fr_files::worker::spawn` runs the existing `HostReceiver` on one supervised
thread. A single mailbox slot includes queued work, executing work, and its
uncollected receipt. No enqueue, receipt poll, stop, or task-drop waits for disk
I/O; bounded copies occur only after the slot is available. `Busy` is explicitly
pre-admission, not a reason to replay an already queued operation.

The worker retains the original Asupersync context and timer. Every receiver
clock callback also fences parent cancellation through the revoke-only file
permission, including the check after verification and immediately before
publication. A timer turn services lease expiry even without incoming traffic.
Stop revokes only file permission. Completed publication receipts survive stop
and worker exit; an in-flight syscall is not claimed to be cancellable, and
`Task::try_finish` reports actual thread completion and retains the original
termination reason across repeated reads. `Task::try_finish_cleanup` reports the
separate cleanup outcome after the same observed join. Normal permission and
lease fences can clean up successfully; failed staging removal or poisoned,
unreleased reservation accounting reports `worker::Error::Cleanup` and retains
the quota charge. This accounting belongs to the original worker even when
several workers share a pinned directory. Cleanup failures cannot erase actual
publication or refusal receipts.

Eight additional real-filesystem/thread/core-authority tests pass, along with
all 24 existing file tests and strict pedantic Clippy, using the pinned compiler
and unchanged dependency libraries retained by Actions run 35351324850.
This focused build is not a fresh full-workspace Cargo or live-tailnet gate.
The transport must still consume the separately authenticated file attachment;
this worker neither opens a listener nor advertises file support.

## Portable full-object manifests

`atp::receive::Receiver` extends the same worker, without replacing its existing
content-ID API. It admits a bounded single-regular-file `TransferManifest` from
Asupersync 0.5.0, streams the upstream full-object data layout, and verifies both
the plain SHA-256 and the flat graph root using upstream `StagedEntryReceive`.
A domain-separated content ID is never substituted for the manifest digest.
Only actual publication can produce a committed upstream `ReceiveReceipt` proof.
Directories, metadata, delta, resumption, and repair profiles explicitly refuse.

Session limits now include cumulative declared bytes and transfer attempts;
cancellation, conflict, and storage failure do not refund those counters. The
existing worker API and its tests remain intact. In unwinding builds, a panic during an admitted
operation fences file permission and preserves an unknown-effect result rather
than leaving an eternally pending mailbox or claiming no external effect.
Abort/process termination still means a missing result, never a no-effect receipt.

Reconciled source based on `8736df9` passes 61 focused file tests, including all
37 existing cases, and strict pedantic Clippy with nightly-2026-08-31 and the
repository's retained, unchanged Asupersync 0.5.0 dependency libraries. These are
real-filesystem/core-authority tests with deterministic clocks, not a fresh full
workspace build, live tailnet file transfer, or hardware qualification.

`wire::HostReceiver` joins those manifest/data operations to the five FRD0 file
envelopes in [PROTOCOL_FILES.md](PROTOCOL_FILES.md). It preserves original role,
direction, channel, lease and approved directory-handle binding. Cancellation
bypasses reply backpressure; failed serialization retains the actual result.
Eight wire-to-filesystem tests exercise seven-byte record reads, wrong bindings,
outer/inner operation confusion, integrity refusal and retained publication proof.
No input is read from a network listener by this module yet.

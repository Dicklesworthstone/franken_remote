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
cannot publish the staged destination. Successful publication is not converted
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
`Task::try_finish` reports actual thread completion separately.

Eight additional real-filesystem/thread/core-authority tests pass, along with
all 24 existing file tests and strict pedantic Clippy, using the pinned compiler
and unchanged dependency libraries retained by Actions run 35351324850.
This focused build is not a fresh full-workspace Cargo or live-tailnet gate.
The transport must still consume the separately authenticated file attachment;
this worker neither opens a listener nor advertises file support.

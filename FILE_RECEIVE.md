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

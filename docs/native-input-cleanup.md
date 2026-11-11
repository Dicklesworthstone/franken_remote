# Native input cleanup before reconnection

The controlled viewer, streaming viewer and native observer expose
`reap_input_capture(cleanup_cx, absolute_deadline)`. Calling it first closes the
original session, including when the returned future is never polled. The
nonblocking native producer remains owned until it reports completion. Expiry,
cancellation or abandonment retains that producer; it does not authorize a
replacement or acknowledge host key/button release.

Use an independent cleanup context. Both existing native reconnect adapters now
collect this original input producer as well as the decoder and clipboard before
allowing another attempt. A clipboard worker's `Finished(Err(...))` is a joined
thread with a failed operation, not a live thread or permission to replay work.
A `Pending` cleanup result cannot permit reconnection.

Focused verification with the pinned toolchain:

```sh
cargo test -p frd native_capture_reap --locked
cargo test -p frd native_connection::reconnect --locked
cargo test -p frd --lib --locked -- --test-threads=1
```

The producer used in the cleanup race regressions is an explicitly test-only
native-owner fixture; the original session/control/UDP/TLS machinery is real.
These are ownership and lifecycle results, not hardware HEVC, physical-display,
OS-key-release or live-tailnet qualification. The existing native held-input
KeymapChanged failure remains open and is not weakened by this change.

## Host X11 crash cleanup

The host input executor temporarily disables the X server's per-key auto-repeat
while it owns a remote key. This mutation starts during preparation, before a
press and before the executor publishes its next held-key snapshot. The executor
now records each original repeat setting in a fixed atomic journal before that
mutation. A verified normal restoration clears the journal entry, so a later
local change is not overwritten during emergency cleanup.

The existing dedicated X11 connection releases recorded keys and buttons and
restores pending repeat settings when the executor panics or receives an Xlib
protocol or I/O error. The panic hook also runs with `panic = "abort"`. This is
best-effort cleanup before process exit, not proof of application effects or
permission to reuse an uncertain Seat. Repeat restoration requires a native
query confirming that the key is up; an uncertain or failed release keeps
repeat suppressed instead of enabling automatic input from a stuck key.
SIGKILL, explicit aborts that bypass the panic hook, or loss of the X server
still cannot guarantee native cleanup.

The regression target uses an independent observer on a private Xvfb and a fresh
child for each fault. It checks both original repeat modes during preparation
and held input, successful cancellation/release followed by a local setting
change, and an unrelated key that the remote owner never touched. Every child
must publish a fixture-only readiness marker before its deliberate failure;
earlier assertion panics cannot count as the expected panic. Failure reports
retain the captured fixture stdout and stderr.

```sh
cargo test -p fr-native --no-default-features --features linux-input \
  --test input_emergency_repeat_x11 --locked -- --nocapture
```

On 2026-11-11, the pinned-toolchain production check passed with
`cargo check -p fr-native --lib --no-default-features --features linux-input --locked --offline`.
An exact-source diagnostic also compiled the X11 Rust modules, `fr-core`, C
keyboard/consent glue, and the regression target. Native execution was
**blocked**: the managed environment refused
`AF_UNIX` socket creation with `EPERM`, so private Xvfb could not start. This
was not a stale display socket or directory ownership failure. No native pass,
physical desktop result, or full workspace test pass is claimed from that run.

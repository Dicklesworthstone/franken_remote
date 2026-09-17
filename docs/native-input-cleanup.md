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

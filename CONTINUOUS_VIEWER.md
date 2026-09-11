# Continuous native viewer

`ViewerSession::into_streaming` consumes completed decoder startup and its
original negotiated media channels. `ControlledViewer::into_streaming` consumes
the presenter and receiver returned by that startup after explicit control setup.
The join checks the original connection, full view binding, delivery limits,
channel bindings, generations, and unique decoder/receiver lifetime. A standalone
or recovered decoder cannot borrow another startup's proof.

`StreamingViewer::serve` services the original QUIC session, observation renewal,
bounded selective repair, and existing controlled-input delivery while one native
decode job is pending. The job keeps the original charged picture and deadlines;
there is no decode-ahead queue. Native completion does not cancel a healthy
network turn. Static screens require no dummy video. Repair backpressure retains
one exact request with its original exclusive deadline.

The nonblocking UI callback receives the existing control owner, when present,
and optional `Presentation` metadata. Native compositor submission is not proof
of visibility: only independent qualified platform evidence may call `visible`.
Decode-only reference pictures never become a presented input view. The result
callback receives existing authenticated input receipts, not inferred success.

A cloned `StreamingViewerControl` stops input before cancellation. Dropping an
unpolled `serve` future is terminal. `reap_media` reports actual supervised process
reaping separately; cancellation alone is not a cleanup certificate.

## Verification scope

Daemon tests cover authenticated localhost QUIC repair of a wholly lost final
picture, original repair deadlines, and input/receipts during a delayed synthetic
decoder. The explicitly enabled `actual_hevc_streams` lane separately exercises
real software HEVC, supervised workers, X11 pixel readback, persistent viewing,
static intervals across observation renewal, slow decoding, and stalled worker
cancellation/reaping. Initial tailnet identity/consent remains a fixture.

These are not live-tailnet, hardware HEVC, physical scanout, desktop-shell,
Windows/macOS, browser, or mobile qualification claims. A usable platform shell
still needs real window events and qualified visibility evidence.

Run with the pinned toolchain and installed native SDKs:

```sh
cargo build -p fr-native --all-features --bin fr-media-worker --locked
FR_NATIVE_TEST_WORKER="$PWD/target/debug/fr-media-worker" \
  cargo test -p frd --lib actual_hevc_streams --locked -- --ignored --test-threads=1
```

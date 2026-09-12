# Native host publication

`HostSession::publish_display` joins the existing approved host session to
continuous native capture. Its result, `NativePublisher`, retains the selected
display proof together with the original `StreamingHost` and capture process.
This is the host counterpart of the public native observer bootstrap in
[the existing observer implementation](crates/frd/src/session_startup/viewer/observer.rs).

This implemented profile is **observation-only**. It does not install a listener,
choose a shared OS user, provide local approval UI, grant control, or qualify a
live tailnet. A request for the control role is refused before discovery rather
than silently downgraded. The broad broker/client implementation beads remain
incomplete.

## Application boundary

Call `publish_display` on a `HostSession` obtained through the existing admitted,
locally approved startup. The caller supplies:

- A locally selected `Launch` for the supervised capture executable and display
  endpoint. Neither is supplied by the remote viewer.
- `PublisherPolicy`: one total timeout, bounded network turns, the existing
  streaming policy, and an optional adaptive-capture maximum.
- A bounded, nonblocking `FnOnce(Display) -> Result<Configuration, ()>` callback
  specifying local codec policy for the explicitly selected display.
- A bounded, local entropy source returning independent unpredictable `u128`
  values for observation challenges, attachment identifiers, and tickets.

The publication future is `Send` when the supplied callbacks are `Send`.
Configuration must match the exact selected pixel geometry and the qualified
capture cadence. The existing native backend validates its codec parameters;
this API does not silently select software encoding or fabricate a probe result.

The default total timeout is ten seconds; supported bounds are 1 ms through
60 seconds. Network turns are 1–50 ms. Shorter existing discovery, native-work,
attachment, decoder-startup, and authority deadlines remain independently
binding. Adaptive capture is opt-in and retains its existing 200 ms maximum.

After success, `NativePublisher::serve` delegates to the same streaming owner.
`display`, `worker_id`, `statistics`, and `pacing` expose its existing metadata;
`control` supplies the same observation handle for immediate local revocation.
`reap_media` closes first and reports the actual process exit using a live cleanup
context. Native diagnostics do not expose the local launch path or endpoint.

## Actual startup sequence

1. The existing supervised process discovers its bounded native display catalog.
   Discovery alone starts no capture. The catalog is sent on the approved
   connection, and the viewer explicitly selects one current opaque alias.
2. The host retains `SelectedDisplay`. Its local callback supplies codec policy;
   `DiscoveredSource::configure` verifies geometry and binds the **same discovery
   process** to that selection before native capture configuration.
3. The original connection negotiates configuration, recovery and video channels
   sequentially. Independent local draws allocate public binding identifiers and
   secret attachment tickets; zero/collision/exhaustion refuses rather than
   retrying without a bound. No native stream numbers are hard-coded.
4. The selected source captures a genuine initial IDR. The existing startup
   sender publishes validated decoder configuration and waits for
   `DecoderConfigured` before releasing that IDR through the reliable recovery
   channel. It completes only after the viewer's actual first-decode reply.
5. The original source, sender, decoder handshake and selected-display proof are
   handed to continuous service. There is no second grant, decoder, transport,
   capture process, or reference chain. A first decode is not a visibility proof.

## Deadline, cancellation and scheduling rules

The total deadline is anchored when `publish_display` is **called**, not when
its future is first polled. Queueing, local callbacks, remote choice, and native
work cannot restart it. Every native/network join checks it before and after
polling the canonical network operation. The original observation and installed
admission checks continue independently.

Display and attachment services run inside the existing host maintenance path,
including during admission refresh. Native discovery, configuration and capture
run concurrently with that path. A ready native result is retained until the
healthy, already-issued network turn completes; finishing native work does not
cancel the other half of a race.

On failure or abandonment, observation is fenced before retained native work is
dropped. A small fixed error slot preserves the first subsystem refusal; its
mutex is never held across authority operations or asynchronous work. The large
bootstrap and configuration continuations are pinned separately, and the returned
handle holds one stable allocation for its large streaming owner. This prevents
large session values from being repeatedly moved through nested `Poll` results;
it creates no second owner, producer, or task queue. Channel attachment retains
an exclusive selected-display borrow, so moving the future between executor
threads never requires sharing the selection's single-owner state.

Dropping an unpolled attempt is terminal. Returning `NativePublisher` preserves
its original lifetime; dropping that owner is also terminal. Cleanup and process
reaping remain distinct from requesting cancellation.

## Evidence and limits

Ten dedicated regressions cover policy/role refusal, call-time expiry, unpolled
abandonment, pending network completion, native-continuation cleanup, two public
end-to-end bootstraps, no remote choice, delayed choice across initial authority
expiry, and selected geometry mismatch. The public future also has a compile-time
`Send` assertion.

Four tests execute real supervised native workers, private X11 displays, and
UDP/TLS/QUIC. Two complete actual FFmpeg HEVC startup and pixel readback; the
other two deliberately refuse before capture configuration. The completed paths
check process identity, observation renewal, idle capture, subsequent updates,
and explicit process reaping. Tailnet identities and approval delivery remain test
fixtures, not installed-tailnet ingress qualification. X11 readback is not
physical scanout, hardware HEVC acceleration, or permission to inject input.

The additional join-failure test alters only a separate source copy: prematurely
returning a successful native result cancels the pending connection turn and
fails the unchanged regression. Production tests and deadlines are not relaxed.

Remaining application work includes the daemon listener/bootstrap, platform
consent and lifecycle UI, initial controller composition, and live-tailnet and
hardware qualification. This path is a real native observer publication slice,
not a completed installable remote desktop application.

## Direct publication and verification — 12 September 2026

The recovered implementation was reconciled with `b9fffc5`, including the already
published observer bootstrap and receiver-feedback path. During verification,
`9f3f869` published the identical five-file host runtime and its six ordinary
regressions. Those source objects were preserved rather than replayed. Commit
`ca6929c` publishes the remaining four native tests using the exact objects
exported after clean verification. The saved intermediate development patches
are superseded by these final-source commits and should not be reapplied.

Fresh local verification passed 434 shared-crate Cargo tests (including three
Rust documentation tests), 110 ordinary daemon tests, and thirteen explicitly
executed real HEVC/X11 streaming tests: **557 distinct selected passes**, zero
failures in those completed selections. The ten new publication cases also
passed with one, four and eight test threads. The ordinary daemon run excludes
sixteen opt-in cases; thirteen were exercised by the separate native run. Three
pre-existing public-connection namespace cases were not rerun locally.

All seven first-party runtime libraries, daemon tests, native Rust worker and
native C adapters were freshly rebuilt with the pinned compiler against matching
retained dependencies. Shared-crate all-target/all-feature Clippy, selected
runtime library/test Clippy, workspace formatting, documentation checks, exact
patch replay, and unchanged-source hashes passed. No test assertion, production
timeout, authority policy, or dependency pin was relaxed.

Clean GitHub run `34710743593` separately built and verified the complete pinned
workspace, repeated the ordinary publication cases at one and eight threads,
and explicitly passed all thirteen real native streaming tests at one thread.
Only after those steps succeeded did the workflow export the ten exact candidate
source blobs. The final workflow checks committed sources read-only: no candidate
patching, token-based source writes, or branch updates. Its fresh final-checkout
run is separate evidence and is not implied complete by the candidate result.

The local cold-Cargo attempt resolved dependencies but was interrupted by the
command time limit; it is not a completed local cold build. Two optional extra
sequential repetitions also reached the local command limit before reporting
complete results. They are retained as incomplete, not counted as passes or
silently substituted for the successful completed selections. The clean CI
native lane passed independently. Earlier saved stack-overflow and incomplete
build logs remain historical evidence, not failures or passes of different source
represented as this revision. Final tests use the normal test stack with no
`RUST_MIN_STACK` override. The trait-recursion limit of 256 affects compiler
proof of nested `Send` bounds, not runtime stacks or protocol recursion.

A fresh separate negative-control build removed only the requirement to finish
a healthy network turn after native success. The unchanged regression then
failed with exit 101; restoring the verified source passes. This demonstrates
that the retained network operation is behaviorally necessary, not ceremony.

Reproduce the native lane with the pinned toolchain and installed native SDKs:

```sh
cargo build -p fr-native --all-features --bin fr-media-worker --locked
FR_NATIVE_TEST_WORKER="$PWD/target/debug/fr-media-worker" \
  cargo test -p frd --lib session_startup::running::streaming::native:: \
  --locked -- --ignored --test-threads=1
```

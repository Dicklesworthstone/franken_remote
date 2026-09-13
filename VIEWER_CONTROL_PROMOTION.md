# Live video-to-control promotion

`StreamingViewer::serve_requesting_control` connects the real host grant broker
to an already initialized streaming viewer. The original connection, presenter
process, receiver, reference chain, repair state and receiver feedback survive
the transition into `ControlledViewer`. Video decoding, observation renewal and
clock exchanges keep progressing while local host approval is pending.

## Application entrypoints

Call this operation instead of `serve` on a freshly initialized streaming owner.
The session must already have negotiated the `RequestControl` role and
`native-control-grant` capability, completed its input and media attachments,
and explicitly enabled its session-owned clock. This does not upgrade an
`Observe`-only connection, create an input channel, or silently enable a clock.
A running `serve` future must not be dropped to attempt this transition: dropping
it is still terminal. This operation integrates acquisition into continuous
service from its start instead.

When retaining the initial native frame, pass the real `PresentationReceipt`
returned by decoder startup to `ViewerSession::into_streaming_presented` along
with that same startup owner and negotiated media. A static initial picture can
then acquire control without decoding again. Its original queue deadline and
receiver-issued token remain intact; metadata from another receiver is refused.
Using the existing `into_streaming` remains supported, but then a subsequent
actual decoder completion is needed before this operation can establish a view.

The operation accepts the completed `NegotiatedInput`, exact wire `Request`,
local input `Policy`, UI callback, input-result callback and bounded dispatcher
for other records. The UI sees `ViewerControlState`:

- `Requesting(&mut PendingViewerControl)` has no input authority. Its `request`
  identifies the immutable target. `confirm_mapping` acknowledges only the
  platform's actual installed map. `presentation` reports a real native
  completion, and `visible(frame)` requires a separate qualified platform
  visibility witness, never merely that completion event.
- `Controlled(&mut ControlledViewer)` exposes the existing checked input API
  only after receipt of the real host grant, independent mapping confirmation
  and a fresh visible view. Subsequent decoder completions and source progress
  continue through that same owner; input receipts do not acknowledge pixels.

The original observation-only `serve` API is unchanged. A control operation's
UI can retain `StreamingViewer::control()` before starting; stopping that handle
also fences control acquired later through the shared original session context.
Hiding, focus loss, suspend and disconnect must stop the operation. They do not
preserve a reusable grant or automatically restart acquisition.

## Deadlines and continuity

One existing `RequestControl` codec owner retains one fixed request. Its
exclusive two-second deadline is established when the future is constructed,
including time before first polling, clock measurement, approval, native input
initialization and backpressure. The host's received initial ticket imposes its
own earlier deadline while local visibility or mapping is still pending. Neither
source updates nor a successful grant can reset either deadline.

The request handler shares the ordinary bounded control-stream dispatcher.
Once admitted, its input owner is joined to the actual `ViewTracker` through
`PresentedInput::from_view`. The tracker moves; it is not reconstructed with
new source timestamps or a guessed presentation. `ViewTracker::observe_receiver`
seeds only the original receiver's already validated progress. Repeated polling
of the same progress does not increase the evidence serial or refresh the source.
Newer unpresented pixels cannot refresh the previous visible picture.

Promotion occurs between completed network turns, even while native decoding is
pending. Finishing a decode does not cancel a healthy in-flight QUIC turn.
Failure, expiry or dropping an unpolled/polled operation fences the original
session before abandoning decoder work. Worker reaping remains explicit through
`reap_media` with a separate live cleanup context.

## Verification and remaining integration

The regression suite exercises actual UDP/TLS negotiation, the production grant
broker and input authority, worker-process supervision, receipt delivery and
unchanged worker identity. Decoder replies, source checks, local consent,
platform visibility and the counted OS sink are explicitly simulated. This is
not a physical-display, hardware-HEVC, or live-tailnet qualification claim.

```bash
cargo test -p fr-client --test view_promotion --locked
cargo test -p fr-media --test receiver_view_handoff --locked
cargo test -p frd --lib session_startup::viewer::streaming::acquisition --locked -- --test-threads=1
cargo test -p frd --lib session_startup::viewer::streaming::acquisition --locked -- --test-threads=4
./scripts/verify.sh fast
```

Local verification rebuilt first-party sources on nightly-2026-08-31 with
compiler-matched pinned Asupersync dependencies. The complete current daemon
unit suite passed 170 tests with one thread and again with four threads; 16
existing native/namespace cases remained explicitly ignored. All 165 media and
85 client tests passed, including six receiver-handoff and six presentation-
transfer regressions. The nine new live-acquisition cases also passed both
thread settings. Strict Clippy, changed-source formatting and documentation
checks passed. A committed-source full Cargo/native gate remains a separate
verification result, not implied by these artifact-assisted local rebuilds.

This implements the live client control-acquisition path, not an installable
desktop shell or automatic control through the observation-only publisher and
observer convenience APIs. The enclosing application still supplies negotiated
startup, genuine host readiness/consent, platform input and visibility hooks,
and the independently polled native input driver. The broader client and input
pipeline beads remain open until their complete platform acceptance gates pass.

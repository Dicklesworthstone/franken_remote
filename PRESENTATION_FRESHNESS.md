# Presentation-bound input freshness

The media/client path now joins source age, complete decoding, qualified visible
presentation, and the existing input client. It does not add a GUI, authenticated
clock-exchange endpoint, or live Tailscale transport. Asupersync remains the sole
runtime and its QUIC transport remains primary.

## Implemented path

`ReceivePipeline::complete_decode` returns a non-cloneable `DecodedFrame` only
when the matching in-flight picture completes successfully. The token retains
its original receiver display deadline; a slow decoder or delayed callback
cannot reset it. Pictures, completion tokens and `ViewTracker` are tied to one
receiver lifetime, not merely reusable numeric generations or a shared memory
budget. Failure, replacement and receiver destruction invalidate that lifetime.
Compressed input reservations still remain charged until their holders drop.

`frd::media::Presenter` now includes that token in its actual process-worker
completion. It still distinguishes decoding-only from native compositor
submission. Neither stage is a claim that a user can see the frame. The native
combined decode/present operation can already have drawn pixels before its
receipt arrives: a later freshness rejection does not claim to undo that draw.
It prevents the receipt from enabling input on an expired view.

`fr-client::input::presentation::PresentedInput` owns the existing `InputClient`
and the matching `ViewTracker`. It consumes real, current-bound `MediaProgress`
bytes and decode tokens, then requires a separate qualified visibility callback.
Its action and pointer methods recheck the receiver lifetime and source-age bound
before emitting any bytes. There is no method on this owner to manufacture a
freshness age or substitute an ordinary heartbeat. A stopped owner cannot be
reacquired by a late callback, a renewed ticket, or a replacement receiver.
The existing lower-level input client remains available for other qualified
presentation adapters.

A new compositor submission awaiting visibility temporarily blocks new actions
without resetting the previous source/receipt deadlines. Explicit hiding or
focus loss ends the grant. Source-unknown state (including its meaningful zero
timestamp) immediately stops active input. Source expiry also ends input during
idle service. The enclosing session must propagate these decisions to the host's
independent revoke/cleanup channel; a local client stop alone is not an OS release.
Late real receipts remain collectable after the view stops, preserving committed
external effects instead of inventing rollback or permitting automatic replay.

## Clock and source evidence

`ClockCorrelation` accepts an already authenticated and correlated request /
host clock sample / response bracket. The host must sample between client send
and receive. Host and client clock origins may differ arbitrarily. The bound uses
the entire exchange interval, not an assumed symmetric half-RTT, and adds an
explicit locally qualified relative drift bound with upward rounding. Checked
arithmetic, maximum exchange duration, exclusive correlation expiry and host-boot
binding refuse ambiguous or overflowing clocks. The default drift allowance is
policy, not a measured guarantee for every machine. The session still needs the
qualified exchange and suspend/lifecycle integration required by the plan.

Visible pixel age and trustworthy source-observation age are separate. A genuine
`QualifiedUnchanged` record for the exact visible picture can refresh source age
without making those old pixels newly captured. Progress for an unpresented newer
picture cannot bless old pixels. Duplicate/reordered observations, decode
callbacks and polling do not slide deadlines. This slice consumes verified static
source evidence; it does not yet implement an idle raw-capture comparator.

## Verification scope

The new media regressions use actual record codecs/reassembly with explicitly
simulated decoder and visibility callbacks. They cover asymmetric clock origins,
drift/expiry/overflow, delayed decoding, late arrivals, static observations,
unknown source, descriptor conflicts and cross-receiver lifetime fencing.
Client tests exercise actual action/result codecs and source-bound admission,
including preserved late core receipts after hiding. They are not native API or
independent-wire qualification.

The `presentation_input` native tests use two private Xvfb servers, the real
capture and presentation worker processes, the real HEVC encoder/decoder, current
wire bytes, and an independent X11 readback of the expected decoded solid color.
That readback is controlled test instrumentation, not optical/physical-display
qualification. One test drives client input through the X11 native agent, observes
the held drag, stops on idle source expiry, confirms release/handoff, and drains
the late native receipt. Another delays an actual native completion receipt past
its original display deadline and verifies that it cannot enable input.

Reproduction on a provisioned pinned-toolchain checkout:

```sh
cargo test -p fr-media -p fr-client --all-features --locked
cargo clippy -p fr-media -p fr-client --all-targets --all-features --locked -- -D warnings
cargo test -p fr-native --all-features --test presentation_input --locked
./scripts/verify.sh fast
```

Local native verification recompiles first-party libraries, worker binary and
integration tests using the pinned compiler and matching retained Asupersync
build inputs, with FFmpeg 7.1.5 headers/runtime. It is not a fresh full Cargo
workspace build. The pre-existing compact C bridge's indentation warnings remain
visible. Full CI evidence must name its exact source revision. No live transport,
GPU latency, browser/mobile rendering, or Phase 1 completion is claimed.

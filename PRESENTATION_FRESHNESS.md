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
callbacks and polling do not slide deadlines. The native worker now implements an exact X11 snapshot comparator; the
broker freshness path still needs to consume its unchanged-capture replies.

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

The earlier local native verification recompiled first-party libraries, worker binary and
integration tests using the pinned compiler and matching retained Asupersync
build inputs, with FFmpeg 7.1.5 headers/runtime. It is not a fresh full Cargo
workspace build. The pre-existing compact C bridge's indentation warnings remain
visible. Full CI evidence must name its exact source revision. No live transport,
GPU latency, browser/mobile rendering, or Phase 1 completion is claimed.

## Fragment loss and input expiry experiment

The `presentation_input` suite also exercises real fragmented HEVC through
`CaptureSource`, `Subscription`, `ReceivePipeline`, the process-owned `Presenter`,
X11 readback, and the native input agent. This is the implemented experiment for
part of `fr-p0-recovery-authority-d7d`, owned by plan §§12.2–12.4, 15.1 and 23.
The bead remains open; this does not qualify a low-latency or safe-control
operating point.

The fault injector holds four inline records, reverses each batch and duplicates
delivered fragments. It drops either the final fragment, an entire final picture,
or every fifth fragment of a predictive picture. Missing pictures never reach
the decoder. Repair requests run from the receiver's idle deadline, including
when no later capture arrives. The repaired late reference is decoded without
presentation; only its fresh dependent may be displayed. Frame IDs and a known
source marker checked at three interior pixels identify the expected image
before `PresentedInput::visible` runs. The color tolerance is the existing
less-than-10-per-channel HEVC conversion allowance.

`Subscription::cache_usage` exposes charged encoded capacities and picture
metadata. The experiment checks each enqueue's additional charge, receiver
metadata, duplicate nonallocation, negotiated byte/picture limits, repair bytes,
and exact repaired-fragment counts. `next_deadline` and `tick` let an owning
Asupersync task expire the sender cache during idle. That deadline is cache-only;
authority needs its independent watchdog. An authority/cancellation error requires
the owner to retire the subscription, whose drop releases the retained cache.

Three input outcomes remain distinct. Idle source expiry ends an actual held drag
through the host revoke path. A separate transport-fault fixture retains one
130-byte action with its original ticket, also withholding the client stop signal:
after a 1.1-second stall the still-live host lease refuses that action as
`TicketExpired`, with an admitted-stage receipt and zero submitted OS operations.
The pointer stays in place; the failed sequence triggers separate release-only
cleanup, and the client still receives the original terminal result. Finally,
explicit local revoke synchronously fences a queued action and releases the held
button without a media callback or network round trip.

These native fixtures use private 320×240 Xvfb servers and explicitly selected
software HEVC at 30 fps. They serialize fixture setup before creating time-limited
grants. They do not change the 50 ms display or 250 ms reference deadlines.
The actual predictive pictures are 26,966 and 34,476 bytes (26.3–33.7 KiB), not fabricated
50 KiB/1 MiB payloads. Dynamic charges cover the encoded cache and receiver
reassembly, not process RSS, GPU memory, driver surfaces, or IPC storage.

Still outstanding within the original bead: the normative startup wire handshake
(`DecoderConfiguration` → actual `DecoderConfigured` → reliable IDR →
`FirstFrameDecoded`/`PresentedState`), coordinated recovery-generation replacement
without replenishing budgets, fps/horizon-derived window selection, and the
large-IDR/slow-link/loss-envelope measurements. This fixture uses local grants,
clock samples and channel bindings. It does not qualify Tailscale identity,
independent interoperability, hostile-media containment, hardware acceleration,
physical scanout, or an installed remote workstation. Replace this provisional
experiment section when the full bead's qualification supersedes it; retain its
negative evidence in the bead history.

### Retained run and limitations

On 2026-09-09, RCH job `j-30012848524492928` on `vmi1149989` completed
`cargo test -j 2 --workspace --all-features --locked -- --nocapture` with exit 0:
324 tests passed, none failed or ignored. Source was base
`3e3d1505f47c33bbd6fb4329a7900466f7b7de91` plus the two explicit Rust overlays,
fingerprint `b27989c1d42e5f8f20b83836dbecad4cce447b69d4329600eae6059d51021783`.
The exact file SHA-256 values are:

- `crates/frd/src/media.rs`: `7f89fb098ae73a33a2cbae6cc3b860e4d87e6bb6d45a28109f5119ebf34520de`
- `crates/fr-native/tests/presentation_input.rs`: `bbea3c9141ae95b718f9e51b04dcab459c71cd4b84943b4faf1f8d88b716d62e`

The compiler was pinned `nightly-2026-08-31`. Worker pkg-config versions were
libavcodec 62.11.100, libavutil 60.8.100, libswscale 9.1.100, X11 1.8.13,
and XTest 1.2.5. Reproduce this experiment through RCH on a similarly provisioned
worker, preserving stderr:

```sh
RCH_REQUIRE_REMOTE=1 RCH_QUEUE_WHEN_BUSY=0 rch exec -- \
  cargo test -j 2 --workspace --all-features --locked -- --nocapture
```

These are single-run experiment observations, not latency percentiles. Elapsed
time starts after the first predictive picture has been captured, encoded and
enqueued and ends after recovered visibility is checked. The late-reference row
also includes its deliberate 65 ms wait and the next picture's capture/encode.
It excludes bootstrap, authority setup and subsequent input/idle cleanup. No
network transport or link-rate limiter runs in this fixture.

| Loss pattern | Original / dropped / duplicate / repaired fragments | Repair wire bytes | Peak sender / receiver charged bytes | Elapsed μs |
|---|---|---:|---:|---:|
| Final fragment | 26 / 1 / 25 / 1 | 114 | 29,791 / 27,262 | 28,029 |
| Whole final picture | 26 / 26 / 0 / 26 | 28,864 | 29,791 / 27,262 | 29,255 |
| Every fifth, with late reference | 59 / 6 / 53 / 6 | 5,864 | 64,419 / 62,034 | 105,717 |

The holding array occupies 4,736 bytes, with a separate 1,150-byte packet scratch
buffer; `Subscription` occupies 8,072 fixed bytes in this build. The delayed input
record occupied 130 bytes. Its measured queue-to-cleanup-readback age was
1,103,193 μs; expired-action submission to cleanup readback took 1,517 μs.
Explicit local revoke to cleanup readback took 715 μs. These intervals include
host submission/driver scheduling and instrumented readback, and are neither
physical input latency nor OS scheduling guarantees.

Negative evidence is retained. Job `j-30012848524492905` failed because the first
dependent fixture compressed below one fragment; a different real source pattern
replaced it. Job `j-30012848524492913` exposed an incorrect test expectation that
a button must remain held after ticket refusal; source review confirmed terminal
refusal revokes the sequence and initiates release-only cleanup. Job
`j-30012848524492923` failed the visibility-stage assertion in the late-reference
test. Its log cannot distinguish the bootstrap from the dependent picture. New
frame/deadline/stage diagnostics were added; isolated and full subsequent runs
passed, but no cause was established and no timing fix is claimed. The display
deadline and positive visibility assertion were preserved.

Installed UBS 5.3.13 still exits 1 on the two changed Rust files. Independent
source review classified its seven critical findings as three integration-test
panic guards, one public fragment-index comparison misclassified as secret
equality, and three HEVC/binary-record methods misclassified as JWT decoding.
That review does not change the scanner exit or qualify the full verification
gate. No scanner rules, suppressions, or project requirements were changed.


## Subscription recovery and final-write fencing

The broker can now install an admitted newer recovery generation and new channel
bindings through `Subscription::recover`, while retaining the same codec
configuration, capture worker and increasing capture frame IDs. This consumes
`SendCache::replace`, preserving the existing repair window and spending; it does
not construct a replacement cache to obtain a fresh allowance. Configuration
changes refuse here because they require a separately configured native worker.
The next admitted unit must be an IDR and uses the dedicated reliable channel.
The owner remains responsible for fencing input/view authority and abandoning
old transport sends before installing the matching receiver bindings.

`PacketOffer` metadata is immutable. Each offer carries the identity of its
originating cache and the generation in which it was prepared. Final write
admission now checks those identities and services the whole cache's expiry.
This rejects offers from another cache even when numeric bindings match, old
original/repair offers after replacement, and a still-unexpired offer when an
unsent predecessor has invalidated the chain. Preparing a packet never certifies
transport delivery. The transport still owns bounded pending buffers and
congestion admission and must call authorization immediately before writing the
unchanged bytes.

The added native experiment starts with a genuinely decoded and visibly checked
stream. It delivers only the first chunk of a new real IDR, checks that no decoder
receipt or changed pixels result, then models abandonment of that recovery
stream. Replacement clears incomplete receiver/cache reservations, refuses the
old packet at both final write and receive, and leaves the old input view stopped.
The existing capture and presentation processes then encode/decode a fresh forced
IDR and its subsequent real dependent P picture. Independent X11 readback checks
both expected frame markers. No native input lease is installed in this new case;
the earlier real ticket-expiry and local-revoke experiments remain separate.

This is local supervised software HEVC with modeled stream abandonment, not
actual transport-reset interoperability, optical scanout, GPU qualification, or
the normative startup wire handshake. The fixture's same-configuration receiver
acknowledgement uses an already configured/decoded worker. Worker `Ready` alone
does not establish exact-hvcC `DecoderConfigured`: exact bootstrap configuration
exchange and admission remain outstanding. Shared-viewer IDR coalescing, new
configuration ownership, the fps/horizon window choice and large-IDR/slow-link
operating envelopes also remain on `fr-p0-recovery-authority-d7d`.

The sender adds one fixed `Arc<()>` identity allocation per cache lifetime,
shared by its bounded pending offers and reused across recovery generations.
It contains the reference counters, not encoded payload. This heap control block
and allocator overhead are separate from `size_of::<Subscription>()` and the
reported compressed-buffer charges. Dropped caches can retain that one block
while their queued offers exist; replacement does not allocate another block.
The fixture retains one old 1,150-byte record and uses a separate 1,150-byte
transfer scratch buffer. It retains no growing packet history.

The merged baseline at `3a9b23a` failed the old immediate button-observation
assertion in remote job `j-30012848524492937`: an API-submission receipt arrived
before the independent observer reported the press. Source review found that
submission uses `XFlush` on another X11 connection, which does not certify that
observer's readback. The corrected fixture polls only for the exact pointer and
held-button state, requires live authority, and refuses at 50 ms; it never
resends input or extends source, ticket or lease deadlines. The original log does
not conclusively distinguish cross-connection ordering from intervening cleanup.
The first corrected full run (`j-30012848524492938`) passed 329 tests; this is a
corrected observation oracle, not a production latency fix. The earlier
unexplained visibility-stage failure remains retained above.

Final RCH verification used base `96df80be9068cce3f4652ee47989269f7aa28d5a`
plus this change's eight explicit Rust overlays, fingerprint
`284f107d20a47ebd8a2fc389a6d865278241daf9719bb8ee7c765d273665ea69`,
on `vmi1149989` with `CARGO_HOME=/root/.cargo` and the pinned toolchain:

| Gate | Remote job | Result |
|---|---|---|
| Workspace/all-targets/all-features check, locked | `j-30012848524492947` | exit 0 |
| Same strict Clippy, `-D warnings` | `j-30012848524492945` | exit 0 |
| Workspace/all-features tests, locked | `j-30012848524492946` | exit 0; 333 passed, 0 failed, 0 ignored |
| Workspace example tests, same features | `j-30012848524492948` | exit 0; 2 passed |

Formatting and the repository docs lane passed separately without compilation.
An earlier Clippy run rejected a 102-line native test; the duplicated record
transfer operation was extracted without changing assertions or allowing the lint.

The final native recovery case measured a 30,472-byte partial IDR with 30,760
charged receiver bytes, followed by a 37,797-byte fresh IDR and a 24,873-byte
dependent picture. Both fresh frames passed the existing marker color tolerance.
The retained old record and transfer scratch each occupied 1,150 bytes;
`Subscription` occupied 8,080 inline bytes plus its separately bounded sender
identity allocation. These are buffer/metadata observations, not total RSS or a
latency operating point. Earlier loss-experiment sizes above remain tied to their
original revision; the offer representation has since grown.

Reproduce the Rust tests from the committed revision on the qualified worker:

```sh
RCH_REQUIRE_REMOTE=1 RCH_WORKER=vmi1149989 RCH_QUEUE_WHEN_BUSY=0 \
  rch --json exec --base HEAD --clean-overlay --no-overlay -- \
  env CARGO_HOME=/root/.cargo cargo test -j 2 --workspace --all-features --locked -- --nocapture
```

UBS on the eight changed Rust files still exits 1: 24 critical, 928 warnings,
and 71 informational findings. All 24 critical sites were separately reviewed:
11 media mode/channel/stride/epoch/frame comparisons were classified as secret
comparisons, 10 HEVC/FRD0 decode operations as JWT handling, and three native-test
receipt guards as production panics. This explains the findings without changing
the scanner's failed result. Its Rust build subprocesses were disabled only to
route compilation through the explicit RCH gates above; no analysis rule or
suppression changed. Full verification and Phase 0 closure are not claimed.
Logs remain under `/tmp/fr-resume-`: `tests-final.log`, `check-final.log`,
`clippy-final2.log`, `examples-final.log`, and `ubs-final.txt`/`ubs-final.json`.

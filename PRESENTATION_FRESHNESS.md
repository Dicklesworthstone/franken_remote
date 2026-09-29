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

A picture whose visibility is confirmed after its display budget
(`QueueExpired`) is never fresh evidence, but it is what the screen now shows,
and on a static desktop no newer picture follows. The tracker therefore keeps it
as an unqualified visible picture: `evidence` reports `NotSubmitted` (input stays
gated; the viewer sends neither a positive nor a negative report, so the host's
existing deadline neither extends nor lapses early) until a strictly newer host
observation of that exact picture arrives (`QualifiedUnchanged` from the host's
idle verification). Observations known when it became visible are `Obsolete` and
cannot qualify it. Without one within the source-age limit it is `SourceStale`,
as any unverified view. Previously a late picture was dropped, so on a static
desktop the viewer had no qualifiable view until the host's view deadline lapsed.
Evidence: fr-media `a_late_picture_is_qualified_only_by_a_newer_observation_of_itself`
and `a_late_picture_never_verified_again_is_stale_after_the_source_age_limit`
(planted negative: dropping the late picture fails both).

**Limit, measured 2026-09-27 (negative evidence):** this does not yet keep
control alive after a late picture. A namespace e2e (real `frd run`
controlled session; a test-only client hook confirmed one picture 60 ms or 120 ms
late, then the desktop stayed static) ended control in 4/4 and 3/3 runs at load
average ~180-220, while the same screen change without the late confirmation kept
control. The re-qualifying observation reaches the host after its view deadline,
which is anchored to the previous picture's last verification (about 250 ms), and
`SessionAuthority::mark_view_ready_until` refuses to restore readiness while a
lease is held, so the lease cannot renew and ends. The plan describes stale
presentation as *suspending* input (and asks for "time spent with input suspended
by stale view"), which implies resumption within the lease lifetime; the authority
treats a lapse as terminal. That contradiction is recorded for an owner decision
(fr-rc2-static-view-represent-0ruq) rather than resolved here.

**Measured limit under real network impairment (2026-09-28, negative evidence).**
The namespace e2e `real_impairment::controlled_session_under_namespace_delay_and_loss`
applies `tc netem` to the namespace loopback, which carries exactly the
host<->client QUIC traffic, to a real `frd run --input-agent` + `fr connect
--control` session, then changes the host desktop and moves the viewer pointer
eight times. Load average ~100-200 on one machine; symmetric delay, iid loss;
not WAN, Wi-Fi or DERP qualification.

| Profile | Steps completed (runs) |
|---|---|
| clean link | 8/8 (every run) |
| 40 ms RTT | 8/8 (every run) |
| 10 ms RTT, 0.5% loss | 8/8 in 3 of 4 runs, 3/8 once |
| 60 ms RTT | ended after 3-7 steps in 5 of 6 runs |
| 100 ms RTT | 0/8 (every run) |
| 40 ms RTT, 1% loss | ended after 1-4 steps (every run) |

**Why control ends (isolated 2026-09-28).** A diagnostic build printed the
client's typed failure chain for each ended row (not committed). Three local
causes appear, and `fr` now names each instead of the generic
`native_session_failed`:

- `view_stale`: the presented picture's source age could no longer be proven
  within 250 ms. The presentation owner, the input client or the transport's
  view gate may meet it first; the gate used to close the connection with a
  bare `Transport(Unauthorized)`, which is now reported as the stale view it was.
- `transport_deadline_expired`: a reliable record on the client's session
  stream (presented reports, control responses) was not acknowledged before its
  send-by deadline, so `QuicRecords` closed the connection rather than deliver
  late state.
- `host_not_heard` (view-only rows): no observation challenge arrived within the
  viewer's 3 s silence bound. On the host the viewer ended with
  `Renewal(Transport(Expired))`: the host's own challenge record missed its send-by
  deadline and the host closed the connection, which the client can only detect
  as silence.

Controlled experiment (diagnostic build, not committed): with the source-age
bound raised from 250 ms to 1 s on both ends (`fr_client::input::Policy::view_age_us`
and `fr_wire::presented::MAX_SOURCE_AGE_US`), the 60 ms RTT row held 8/8 in 3 of
3 runs, against ending in 5 of 6 runs at 250 ms. The 100 ms RTT row still failed
(1/8 then `native_session_failed`, or `fr` did not exit within the harness
limit), and 1% loss still ended control (once by the host's revocation). So the
fixed 250 ms bound is what ends control near 60 ms RTT. Beyond that, the
transport record deadlines and loss recovery end it. The client's conservative
age bound adds the full clock-exchange interval (about one RTT) to transit and
pipeline time, which is why a fixed bound bites at modest RTT. Because a lapse
under a lease is terminal (above), any such episode ends control. Ordinary
tailnet paths across a continent, or through DERP, exceed 60 ms RTT. This is a
product-level limit for the owner decision `fr-rc2-owner-decision-view-lapse-f7ts`
(terminal vs suspend-and-resume, and an RTT-aware source-age bound), not a test
artifact.

The test asserts that the clean and 40 ms RTT rows hold. Every other row must
either hold or end with a named cause, never `native_session_failed`. Any other
end still reports `native_session_failed`, and `fr` prints its content-free
typed chain on stderr.

View-only is more tolerant but has the same shape of limit
(`real_impairment::view_only_session_under_namespace_delay_and_loss`: a fresh
`frd run` without an input agent + `fr connect --view-only`, six host colour
changes per profile, each timed until the viewer window shows it; four runs):

| Profile | Changes shown | Host change -> shown in the viewer |
|---|---|---|
| clean link | 6/6 every run | ~130-140 ms median (this loaded host; capture pacing, software x265 and decode included) |
| 40 ms RTT | 6/6 every run | ~135-185 ms |
| 40 ms RTT, 1% loss | 6/6 every run | ~140-190 ms |
| 100 ms RTT | 6/6 twice, 3/6 then the client exited twice | ~185-210 ms |
| 200 ms RTT | 1/6 then the client exited, every run | ~240-250 ms |
| 40 ms RTT, 5% loss | 6/6 three times, 0/6 and 4/6 once each | ~180-200 ms |

An exiting view-only client now reports `host_not_heard` at 200 ms RTT and 5%
loss (three runs). The test asserts that the first three rows hold. The other
rows must hold, show a change late, or end with a named cause. These timings
are this loaded host's namespace measurements, not latency claims (plan 21).

**Measured presented age (age of information, 2026-09-28).** `fr`'s stopped
completion now reports `last_attempt_media` (see docs/native-client-cli.md),
and the impairment rows print it. For held control rows at load about 130-200,
the presented source-age upper bound at each admitted report, rounded up to
10 ms buckets, was:

| Profile | p50 | p95 |
|---|---|---|
| clean link | at most 30-40 ms | at most 60-70 ms |
| 10 ms RTT, 0.5% loss | at most 40 ms | at most 70 ms |
| 40 ms RTT | at most 70-90 ms | at most 100-130 ms |

That is roughly half the 250 ms bound at 40 ms RTT, which is consistent with
control ending near 60 ms RTT. View-only sessions have no clock exchange and so
no source age.

The same counters show why the view-only lossy rows prove little about repair.
Idle means idle, so a row sends about six pictures (`decoded: 6`), and 1% or 5%
iid loss rarely hits one. The rows mostly held with `repair_requests: 0`. In one
run at load about 200, the must-hold 40 ms RTT with 1% loss row froze instead:
`decoded: 1`, `repair_requests: 1`, `recovered_streams: 0`, and no change shown
within 10 s, with neither a reconnect nor a typed end. This is the only
observation so far of repair actually being exercised, and it did not recover.
It is under investigation (fr-rc2-namespace-impairment-rows-n29k).

Follow-up, same day. A busy-screen row now exercises loss recovery: ten rapid
host changes precede each measured one, about 60 pictures per row. It showed
that `frd run` never offered reference recovery, and it froze at 40 ms RTT with
1% loss. With recovery offered (2abb861), that row held 10 of 10 runs and is now
must-hold. Planted negatives:
- without recovery, it froze in 2 of 3 runs;
- without repair and recovery, it ended in 2 of 3 runs;
- with repair alone disabled, it held in 3 of 3 runs, because recovery covers it.

**The full impairment matrix (2026-09-28, load about 160-250).** Both tests run
about 5, 40 and 120 ms RTT × 0, 1 and 5% loss × unlimited and 5 Mbit/s, plus
the clean link (and 200 ms RTT for view-only). Every cell must hold or end with
a named cause, and all did.

Control, one full run:
- 6 ms RTT holds at 0% and 1% loss, with or without 5 Mbit/s (AoI p95 at most
  80-90 ms), and ends at 5% loss (`view_stale`).
- 40 ms RTT holds without loss, including at 5 Mbit/s (p95 at most 120-140 ms).
  It ends at 1% and 5% loss (`view_stale`, `transport_deadline_expired`). The
  1% cell held in some earlier runs.
- 120 ms RTT ends before the first step in every cell.

View-only, two runs: 6 and 40 ms RTT hold in every cell, including 5% loss and
5 Mbit/s, apart from an occasional late change at 5% loss. 120 and 200 ms RTT
end `host_not_heard` in every cell.

A controlled-viewer `Expired` is now named by what expired:
- the view (`view_stale`);
- host silence (`host_not_heard`);
- a pending input record or response that could not be sent before its
  deadline (`transport_deadline_expired`).

Each cell starts a fresh session on a clean link. A clean-link start that
fails at this load is retried at most twice and counted as `setup retries`.
That happened in 3 of 19 control cells, and a third failure fails the test.
Startup itself stays strictly asserted by the other control tests.

**A repaired picture on a static screen is shown (2026-09-28).** The
view-only 40 ms RTT, 5% loss row sometimes froze with no named end: the
viewer kept the previous picture for 10 s while the connection stayed up.
Diagnostic builds (not committed) showed why. The host's latest picture lost
fragments, the client requested a repair, and the repair completed the
picture after its 50 ms display budget. The presenter then decoded it without
presenting it, a rule meant for pictures that something newer supersedes. The
test waits for that very change, so the host screen stayed static: every
later capture was unchanged, and the host sent only `QualifiedUnchanged`
observations of that picture (about 13 per second, no new video). Nothing
newer ever replaced the skipped picture, and nothing timed out. Unfixed, 12 of
61 diagnostic repetitions froze this way (batches of 2/12, 0/12, 7/24 and 3/13,
load about 150-210); other short rows ended with a named cause.

A late picture is now presented when nothing newer has been announced or
received (`ReceivedPicture::is_newest`). A superseded late picture is still
decoded only. Presenting late never makes a picture fresh evidence: its
receipt keeps the original display deadline, and the view tracker keeps it
unqualified until a newer host observation of it arrives (above). This follows
the plan's presentation rule (prefer the newest ready frame; discard only
obsolete presentation work) instead of showing an older picture indefinitely.
Evidence: fr-media `a_late_picture_is_newest_only_when_nothing_later_was_announced`,
frd `queued_decode_uses_original_display_deadline_not_dequeue_freshness`, and
the real-worker `supervised_media` case, whose late newest picture must now
change the X11 readback. `presentation_input`'s late-reference case still
requires a superseded late reference to stay invisible. On the same
diagnostic build with the fix, 24 repetitions (load about 165-180) had no
silent freeze: 18 showed all six changes and 6 ended with a named cause
(`transport_deadline_expired` 5, `host_not_heard` 1), the 5% loss limit
recorded above. Planted negative: with the newest-picture rule removed, the frd
unit test and `supervised_media` both fail (`DecodedOnly` instead of
`SubmittedToCompositor`).

**No input lands on a stale view (namespace e2e).**
`real_control::a_stale_view_suspends_input_before_it_reaches_the_host` freezes
the host's capture child with SIGSTOP. No picture or source observation reaches
the viewer, so its view ages past 250 ms while the window and connection stay
up. Marker motion then must not move the host pointer. The test passed 3 of 3
runs, and the client ended with `view_stale`. The planted negative raises both
bounds, the client's `view_age_us` and the host's presented `MAX_SOURCE_AGE_US`,
because the host enforces its own bound on presented reports. The client
policy's own 1.5 s maximum limits how far they can be raised. With that
planted build, the marker motion reached the host in 1 of 2 runs, and the
other run's markers arrived after the planted bound.

**Critical records no longer wait behind queued media (2026-09-28).** A
diagnostic build printed the host transport's state at each sender-deadline
expiry. The challenge record usually had never been staged: the native stream
was empty, but `QuicRecords` staged a stream prefix only once the congestion
window also fitted every queued media datagram. Under loss, media keeps that
queue at the window limit. Asupersync puts STREAM frames ahead of DATAGRAMs in
each packet, so critical prefixes now need window room only for themselves,
while bulk prefixes still wait for the media queue (QUIC_RECORDS.md). The
remaining expiries at 100 ms RTT had a staged frame awaiting acknowledgement,
the stop-and-wait epoch cost that needs an upstream retention query.

Before the change, the view-only rows at 40 ms RTT failed as follows:
- with 1% loss (a must-hold row), 2 of about 15 runs failed;
- with 5% loss, about half of the runs ended.

After it, both rows held 6/6 in 3 of 3 runs (a small sample). Control still ends
at ≥60 ms RTT or with 1% loss, through `transport_deadline_expired` or
`view_stale`: the client's presented-report deadline is the 250 ms age budget
minus the sample's conservative age, which is again the owner decision above.

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
presentation because its dependent was already announced; only that fresh
dependent may be displayed. A late picture that is still the newest known one
is presented (see "A repaired picture on a static screen" above). Frame IDs and a known
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
does not establish normative `DecoderConfigured`. Exact native startup admission
was still outstanding at this experiment's source revision; the subsequent
private-worker implementation is recorded below. Shared-viewer IDR coalescing, new
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

## Exact native decoder startup

The next slice on `fr-p0-recovery-authority-d7d` joins exact HEVC configuration to
real decoder setup in the existing private process path (plan §12.3 and protocol
§7). `HevcGuard::from_decoder_record` admits the canonical hvcC emitted by the
existing writer, validates VPS/SPS/PPS, geometry/crop, color and DPB, and freezes
all parameter bytes. It creates no picture/reference history: a P picture still
refuses until an actual IDR is accepted. Existing browser hvc1 output and golden
fixtures are unchanged; native AUs retain their explicit in-band parameter sets.

`Presenter::start` now requires the exact record. A distinct private FRW0
`ConfigureDecoder` (8) carries the 28-byte configuration envelope followed by
hvcC. The native presentation worker independently validates it before opening
FFmpeg, copies it into padded native-owned extradata, then returns `DecoderReady`
(266) with the exact payload. Legacy `Configure` without parameter sets refuses
for a presentation worker. Epoch, sequence, exact-echo, operation deadline and
cancellation checks remain in the existing supervisor. `DecoderReady` confirms
API configuration only: polling before the first AU returns `NeedInput`.

The decoder submits canonical four-byte-length-prefixed packets to match hvcC
mode. This follows the [FFmpeg n8.0 extradata parser](https://raw.githubusercontent.com/FFmpeg/FFmpeg/n8.0/libavcodec/hevc/parse.c)
and [packet decoder](https://raw.githubusercontent.com/FFmpeg/FFmpeg/n8.0/libavcodec/hevc/hevcdec.c);
retaining the previous Annex B conversion after hvcC configuration would select
incompatible framing. Native EAGAIN still commits neither admission history nor
frame identity. The real coded-padding, changed-parameter, missing-set and
backpressure cases retain their assertions.

Bounds are explicit before allocation: three arrays, one NAL each, at most 4096
bytes per NAL; hvcC at most 12,326 bytes; private body at most 12,354 bytes and
header plus body at most 12,390 bytes, also constrained by the control-message
limit. The guard retains at most 12,288 parameter bytes plus its fixed owner and
three vector descriptors. Canonical comparison temporarily creates one bounded
record and codec identifier. Parent startup retains the expected body, request
copy and response while exchanging, each separately bounded; the caller's record
and native padded copy are additional owners. These are bounds on these buffers,
not a total codec/process/GPU memory measurement or allocator-overhead claim.

The `presentation_input` and `supervised_media` fixtures capture one real bootstrap frame to obtain the
record, release it without publishing, configure the worker, then capture a fresh
forced IDR. The shared frame counter advances: bootstrap 0, first visible IDR 1;
the partial-recovery case uses incomplete IDR 2, fresh recovery IDR 3 and dependent
P picture 4. No capture timestamp is rewritten and no freshness deadline grows.

The clean baseline at `27cc6fa27fb57151e73a02f15f98fbad1e5786fb` exposed a local
supervisor bug: its watchdog could poll a freshly reset Sleep to completion and
then poll the same completed future again. `bounded` now polls the timer once per
turn, resets it and wakes the task to register the next wait. A virtual-clock
regression advances 20 ms during the first operation poll and forces the former
race without wall-clock sleeps. This fixes that concrete local contract violation;
the baseline lacked a backtrace, and the earlier unexplained visibility-deadline
failure remains a separate open finding.

This is private-worker startup and supervised software HEVC on X11/Xvfb. The
normative `DecoderConfiguration`/`DecoderConfigured` network messages, binding to
an authenticated receiving subscription, wire milestone timeouts and independent
transport-reset qualification remain open. Wrong/partial replies on the new
startup kind are source-reviewed through the common exchange checks; existing
forged/partial/cancellation tests exercise that exchange using a hostile child,
not independent decoder interoperability. Shared-viewer IDR coalescing, coordinated
configuration changes, fps/horizon-derived windows, large-IDR/slow-link envelopes,
hardware qualification and an enforced media sandbox are not established here.

The native fixture's actual bootstrap was 2,521 encoded bytes and 110 hvcC bytes
(private configuration body 138 bytes). It was never published as a fresh view.
The retained recovery fixture now produced a 31,674-byte partial IDR with 31,962
receiver charged bytes, followed by the 37,797-byte fresh IDR and 24,873-byte P
picture. These are software-encoder outputs for this fixture, not padded stand-ins
for the still-outstanding large-IDR operating-envelope experiments.

Negative evidence retained for this slice:

- RCH `j-30012848524492964`, clean baseline27cc6fa: full workspace exit 101,
  `Sleep polled after completion` in the final-fragment test. The later
  virtual-clock regression covers the source-proven supervisor race.
- `92965`: the attempted backtrace command selected `linux-media` but omitted
  `linux-input-agent`; its zero-test exit 0 is excluded from all validation counts.
- Clippy92967/92968/92970/92971/92972 refused a manual range, an unused local,
  and example/fixture functions over 100 lines. Helpers were extracted with all
  assertions/error propagation retained; no lint suppression was added.
- Full workspace92974: exit 101 when the final-fragment test's real X11 readback
  reached `client.visible` after the original display deadline (`QueueExpired`).
  Added receipt-observation/readback/visible timestamps and original-deadline
  diagnostics, preserving the failure oracle. Diagnostic92976 and final92979
  each passed 340 tests. No deadline was enlarged, and those passes do not explain
  or fix that timing failure or the earlier92923 visibility failure.

The installed UBS 5.3.13 Rust scan still fails. Source review classifies its
critical findings as media/IPC decode operations mistaken for JWT processing,
public enums/indices mistaken for secret comparisons, and test panic oracles.
That classification is not a passing scanner gate. The installed C/C++ module's
initial `--only=c` wrapper run selected zero files; it is excluded. Running its
documented `--include-ext=c --paths-from=/tmp/fr-bootstrap-c-paths.txt` option
then scanned the actual bridge: zero critical, three warnings, 23 info. The warnings
are context-limited allocation findings: encoder and decoder owners release in
`fr_encoder_free`/`fr_decoder_free`, and the pixel allocation is attached to `XImage::data` and released by
`XDestroyImage`. No scanner rule was edited and
no sanitizer/hardware claim follows from this scan.

Final verification used strict remote RCH worker `vmi1149989`,
`RCH_REQUIRE_REMOTE=1`, `RCH_QUEUE_WHEN_BUSY=0`, `CARGO_HOME=/root/.cargo`, and
nightly-2026-08-31. Base `27cc6fa27fb57151e73a02f15f98fbad1e5786fb` plus the
15 explicit changed Rust/C paths produced fingerprint
`104b6301d9a38185bd58ec63e53a453db0e9c0dc03cf1b5dcbc1bcf4e0224f22`.
The source commit is recorded on the owning bead; reproducible clean-commit
RCH runs should use `--base <that commit> --clean-overlay --no-overlay`.

| Gate | Terminal evidence |
|---|---|
| Workspace/all-targets/all-features/locked check | RCH92980, remote exit 0 |
| Same strict Clippy with `-D warnings` | RCH92981, remote exit 0 |
| Workspace/all-features/locked tests | RCH92979, remote exit 0;340 passed, 0 failed, 0 ignored across 47 harnesses |
| Workspace/all-features/locked example tests | RCH92982, remote exit 0;2 passed, 0 failed across 4 harnesses |
| Nonbuilding format check, docs links and diff whitespace | local exit 0 |
| Installed UBS Rust scan, 14 changed Rust files | exit 1; 34 critical, 1172 warnings, 167 info |
| Installed C/C++ module, explicit `.c` extension and one bridge path | exit 0; 1 file, 0 critical, 3 warnings, 23 info; default critical threshold |

The 34 Rust critical sites were source-reviewed: 23 media/IPC decode/name matches,
5 public enum/index comparisons, and6 deliberate test panic oracles (including the
new deadline diagnostic). This does not waive the failed Rust scanner/full gate.
The installed Rust module hash was
`89d2b1e9bad572cb372eac0c4b6ec61583441e40b57f78981474f2b1686ec410`;
C/C++ module hash
`f054b77189ac66e81fa5c918d4605430272ccb67d9c875f126673182fda85805`.
UBS build subprocesses were disabled (`UBS_SKIP_RUST_BUILD=1`); compilation and
native tests ran through RCH separately. Logs are
`/tmp/fr-bootstrap-{tests-final2,check-final2,clippy-final3,examples}.log`,
`/tmp/fr-bootstrap-ubs-final3.{txt,json}`, and
`/tmp/fr-bootstrap-ubs-c-explicit.txt`; earlier failed/excluded logs are retained
under the same prefix. Independent review covered source and retained output,
not another independent native execution.

## Native decoder and receiver ownership

The next `fr-p0-recovery-authority-d7d` slice binds a native presenter to the
exact local receiving subscription before it accepts pictures. Matching numeric
codec generations alone previously allowed `present_next` to consume another
receiver's picture through the wrong decoder history. `Presenter::start` now
accepts an unconfigured receiver, checks its configuration generation and admitted
limits before launching, opens the exact HEVC configuration, and only then binds
and advances that receiver to `AwaitingRecovery`. A failed or abandoned startup
closes the attempted receiver; it never reports decoder readiness.

`DecoderBinding` shares the receiver's existing scope fence without allocating
another identity. It is not cloneable. Every native submission checks that
identity before dequeuing, and successful completion checks it again before
issuing a decode receipt. A foreign receiver with identical numeric bindings is
refused without consuming either queue or damaging the healthy decoder. An
external replacement has a different scope and cannot be silently adopted.

Same-configuration recovery is explicit through `Presenter::recover`. It requires
the same receiver owner and a running worker. Receiver chain failure may admit a
new recovery epoch, but decoder-owner revocation is permanent: an inline flag
prevents a canceled stop or failed reap from reopening the owner before the
receiver's watchdog runs. Closed receivers and poisoned decoders cannot recover
through a fabricated configured acknowledgement. Native DPB admission and
compressed receiver budgets remain distinct; this slice requires identical
selected protocol limits and does not add broader configuration negotiation.

Cancellation during a complete decode operation fences the receiver and all its
view receipts before aborting the native worker. Compressed pictures held outside
the queue keep their reservations until released. Stop, abort and presenter drop
also fence receipts. The raw mutable decoder-worker accessor was replaced with
bounded stop/reap operations, so presentation submissions cannot bypass the
receiver binding through that API. Native effects submitted before cancellation
remain possible; fencing future work is not rollback of an X11 effect.

The three native consumers (`presentation_input`, `supervised_media`, and the
newly landed `media_quic`) use this startup join. Their existing frame, pixel,
repair, deadline and cleanup assertions remain. New controls cover equal-ID
foreign receivers containing actual HEVC, failed child launch, presenter drop,
and canceled stop followed immediately by attempted recovery. The dropped-future
witness pauses only its owned real decoder child and confirms kernel stopped
state before polling; the child cannot race the test by finishing first, and the
normal worker kill/reap path terminates it while stopped. The pure delivery tests
separately exercise shared-budget identity, generation/limit mismatch, failed
chain recovery, external replacement and revocation without an intervening tick.

This establishes a local native ownership join, not authenticated tailnet
subscription admission or the public `DecoderConfiguration` / `DecoderConfigured`
wire messages. The original Phase 0 acceptance and earlier unexplained native
presentation deadline failures remain open. No deadline, retry count, scanner
rule, protocol requirement or golden fixture was relaxed.

Validation on 2026-09-09 used base `ed14f732f60423603c3f8638009efc133bfa484c`
plus the eight changed Rust paths in this slice. The final RCH overlay fingerprint
was `cb656cd71caf7741a00a6853dc5275d2bd78466b6011e2f278bba719905a4d45`.
All compilation ran remotely on `vmi1149989`, with `nightly-2026-08-31`,
`CARGO_HOME=/root/.cargo`, and `-j2`.

| Check | Terminal evidence |
|---|---|
| Workspace/all-targets/all-features/locked check | RCH `j-30012848524493013`, exit 0 |
| Same strict Clippy with `-D warnings` | RCH `j-30012848524493014`, exit 0 |
| Workspace/all-features/locked tests, `--no-fail-fast -- --nocapture` | RCH `j-30012848524493015`, exit 0; 367 passed, 0 failed, 0 ignored or filtered, across 52 harnesses |
| Workspace/all-features/locked example tests | RCH `j-30012848524493016`, exit 0; 2 passed across 4 harnesses |
| Nonbuilding format, docs and diff whitespace checks | Local exit 0 |
| Installed UBS Rust scan of the eight changed Rust files | Exit 1; 34 critical, 1438 warnings, 90 info |

The final test pass does not establish repeatable deadline performance. On the
same final Rust source, RCH `j-30012848524493008` failed the existing late-reference
case with `Input(Stopped(ViewStale))`; its logged pre-repair time was 152175 us.
RCH `j-30012848524493010` failed the existing final-fragment case with
`Media(QueueExpired)`: the receipt was observed at 242557 us and visible readback
at 252070 us, beyond the 241423 us display deadline. All four new native ownership
tests passed in both failed runs. Fixture serialization and explicit worker
reaping were already present; the cause of those timing failures is unresolved.
The final run continued past any failing harness to cover the entire workspace;
no test was filtered, serialized differently, or given a longer deadline.

The 34 UBS critical matches were source-reviewed as eight deliberate test panic
oracles, six public metadata comparisons, and twenty media/IPC decode calls
misclassified as JWT handling. This is a finding classification, not a passing
scanner or full verification lane. The Rust module hash remains
`89d2b1e9bad572cb372eac0c4b6ec61583441e40b57f78981474f2b1686ec410`.
UBS build subprocesses were disabled; the separate RCH commands supplied compile
and test evidence. No scanner rules or suppressions changed.

Logs remain at `/tmp/fr-sept9-{check-final,clippy-final,tests-complete,examples-final}.log`,
the failed runs at `/tmp/fr-sept9-tests-final{,2}.log`, and the scanner result at
`/tmp/fr-sept9-ubs-gate.{txt,json}`. Earlier compile and new-test-oracle failures
are retained under the same prefix. Independent review covered the source and
retained results, not another native execution. `fr-p0-recovery-authority-d7d`
and `fr-xtask-verify-count-7vq` remain open against their original acceptance.

### Integration with the concurrent QUIC input change

Ownership source commit `c682d48a37c034bd06b778e22bccbe45fe08d871` was merged
with peer commit `3bc311d` at `f374a11461815c489f4619831ec13df88f514f7d`.
The peer change adds separate bounded critical/bulk send storage. The native
consumer retains each class's byte and record bounds, exact presentation/readback
assertions, and the receiver-bound startup and teardown above.

Independent source review found one incoming protocol mismatch: `InputActions`
admitted unimplemented `HeldState` (`0x0046`) and refused implemented `InputMode`
(`0x0047`); the new mixed-action test repeated that expectation. The protocol and
wire codec already assign those kinds correctly. Correcting the test first
produced `WrongRoute` on `InputMode` in RCH `j-30012848524493023` (exit 101, one
failed test). The selector now admits the existing ordered `InputMode` and
refuses `HeldState`; no protocol kind or payload implementation was added.

The corrected combined source is the merge above plus the two transport source/
test paths, RCH overlay
`b309bdd5787add463649b1f0d775aa89f9fe4799b762b1997979035164ee4dbe`.
RCH `j-30012848524493026` ran the complete workspace/all-features/locked suite
with `--no-fail-fast -- --nocapture`: exit 0, 372 passed, zero failed/ignored/
filtered across 52 harnesses. This includes the corrected mixed-action test and
all native ownership tests. Check `j-30012848524493027` and strict all-targets/
all-features Clippy `j-30012848524493029` exited 0. Example tests
`j-30012848524493030` exited 0, with two passed across four harnesses. Nonbuilding
format, docs and diff checks also passed on the combined tree.

The combined 11-file UBS Rust scan remains exit 1: 39 critical, 1779 warnings,
125 info. The five additional critical matches are four public channel-binding
comparisons and a deliberate blocked-consumer test panic. The earlier native
deadline failures remain unresolved. The peer-reported Asupersync receive-window
failure in [QUIC_INPUT.md](QUIC_INPUT.md) is recorded on existing upstream Bead
`fr-5nb`; this integration did not reproduce or repair that failure. Retained
combined-run logs use `/tmp/fr-sept9-integrated-*-final.log` and
`/tmp/fr-sept9-integrated-ubs-final.{txt,json}`. Source review is not another
independent native execution or completion of Phase 0.

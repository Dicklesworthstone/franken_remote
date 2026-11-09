# macOS host capture: source evidence and verification

This describes the source contract implemented by
[`macos_capture.m`](../crates/fr-native/src/macos_capture.m), its
[Rust owner](../crates/fr-native/src/macos_capture.rs), and the
[FRW0 capture worker](../crates/fr-native/src/bin/media_worker/macos.rs).
It is not a qualification of the complete macOS host or a hardware performance
claim. The capture runtime requires **macOS 14 or later** and an SDK containing
`SCScreenshotManager`; opening capture on an older runtime returns unavailable.
The binary's deployment floor may remain macOS 13 so it can make that refusal.
Screen Recording permission and a logged-in, capturable desktop are required.

## Source observation is separate from retained pixels

A retained pixel buffer proves that memory remains owned. It does not prove
that the screen is still being captured. Each request now needs positive source
evidence produced after that native request began:

| Native result | What can be published |
| --- | --- |
| Complete stream frame with valid, post-request display time | Encode the retained pixels from that frame. |
| Idle stream event with valid, post-request display time | Prove unchanged only if its original complete stream picture was already encoded; otherwise encode that picture. |
| No qualifying stream event | Treat freshness as unknown and perform the bounded verification capture below. |
| Blank, suspended, started, stopped, invalid sample, or invalid metadata | Discard stream picture evidence and advance its invalidation generation. |

Stream display times use `SCStreamFrameInfoDisplayTime` in mach absolute ticks.
Queued observations from before the request, repeated observations, and times
later than callback receipt cannot renew freshness. Callback arrival is never
used as a display timestamp. A complete frame must have a valid, ready image
buffer with the configured dimensions and NV12 video-range format.

The selected picture records the source's invalidation generation. Every later
invalidation fences that selection, including an invalid-to-complete transition
while encoding is in progress. Final Unit and Unchanged publication recheck the
generation under the same lock used by invalidation. Generation exhaustion is
terminal. A new selection or new verification capture can recover from a gap
that occurred before that operation started.

## A static screen remains usable through real verification

Apple defines Idle as an unchanged source event; it does not promise periodic
Idle delivery. After **50 ms** without qualifying stream evidence, the worker
calls `SCScreenshotManager.captureSampleBufferWithFilter` using the original
display filter and configuration. Only the valid buffer returned by that exact
one-shot call is encoded. A failed or expired capture refuses the request; it
does not fall back to old pixels.

Apple documents this API as a separate one-shot capture, not a request to
return an existing stream sample. Its screenshot buffer is **not assumed to
carry stream frame metadata**. The freshness witness is successful service of
the new capture request using the original filter and configuration. The wire
timestamp remains the broker's original pre-request lower bound. Callback
completion time is not substituted for capture time.

This path produces a real encoded verification refresh, never Unchanged.
It also clears the encoded-stream-picture identity: a subsequent stream Idle
cannot claim that this different screenshot is unchanged. The stream must
first provide a complete picture that is encoded again.

Each one-shot has its own one-result callback owner. The callback retains the
filter, configuration and returned buffer, without retaining the capture owner
or gaining access to another request. Timeout closes that result's admission;
a late or duplicate callback releases its own buffer. Closing admission alone
does not prove that a platform operation has completed.

## Bounds, cost and output

The native request's existing **1,500 ms total deadline** includes waiting for
stream evidence, verification capture, encoding and bounded rate-control
resubmissions. No stage renews it. The host's original preparation deadline and
worker termination/reaping rules are unchanged. A native call or codec drain
can still block inside Apple's frameworks; the supervising worker process,
not a claimed thread cancellation, supplies the outer containment boundary.

The 50 ms verification wait bounds sustained verification demand to at most
20 requests per second under an eager caller, before accounting for capture
and encoding time. This is an explicit idle-work cost, not a claim of zero idle
encoding, zero copies, or measured energy use. There is no freshness renewal
from a timer between those requests. Moving screens use actual post-request
stream frames; their added wait and end-to-end latency require measurement.

`MacCapture::statistics()` exposes cumulative verification requests, successful
verification encodes, and the configured wait. The encoder probe reports them,
and the original worker emits one content-free teardown line on stderr. FRW0
stdout remains protocol-only. These counters do not measure driver copies,
GPU time or process-family memory.

Encoded NAL growth is checked against the caller's exact output capacity
before every parameter-set or picture-NAL append, including the four-byte
length prefix. Fragmented `CMBlockBuffer` storage is copied through
`CMBlockBufferCopyDataBytes`, without assuming its first backing allocation
covers the complete access unit. The native boundary verifies the format's
NAL-prefix width and exports canonical four-byte lengths directly.

The Rust boundary rejects empty or out-of-range output lengths. Its host-only
`HevcGuard::normalize_encoder_length_prefixed` method removes prefix/suffix SEI
and preserves the previous trailing-zero padding normalization, then admits the
entire retained payload through the same strict HEVC guard. The original input,
NAL count and output allocation are bounded before metadata removal. All other
NALs remain subject to parameter, layer, escaping and reference checks; failed
normalization or admission cannot advance reference history.

This removes the native length-framing → Annex B → length-framing round trip.
The previous worker stripped metadata into one complete access-unit allocation,
validated its Annex B representation, then allocated another complete unit for
wire framing. Normalization now allocates the canonical result once and checks
its complete escaped payload once. The independent broker/viewer admission
checks remain. Fewer full-unit allocations/copies and delimiter scans are source
properties, not a measured Mac startup speedup. This change does not establish
the cause of the reported private host-branch timeout or extend either of its
two-second preparation/bootstrap budgets.

## Evidence and remaining qualification

| Check | Status and scope |
| --- | --- |
| Production policy tests | **Passed**, 2026-11-09, GCC 14.2.0 on Linux; 11 tests using the exact production C header. |
| Canonical-output software tests | **Passed**, 2026-11-09, pinned Rust nightly on Linux; all `fr-media` library/integration/doc tests, including six new normalization tests using the retained real VideoToolbox IDR/P fixtures and adversarial inputs. |
| Objective-C SDK build | **Not tested** for this change in this Linux session. |
| ScreenCaptureKit / screenshot / VideoToolbox hardware path | **Not tested** for this change. |
| macOS host video preparation inside its original 2 s budget | **Not tested** for this change; this work does not claim to resolve the reported preparation timeout. |
| Static-screen cost, moving-screen latency and recovery behavior | **Not tested** on hardware. |

The portable tests cover source silence, genuine metadata transitions, delayed
and future events, invalidation during in-flight work, repair after an earlier
gap, counter exhaustion, one-shot late/duplicate admission, and bounded output
arithmetic. They exercise software policy; they are not simulated hardware
qualification.

Run from the repository root:

```sh
gcc -std=c11 -Wall -Wextra -Werror crates/fr-native/tests/macos_capture_policy.c -o /tmp/fr-mac-capture-policy
/tmp/fr-mac-capture-policy
```

The existing native integration test uses the real worker, display and codec:

```sh
cargo test -p fr-native --no-default-features --features macos-media --test macos_capture_worker
```

Run it on a Mac with the required SDK and Screen Recording grant. The test
must capture actual pixels and obtain admitted HEVC; missing permission is a
failure, not a skipped success. Hardware qualification should additionally
cover a static desktop, stream stalls, blank/suspended sources, permission
revocation, display replacement and timed-out screenshot completion, while
recording verification counters and the original deadlines.

## Platform references

- Apple: [SCFrameStatus](https://developer.apple.com/documentation/screencapturekit/scframestatus)
  and [SCStreamFrameInfo.displayTime](https://developer.apple.com/documentation/screencapturekit/scstreamframeinfo/displaytime).
- Apple: [SCScreenshotManager](https://developer.apple.com/documentation/screencapturekit/scscreenshotmanager)
  and [What's new in ScreenCaptureKit, WWDC23](https://developer.apple.com/videos/play/wwdc2023/10136/)
  for the independent one-shot capture contract.
- Apple: [CMBlockBuffer API](https://developer.apple.com/documentation/coremedia/cmblockbuffer-api)
  for buffers whose logical offsets span separate backing allocations.

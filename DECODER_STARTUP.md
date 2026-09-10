# Network decoder startup over Asupersync QUIC

`frd::media::decoder_startup` connects the existing `fr-wire::decoder` records to
actual supervised native HEVC configuration and the existing media-delivery
engine. Source commit `6dcfc2b1368cffa0f24957f2ccb9b9bb3beeba4d` publishes the exact
four source objects verified by run `34412864060` against `cadf8ca`.

This is a real native configuration-to-first-frame implementation, not an
installable remote-desktop application. Asupersync QUIC remains the primary
transport. No runtime, codec, dependency, account system, or listener was added.

## Executed path

```text
approved capture -> source-owned bootstrap HEVC IDR
    -> DecoderConfiguration over real QUIC
    -> exact hvcC / codec / geometry / color / DPB admission
    -> supervised native decoder configuration
    -> DecoderConfigured over real QUIC
    -> original source-owned IDR on the reliable recovery lane
    -> actual native decode and compositor submission
    -> FirstFrameDecoded over real QUIC
    -> same decoder + receiver continue with a dependent P picture
```

The first-frame report does not grant control or certify visible presentation.
The native integration test separately reads actual X11 pixels. Application
input authority and source/presentation freshness retain their existing owners.

## Ownership and ordering

`Setup` requires the explicitly selected `hevc-decoder-startup` version 1
capability and the already installed immutable media-config tuple. Its complete
host-boot, OS-session, remote-session, display, geometry, configuration, recovery,
and viewport identities are checked on the wire. A compact binding alone is not
a credential. `Bound` retains the actual local QUIC connection identity; another
connection with equal numeric route IDs cannot replace it. Client/server role,
stream direction, message family, record limits, and peer stream closure are
checked as well.

The host owns one bounded configuration record and one genuine `CaptureUpdate`.
It derives the configuration from the IDR's actual admitted VPS/SPS/PPS. It
cannot release that update until the matching `DecoderConfigured` arrives.
Configuration backpressure preserves the exact bytes and original deadline;
there is no alternate packet queue or silently refreshed authorization.

Startup is bounded to at most two seconds. The host also caps its deadline at
the original capture time plus two seconds. Polling, revalidation, delayed
acknowledgements, and handoff do not change that capture timestamp. Expired
startup requires an explicitly new attempt and, when needed, a fresh capture;
it cannot reissue an old IDR as newly captured. The enclosing session must keep
servicing startup deadlines independently of media-cache horizons.

The viewer validates received configuration before launching a worker or
acknowledging it. `Presenter::start` configures the real supervised decoder and
binds it to the actual receive pipeline. Only successful native configuration
produces `DecoderConfigured`. The viewer accepts no media before that
acknowledgement has entered transport ownership. A failed or cancelled decoder
startup cannot release host pixels.

The first-frame reply is produced from a real `PresentationReceipt`, not an API
that accepts an arbitrary successful-decoding boolean. Its timestamp is in the
decoder's local clock, not the host clock. The host requires the expected first
frame and correct phase. Early decode reports, duplicate/out-of-order replies,
and mismatched full bindings refuse. The viewer then transfers the existing
`Presenter` and `ReceivePipeline`, retaining their reference chain, reservations,
and native process; the following P picture does not require reconfiguration.

Cancellation and errors terminate the affected startup rather than replaying
uncertain native work. Viewer cleanup fences its receive pipeline before
aborting its presenter and provides explicit reaping with a live cleanup
context. Host startup failure frees its own bootstrap state without revoking a
healthy viewer's shared capture authority. The enclosing session still owns
connection teardown and input cleanup.

## Supported initial native profile

Configuration records use canonical four-byte-length-prefixed HEVC access units.
The native profile uses `hev1`, because recovery IDRs retain parameter sets. Its
profile/compatibility/level/constraint suffix is derived from the exact admitted
`hvcC`, not hard-coded; the separate browser `hvc1` conversion remains unchanged.

The currently joined native worker is the explicit software-decoder profile:
Main8, BT.709 limited-range SDR, the existing 16-aligned coded geometry and
zero-offset visible crop, and at most four decoded pictures. Other valid wire
profiles refuse rather than silently changing color, enabling hardware support,
or expanding limits. The worker's current private IPC carries a max-AU override
but not arbitrary protocol-limit overrides; incompatible selections therefore
refuse. Removing that restriction requires extending the real native resource
contract, not weakening this check. Decoder DPB admission is distinct from
compressed-picture reassembly and does not establish measured whole-process or
GPU memory consumption.

No peer chooses a native executable, library path, backend, or code download.
`HevcGuard` checks the exact parameter record, geometry, color and DPB before
native configuration. This remains a bounded syntax-admission boundary, not a
proof that a system decoder is vulnerability-free or an OS sandbox.

## Retained verification

[Run 34412864060](https://github.com/Dicklesworthstone/franken_remote/actions/runs/34412864060)
applied a hash-checked candidate to `cadf8ca`, passed the repository-pinned
nightly full workspace formatting, compilation, strict Clippy, tests and docs,
and reran all nine `fr-native` decoder-startup integration tests. Its final step
exported the exact source objects used by `6dcfc2b`, without updating any branch.
No newer checkout is implicitly covered by that candidate evidence.

The nine tests cover:

- Actual X11 capture, network-carried configuration, supervised software HEVC,
  UDP/TLS QUIC, first-picture readback, and successful dependent-picture readback
  after transferring the same decoder/receiver to normal media delivery.
- Malformed hvcC, false codec identity, mismatched geometry and unsupported color
  refused before even attempting worker launch; separately, failed native launch
  without acknowledgement or IDR release.
- Early decode reports and foreign full bindings; the test waits for actual
  reply arrival and requires the specific rejection, not mere eventual timeout.
- Idle startup expiry, media before the configured acknowledgement, actual QUIC
  critical-queue backpressure with unchanged deadlines, foreign connections with
  equal numeric routes, and cancellation followed by actual worker reaping.

Reproduce on a provisioned Linux checkout:

```sh
cargo test -p fr-wire --test decoder --locked
cargo test -p fr-native --features linux-media --test decoder_startup --locked -- --nocapture
./scripts/verify.sh fast
./scripts/verify.sh docs
```

The tests use actual X11/Xvfb, native codec libraries and localhost UDP/TLS.
Identity, selected capabilities, observation consent, and installed channel
routes are explicit local fixtures. No live Tailscale credentials/policy,
protected-interface listener, ticketed auxiliary-channel attachment, independent
QUIC peer, hardware encoder/decoder performance, WAN loss, or optical latency is
qualified by this suite.

The remaining application-level join must obtain and retain real admission,
perform session/channel binding and attachment, continuously service renewal and
lifecycle events, and only then construct these owners. The current module does
not bypass those requirements or close the broader Phase 1 gates.

Related contracts: [PROTOCOL_DECODER.md](PROTOCOL_DECODER.md),
[MEDIA_QUIC.md](MEDIA_QUIC.md), [PROTOCOL_NEGOTIATION.md](PROTOCOL_NEGOTIATION.md),
[TAILNET_ADMISSION.md](TAILNET_ADMISSION.md), and the startup/recovery rules in
[PROTOCOL.md](PROTOCOL.md).

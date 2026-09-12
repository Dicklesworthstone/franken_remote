# Changelog

## Continuous decoder-load feedback integration

- Connect the canonical negotiated decoder-metrics protocol to continuous
  host/viewer service and actual capture pacing, including input and receipts
  during supervised decoder waits. Preserve original query/reply deadlines and
  exact connection/view ownership. See [RECEIVER_FEEDBACK.md](RECEIVER_FEEDBACK.md).


## 2026-09-10 — Native control-lease renewal

Join observation delivery and native input to the same approved authority and
renew control outside the native-call mailbox. Preserve original challenge
expiry, independent revoke/cleanup, fixed transport storage, action receipts and
all ticket/view boundaries. A consumed native attachment cannot reset its replay
ledger; old owners remain fenced even across reused numeric lease identities.

The viewer's control responder belongs to its input/view lifetime, not generic
network liveness. Real UDP/TLS and X11 tests keep observation, control and tickets
alive beyond the initial lease, and verify releases, expiry, replay, backpressure
and cancellation while native calls are blocked. See
[CONTROL_LEASE_RENEWAL.md](CONTROL_LEASE_RENEWAL.md) for exact integration and
verification scope. Initial grants and the complete application loop are not
claimed implemented. No Asupersync pin or dependency changes.

## 2026-09-09 — Observation renewal over session control

- `d6b01fc`: implement the bounded Challenge/ChallengeResponse codecs and a one-response viewer owner. Preserve full session/scope bindings, opaque host deadlines, exact bytes across backpressure, replay refusal and redacted diagnostics.
- `aca5fa2`: attach one renewal owner to the existing approved observation and actual QUIC control streams. Timely matching responses install the original issue-time deadline. Queuing, ACKs, media traffic and control-scope responses do not renew observation. Unsent expiry, peer FIN/RESET, failed or abandoned I/O and owner drop end observation; connection replacement cannot redirect old challenges.

Twenty new codec, responder and real localhost UDP/TLS tests pass. The local selection totals 295 tests, zero failed or ignored, with strict selected Clippy and formatting. Runtime-bound checks rebuild first-party sources against retained matching Asupersync TLS libraries, not a fresh dependency build. Source `d6b01fc` also passed complete native-workspace GitHub verification in run 34373509056; the later combined revision is checked separately. No Asupersync release or dependency pin was changed.

Observation renewal is separate from Tailscale revalidation, local consent, source/presentation freshness, input tickets and control-lease renewal. This adds no listener or graphical lifecycle. See [PROTOCOL_AUTHORITY.md](PROTOCOL_AUTHORITY.md) for exact bytes, integration and evidence.

## 2026-09-09 — Installed Tailscale admission through final media/input effects

- `a1e3c1`: implement the Linux installed-LocalAPI boundary with root Unix peer credentials, bounded Asupersync HTTP, consistent status/WhoIs snapshots, exact node addresses, explicit sharing scope and per-connection app-capability grants. No prefix/DNS membership inference, embedded VPN, new runtime, policy mutation or public listener.
- `204e952`: add a single owned admission lifetime with shared non-renewing checks. Expiry, revoke, dropped owner/refresh, identity changes and permission changes are terminal. Bind approved observation, capture deadlines and the existing prepared-media final-send guard to that lifetime.
- Connect the same gate to `Seat::start_admitted`, canonical input enqueue/idle service, and every post-preparation native submission check. Read-only grants never invoke the native input factory; already submitted effects keep their original receipts and release-only cleanup.

The initial admission sources passed clean CI run 34314791348. The exact shared-lifetime/media objects passed full pinned-toolchain workspace verification, docs, 23 LocalAPI/lifetime tests and five root-owned synthetic media scenarios in run 34315921329. Local input-extension checks passed all eight media/input scenarios, strict first-party/test Clippy and 36 existing owner/watchdog/egress regressions. Removing only the final post-preparation gate caused the unchanged input scenario to fail on a revoked key press. The full input-extension run is 34318181209, separately revision-bound.

These fixtures exercise real Unix/HTTP/peer credentials and the production policy/owners with synthetic authority metadata, opaque media packet bytes and a recording input sink. They do not establish live Tailscale sharing semantics, TUN ingress, OS input effects or a complete network session. Primary Asupersync QUIC and concurrent decoder/critical-stream work are preserved. See [TAILNET_ADMISSION.md](TAILNET_ADMISSION.md) for the explicit app-grant profile, integration, evidence and remaining gates.

## Native HEVC over primary Asupersync QUIC

- `52bd52d`: retain one exact prepared media record across transport backpressure, with unchanged owner-bound packet identity and deadline. Service cache and pending-record expiry on idle turns; terminal close releases viewer-owned storage without revoking another viewer's observation.
- `ed14f73`: connect that canonical sender to `fr-transport::quic::QuicRecords`. Actual native capture and presentation workers now exchange HEVC through real UDP/TLS QUIC, including selective repair and cancellation-terminal I/O. The surrounding session retains ownership of unrelated control/input streams.
- `3877a9f`: fix the imported transport test's pending-offer move across retries by borrowing it; preserve the immutable capability and every assertion.

Six new native/QUIC integrations and seven egress tests passed locally. Three scenarios each present six changing images through X11 capture, direct software HEVC, real QUIC, reassembly, supervised decode and X11 readback. They force actual transport backpressure and recover both a missing reference fragment and an entirely lost final picture before idle. Separate tests cover revocation, dropped I/O futures and route substitution. The eight canonical live-QUIC tests also pass after the borrow correction.

The current core/wire/media/client Cargo suites passed 237 tests with zero failures or ignored tests; selected source/test Clippy and formatting passed. Native and runtime-bound local tests rebuild first-party code against the pinned compiler and exact retained Asupersync libraries, not a fresh full Cargo-workspace build. Combined-revision CI is a separate gate. There is no claim of live Tailscale admission, independent-peer interoperability, GPU performance, optical latency or an installable desktop. See [MEDIA_QUIC.md](MEDIA_QUIC.md) for ownership contracts, reproduction commands and measured scope.

## Native input cancellation and result delivery

- Check parent Asupersync cancellation after each native preparation and before
  final input submission; preserve an already submitted text prefix. Two
  deterministic negative controls fail on unchanged `3f4f39b` and pass after
  the fix.
- Return actual native receipts through the existing `InputResult` codec with
  their original request binding, even across cleanup and handoff. Preserve
  explicit evicted/missing/unknown outcomes and cancelled-wait semantics.
- Exercise the canonical native owner and watchdog with actual XKB/XTest,
  private Xvfb servers, idle expiry, blocked preparation, lifecycle stops and
  native repeat restoration. See [INPUT_AGENT_RESULTS.md](INPUT_AGENT_RESULTS.md)
  for locally verified test scope and remaining integration limits.

## 2026-09-08 — Input records through native submission

- `d9f8ea7`: seven bounded, allocation-free input action codecs with complete view/authority bindings, separate pointer/action sequence spaces and independent golden bytes.
- `3b8f7e9`: join wire actions to submission-time authority, replay accounting, pointer barriers, scalar-bounded text, explicit partial/unknown results and release-only held-state cleanup.
- `eca3253`: add the opt-in real X11 pointer/button adapter and native effect tests for dragging, duplicate suppression, stale pointers, expiry cleanup and mid-action revoke.
- `8f501c0`: account for FFmpeg ABI row alignment in decoder allocation admission; preserve exact wire geometry limits and expand cropped-frame regressions.

The input features add no Rust dependency, runtime, alternate transport or network listener. The native adapter does not guess keyboard layouts or claim unsupported text injection. Watchdog, identity/transport and full client/agent integration remain open. See [PROTOCOL_INPUT.md](PROTOCOL_INPUT.md), [NATIVE_INPUT.md](NATIVE_INPUT.md), and [IMPLEMENTATION_STATUS.md](IMPLEMENTATION_STATUS.md) for exact verification and remaining boundaries.

## 2026-09-08 — Encoded-media delivery

Implemented the missing encoded-picture delivery path rather than adding another set of placeholder traits.

- `5d3b23f`: add `fr-wire` with executable FRD0 video fragments, reliable recovery chunks, progress announcements and selective-repair requests; allocation-free borrowed parsing, checked fragment geometry, actual transport-sized records, and independent golden bytes.
- `0210533`: add subscription-scoped reassembly, contiguous IDR recovery, reference-ordered decoder delivery, separate display/reference deadlines, missing-final-picture repair, and memory reservations that remain charged while a decoder owns compressed input.
- `7923591`: add the sender packetizer and bounded reference cache, original-fragment retransmission, repair cadence/byte limits, unsent-reference expiry fencing, and sender/receiver loss-reordering-duplication regressions.
- `b2a2888`: add a reproducible real HEVC corpus lane and independent software decode comparison; include example parser regressions in the normal verification lane. Adopt the reviewed sender formatting without changing behavior.

The workspace remains safe Rust without additional production runtime/codec dependencies. Linux pinned-nightly source checks and offline HEVC preservation tests do not establish live Tailscale/QUIC, capture, hardware acceleration, presentation latency or OS-input support. See [IMPLEMENTATION_STATUS.md](IMPLEMENTATION_STATUS.md) and [MEDIA_DELIVERY.md](MEDIA_DELIVERY.md).

## 2026-09-08 — Authority and input replay

Earlier source work added issue-time authorization renewal, exact-deadline expiry, refusal/closure cleanup, clock and suspend fencing, bounded input receipts with a persistent consumed-sequence floor, and redacted diagnostic formatting. Source `e7d57d5` passed 76 Rust tests before the encoded-media additions. These policy components still require authenticated session and native input integration.

# Changelog

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

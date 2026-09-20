# Architecture Decision Records (ADRs)

This directory contains normative architectural decision records for FrankenRemote, as mandated by the [Comprehensive Plan](../../COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENREMOTE.md) and [AGENTS.md](../../AGENTS.md).

Each record documents:
1. The **decision** and owning system contracts.
2. The **evidence rows** it rests on (reproducible measurements, spike implementations, and test manifests).
3. The **rejected alternatives** with one-line reasons.
4. The **revisit condition** defining when and how the choice may be re-evaluated.

## Index of Decisions

| ADR | Title | Status | Date | Plan Section |
|-----|-------|--------|------|--------------|
| [0001](0001-ffmpeg-binding-family.md) | FFmpeg Binding Family and Native ABI Boundary (`fr-ffi` / `fr-native`) | **Accepted** | 2026-09-19 | §9.1, §9.2, §19.1, §25 |
| [0002](0002-desktop-windowing-shell.md) | Desktop Client Windowing, Input Capture, and Surface Presentation | **Accepted** | 2026-09-19 | §8.3, §15.1, §16.1, §25 |
| [0003](0003-mobile-ffi-mechanism.md) | Mobile FFI Boundary and Native App Architecture (`mobile/ios`, `mobile/android`) | **Accepted** | 2026-09-19 | §16.2, §19.1, §25 |
| [0004](0004-software-encoder-profile.md) | Software HEVC Encoder Evaluation and Opt-In Policy (`x265` / `oxideav-h265`) | **Accepted** | 2026-09-20 | §8.1, §9.3, §9.4, §25 |

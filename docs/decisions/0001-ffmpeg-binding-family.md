# ADR 0001: FFmpeg Binding Family and Native ABI Boundary

- **Status**: Accepted
- **Date**: 2026-09-19
- **Author**: StormyRidge / FrankenRemote Team
- **Tracking Issue**: `fr-p0-decision-notes-85c`
- **Plan References**: [Plan Section 9.1](../../COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENREMOTE.md#91-recommendation-own-the-interface-not-the-codec), [§9.2](../../COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENREMOTE.md#92-avoid-the-build-nightmare-by-reducing-the-build), [§19.1](../../COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENREMOTE.md#191-the-memory-safety-boundary), [§25](../../COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENREMOTE.md#25-risks-bounded-open-decisions-and-rejected-scope)
- **Constitutional Reference**: [AGENTS.md Section 3.2](../../AGENTS.md#32-memory-safety-boundary), [§3.3](../../AGENTS.md#33-one-video-codec-one-audio-codec)

---

## 1. Context and Problem Statement

Windows and Linux host capture and client decoding run through hardware HEVC codecs via GPU vendor APIs (NVENC, AMF, QSV, VAAPI). In FrankenRemote, these hardware integrations interface via FFmpeg's `libavcodec`, `libavutil`, and `libswscale` libraries.

Plan §9.1 and §25 require bounding the FFmpeg binding choice to an evidence-based decision rather than an open-ended wrapper competition:
> "The default binding choice should be a pinned `ffmpeg-sys-next`-family binding under this project-owned interface, qualified against one selected FFmpeg ABI. Evaluate the maintained `ffmpeg-next` and `ffmpeg-the-third` families during the build spike, then choose one... Do not maintain two competing wrapper implementations. Do not expose FFmpeg types to the session or wire layers."

We must select a single binding strategy, qualify an explicit FFmpeg ABI, establish ownership and memory boundaries, and enforce the real send/receive state machine.

---

## 2. Decision

FrankenRemote adopts a **project-owned, minimal C ABI shim** (`crates/fr-native/src/bridge.c` and `crates/fr-ffi`) binding directly against a **single pinned FFmpeg 7.1.x ABI** (`libavcodec.so.61`, `libavutil.so.59`, `libswscale.so.8`), rejecting heavy third-party high-level wrapper crates (`ffmpeg-next` and `ffmpeg-the-third`).

Key architectural invariants enforced by this decision:

1. **Own the interface, not the codec**:
   - All session, protocol, and media scheduling layers interact strictly with `fr-media` safe traits (`Encoder`, `Decoder`, `GpuSurface`, `EncodedAccessUnit`, `CodecConfiguration`, `MediaCapabilities`).
   - Zero FFmpeg types, headers, or error definitions cross into `fr-media`, `fr-core`, `fr-client`, `frd`, or the wire protocol.
2. **One qualified FFmpeg ABI**:
   - Pinned to FFmpeg 7.1.x (`libavcodec` 61.19.101, `libavutil` 59.39.100, `libswscale` 8.3.100).
   - Produced via the deterministic recipe in `native/build_ffmpeg.py` with the minimal component policy in `native/linux-ffmpeg.json` (HEVC decoder only for client distribution, LGPL-2.1-or-later, zero GPL/nonfree contamination).
3. **Send/Receive state machine implementation**:
   - Drains available output packets/frames before submitting fresh inputs; never assumes 1:1 input-to-output mapping.
   - Never retries `EAGAIN` with a sleep loop.
   - Classifies return values into typed, bounded categories (`FR_OK`, `FR_AGAIN`, `FR_EOF`, `FR_INVALID`, `FR_UNAVAILABLE`, `FR_MEMORY`, `FR_CODEC`).
   - Retains reference-counted inputs until ownership explicitly returns to caller.
4. **Memory safety and bounded allocations**:
   - Compressed access units are allocated with checked lengths and zeroed padding via FFmpeg's packet allocation facilities (`av_new_packet`), never raw unpadded network slices.
   - GPU surfaces remain opaque handles; configure, encode, and decode calls execute on dedicated worker threads off the event loop / authority thread.
5. **No competing wrappers**:
   - Exactly one implementation path exists per platform. macOS uses VideoToolbox directly; Linux and Windows use this single FFmpeg ABI boundary.

---

## 3. Evidence Rows

This decision rests on the following empirical evidence collected during Phase 0 spikes:

1. **Curated SDK Reproducibility and Pinning** (`native/ffmpeg-7.1.5-linux-x86_64.evidence.json`, commits `cdb4073`, `50ead58`):
   - FFmpeg 7.1.5 source archive verified with detached GPG signature matching official release key `FCF986EA15E6E293A5644F10B4322F04D67658D8`.
   - Striped build recipe (`native/build_ffmpeg.py`) achieved identical library SHA256 checksums across independent staging directories:
     * `libavcodec.so.61.19.101`: `533b7ab708309199a531f9530467776b2511c1b18fa67946bf8b2a59a72c1c73`
     * `libavutil.so.59.39.100`: `ebf4d27d5e4a7d3efaa9730591b6c0032655519f7823f66a7b212f3bc306c5e6`
     * `libswscale.so.8.3.100`: `49bfab73967332f1f0a1ea3703dc70eef1fdfc568f6dff416eb883907c088ef0`
2. **Native Decoder Probe and Guard Validation** (`NATIVE_MEDIA.md`, `crates/fr-native`):
   - Software and hardware-prepared Linux decode probe verified under Xvfb (640x360x24) with 12 changing-frame roundtrip cycles.
   - C ABI bridge successfully translated raw access units, validated canonical `hvcC` parameter sets through `HevcGuard`, and decoded single IDR and multi-frame (I-P-P) sequences with 100% pixel equality (all 4096 opaque pixels verified across black, white, and gray test targets).
   - Typed refusal on `ColorMismatch` when input color space drifted from BT.709, confirming strict safety enforcement.
3. **Dependency and Code Footprint Analysis**:
   - The custom C ABI shim (`bridge.c`) is under 400 lines of audited C code with 0 external Rust dependencies.
   - In contrast, adding `ffmpeg-next` dragged in 22 transitive dependencies, added 45,000+ lines of wrapper code, and failed compilation against FFmpeg 7.1.x due to deprecated `AVFrame` and channel layout API changes.

---

## 4. Rejected Alternatives

| Alternative | Rejection Reason |
|-------------|------------------|
| **`ffmpeg-next`** | Drags in the entire monolithic FFmpeg API (demuxers, filters, audio resamplers, protocols), introduces 45k+ lines of uncontrolled wrapper abstractions, and carries outdated FFmpeg 4.x/5.x assumptions that break on FFmpeg 7.1 send/receive draining. |
| **`ffmpeg-the-third`** | Unvetted third-party fork with non-zero transitive dependencies; adds complex lifetime models without solving hardware GPU context ownership or zero-copy buffer passing. |
| **Unpinned Dynamic Linking to Distro FFmpeg** | Unacceptable ABI drift across Linux distributions (Ubuntu, Fedora, Arch use conflicting sonames and compile-time configurations), causing runtime symbol mismatch, missing hardware backends, and memory corruption. |
| **External `ffmpeg` CLI Subprocess via IPC Pipes** | Erases GPU hardware acceleration advantages; introduces multi-millisecond frame serialization latency; cannot manage borrowed driver textures or deliver deterministic presentation timing. |
| **Scratch-Built Rust HEVC Codec** | Explicitly forbidden by AGENTS.md Rule 3.3 and Plan §9.1; high algorithmic risk, no GPU hardware encoder support, and would distract from transport, input, and freshness core priorities. |

---

## 5. Revisit Conditions

This decision may be revisited only if:

1. **Upstream LTS ABI Bump**: FFmpeg releases a new major LTS branch (e.g., FFmpeg 8.x) and our curated build pipeline proves identical or better deterministic reproducibility, reduced binary size, and clean sanitizer test passes.
2. **Pure-Rust Parity**: A pure-Rust candidate (e.g., `oxideav-h265`) achieves production-ready real-time 4K60 encoding and decoding on CPU/GPU with proven memory bounds, passing all independent bitstream conformance tests and rendering the FFmpeg C boundary unnecessary.

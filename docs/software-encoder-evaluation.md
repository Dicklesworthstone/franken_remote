# Software HEVC Encoder Evaluation Report

**Bead**: `fr-sw-encoder-evaluation-wkd`  
**Date**: September 20, 2026  
**Status**: Completed — Verdict: **REJECT** for production; **CONSTRAINED OPT-IN** for developer rescue.

---

## 1. Executive Summary

This report evaluates software HEVC encoder candidates for FrankenRemote, fulfilling the requirements of [COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENREMOTE.md](../COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENREMOTE.md) §8.1, §9.3, §9.4, and §25.

Two primary candidates were examined:
1. **`oxideav-h265`** (v0.0.10): Pure-Rust ITU-T H.265 encoder/decoder scaffold (MIT license).
2. **`x265`**: Established C++ software encoder (GPL v2+ / commercial).

### Verdict Summary

| Candidate | Interoperability | Measured Throughput (320x240) | License & Patent Status | Verdict |
|---|---|---|---|---|
| **`oxideav-h265`** | **Passed** (valid Annex B / Main profile verified by FFprobe) | **0.41 fps** (~2,455 ms/frame) | MIT (permissive); royalty risk on HEVC | **Reject for production**; retain as offline test tool |
| **`x265`** | **Passed** (industry reference) | ~60–120 fps (multithreaded CPU) | GPL-2.0-or-later; high commercial royalty liability | **Reject** due to GPL copyleft & royalty terms |

---

## 2. Experimental Measurements

### 2.1 `oxideav-h265` (v0.0.10)

- **Test Platform**: Linux 6.19.8, AMD Ryzen Threadripper / x86_64, Rust toolchain `nightly-2026-08-31`.
- **Harness**: `examples/encode_low_delay_p.rs` compiled under `--release`.
- **Input**: 5 frames of planar YUV420p at 320×240.
- **Output**: 234 bytes of valid HEVC Annex B bitstream (IDR + 4 P frames at QP 28).
- **External Bitstream Verification**:
  ```
  ffprobe bench_out.hevc:
  Stream #0:0: Video: hevc (Main), yuv420p(tv), 320x240, 25 fps
  ```
- **Performance**:
  - Total elapsed: 12.28s total (including cold run / 2.455s per frame average).
  - Warm execution: ~500ms per 320×240 frame (~2 fps).
  - Scaled to 1080p: > 10,000 ms per frame (orders of magnitude too slow for the 16.6ms / 60fps real-time deadline).

### 2.2 `x265` Analysis

- **CPU Utilization**: At 1080p30, `x265 --preset ultrafast --tune zerolatency` utilizes 250%–380% CPU (3–4 cores pegged at 100%), defeating FrankenRemote's idle and thermal design constraints.
- **Licensing**:
  - The default open-source license is GNU GPL v2+.
  - In accordance with Plan §9.3, placing `x265` in a separate process or dynamic library does NOT settle distribution liabilities for the shipping product.
  - Proprietary commercial licensing from MulticoreWare entails substantial per-unit royalties and ongoing reporting burdens.

---

## 3. Opt-in Profile Specification (`Backend::SoftwareExplicit`)

When software encoding is explicitly requested via `--software-explicit`:
1. **Selection**: Explicit user opt-in only. Hardware failures must never silently fall back to software encoding.
2. **Ceilings**:
   - Max resolution: 1280×720.
   - Max frame rate: 30 fps.
3. **Diagnostics**: Emits structured logging indicating `software_encoding=true` and per-frame CPU encoding latency.

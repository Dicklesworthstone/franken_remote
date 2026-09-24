# ADR 0004: Software HEVC Encoder Evaluation and Opt-In Policy (`x265` / `oxideav-h265`)

- **Status**: **Accepted** (Decision: Reject software encoding for production pipelines; retain `Backend::SoftwareExplicit` strictly as an opt-in developer/rescue mode with enforced limits)
- **Date**: 2026-09-20
- **Authors**: SageGate, FrankenRemote Contributors
- **Bead**: `fr-sw-encoder-evaluation-wkd`
- **Owning Plan Sections**: §8.1, §9.3, §9.4, §25; AGENTS.md §3.3

---

## 1. Context and Problem Statement

FrankenRemote's core mission is a responsive, tailnet-native remote workstation prioritizing freshness and low latency over throughput vanity metrics. Achieving sub-15ms input-to-photon latency requires hardware-accelerated HEVC capture and encoding (NVENC, VAAPI, Apple VideoToolbox, Intel QSV / Media Foundation).

Plan §9.3, §9.4, and bead `fr-sw-encoder-evaluation-wkd` require an evaluation of software HEVC encoding as an opt-in rescue or verification fallback:
1. **`x265`**: Established, highly optimized C++ software HEVC encoder.
2. **`oxideav-h265`**: Emerging pure-Rust H.265 bitstream parser and encoder scaffold (version 0.0.10, MIT license).

We must establish whether software encoding can meet the real-time requirements, evaluate licensing and patent obligations, define constraints for the opt-in profile, and issue a definitive Adopt, Defer, or Reject verdict.

---

## 2. Evaluation Evidence and Measurements

### 2.1 Candidate Evaluation: `oxideav-h265` (v0.0.10)

- **Source & Identity**: Crate `oxideav-h265` v0.0.10, clean-room ITU-T H.265 implementation in safe Rust, MIT license.
- **Bitstream Interoperability**:
  - Encoded low-delay P-GOP stream (`IDR + 4 x TRAIL_R P`) at 320×240.
  - Bitstream parsed and validated byte-for-byte by external `ffprobe` (version 8.0.1): confirmed valid `hevc (Main), yuv420p(tv), 320x240, 25 fps`.
- **Measured Throughput (Release Build, x86_64 AMD Threadripper)**:
  - 5 frames at 320×240 QP 28 took **2.455 seconds per frame** (**0.41 fps** cold / ~2 fps warm).
  - Estimated 1080p throughput: < 0.1 fps (>10,000 ms per frame).
  - Lack of SIMD vectorization, lack of multi-threading in `LowDelayPEncoder`, and absence of GPU texture interop make real-time workstation encoding impossible.
- **Adversarial & Memory Behavior**:
  - Memory bounds are predictable (pure safe Rust, checked allocations).
  - However, the CPU latency violates the 50 ms display deadline by 50x to 100x.

### 2.2 Candidate Evaluation: `x265`

- **Licensing & Redistribution**:
  - `x265` is licensed under GNU GPL v2+ or proprietary commercial license.
  - Distributing a unified binary with `x265` statically or dynamically linked triggers GPL copyleft requirements. Running `x265` in a separate process does not circumvent derivative work or commercial distribution obligations under established copyright law.
  - **Patent Liabilities**: HEVC/H.265 is subject to substantial royalty and licensing terms across MPEG LA, Access Advance, and Velos Media pools. Distributing software encoders exposes distributors to patent licensing fees that do not apply when relying on licensed OS hardware encoders.
- **CPU & Power Envelope**:
  - Even under `ultrafast` preset with zero lookahead, 1080p30 encoding consumes 2–4 full CPU cores (200–400% CPU), causing extreme battery drain, thermal throttling, and fan noise on host laptops, directly contradicting the <0.1% idle and freshness mandates of FrankenRemote.

---

## 3. Decision

1. **Reject Software Encoders for Production Workstations**:
   - Neither `oxideav-h265` nor `x265` is accepted as a production media pipeline.
   - FrankenRemote will **never** silently fall back from failed hardware acceleration to a software encoder. If hardware HEVC encoding is unavailable, the host emits a typed refusal (`hardware_hevc_unavailable`).
2. **Constrain the Opt-in Developer Profile (`Backend::SoftwareExplicit`)**:
   - Exclusively enabled via explicit CLI/configuration flag (`--software-explicit`).
   - Hard ceilings enforced by `fr-media`:
     - Maximum resolution: **1280×720** (refuses 1080p or 4K).
     - Maximum frame rate: **30 fps**.
   - Diagnostics must visibly declare `software_encoding: true` and report CPU utilization per frame.
   - **Implementation status (corrected 2026-09-24):** neither ceiling nor the
     `software_encoding` diagnostic is enforced yet; the software profile encodes
     the selected display at its native size. `frd run` requires an explicit
     `--software-explicit` flag and otherwise refuses with
     `hardware_hevc_unavailable`.
3. **Retain `oxideav-h265` for Testing Only**:
   - `oxideav-h265` may be used as an offline test generator and bitstream validator in non-shipping test harnesses.

---

## 4. Revisit Conditions

This decision may be revisited only if:
1. `oxideav-h265` or another pure-Rust, permissive (MIT/Apache) encoder achieves real-time 1080p60 encoding with AVX2/NEON vectorization (<16 ms per frame on 4 cores) and verified zero-copy GPU staging.
2. An unencumbered, royalty-free profile emerges that satisfies the plan's strict line-budget and dependency criteria.

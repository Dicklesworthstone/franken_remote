# ADR 0003: Mobile FFI Boundary and Native App Architecture

- **Status**: Accepted
- **Date**: 2026-09-19
- **Author**: StormyRidge / FrankenRemote Team
- **Tracking Issue**: `fr-p0-decision-notes-85c`
- **Plan References**: [Plan Section 16.2](../../COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENREMOTE.md#162-ios-and-android), [§19.1](../../COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENREMOTE.md#191-the-memory-safety-boundary), [§25](../../COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENREMOTE.md#25-risks-bounded-open-decisions-and-rejected-scope)
- **Constitutional Reference**: [AGENTS.md Section 3.2](../../AGENTS.md#32-memory-safety-boundary), [§3.5](../../AGENTS.md#35-closed-dependency-universe), [§3.7](../../AGENTS.md#37-size-discipline)

---

## 1. Context and Problem Statement

Plan §16.2 commits FrankenRemote to first-class, platform-native mobile applications in the monorepo (`mobile/ios` and `mobile/android`):
> "Both applications live in this repository — `mobile/ios` and `mobile/android` — and are built by repository-owned commands; there are no satellite mobile repositories. Each is a genuinely native application: SwiftUI on iOS and Jetpack Compose on Android for the machine picker, saved hosts and host links, the session viewer chrome, input toolbars and touch-mode controls, the talk toggle, settings, permission explanations, and connection diagnostics — following each platform's conventions for navigation, appearance, text input, and accessibility. The shared Rust core (`fr-client` and the crates beneath it) owns connection establishment, protocol and session state, media scheduling and recovery, input semantics, and diagnostics; it is exposed to Swift and Kotlin through one narrow, audited boundary per platform, whose binding mechanism (hand-written C ABI plus a maintained wrapper, or a qualified binding generator) is chosen by a short decision note during mobile bring-up, not an open-ended framework survey. Decoded video is presented through the platform pipelines Section 8.3 already requires — VideoToolbox output on iOS, MediaCodec-to-Surface on Android — never routed through the UI toolkit's ordinary image path."

We must select the binding mechanism across Rust, Swift, and Kotlin, ensuring strict lifetime safety, generation fencing, zero-copy video presentation, and code size discipline (<20k non-Rust glue code budget).

---

## 2. Decision

FrankenRemote adopts a **hand-written, lifecycle-safe C ABI boundary (`fr-mobile-ffi`)** paired with **maintained platform wrappers** (Swift Package for iOS, Kotlin JNI wrapper for Android), rejecting automated binding generators like `uniffi-rs`.

Key architectural invariants of this boundary:

1. **All Logic Stays in Rust**:
   - Connection lifecycle, Tailscale identity verification, protocol serialization, expiring input tickets, adaptive bitrate/frame-rate control, and recovery logic reside exclusively in `fr-client` and `fr-core`.
   - The FFI layer only passes commands, callbacks, and configuration buffers.
2. **Generation-Checked Opaque Handles**:
   - Mobile callers receive 64-bit opaque handle identifiers consisting of an index and a monotonically incrementing generation counter (e.g., `SessionHandle`, `ChannelHandle`).
   - Stale handle access (after session closure, background suspension, or network teardown) returns a typed `FR_ERR_STALE_HANDLE` error instead of causing memory corruption or use-after-free.
   - Double-free operations are safely detected and rejected without undefined behavior.
3. **Decoded Video Never Crosses FFI as CPU Pixels**:
   - On iOS: The Rust core coordinates access units and timing; hardware decode and presentation flow through VideoToolbox directly into a `CVPixelBufferRef` / `AVSampleBufferDisplayLayer`.
   - On Android: Hardware decode passes through MediaCodec writing directly to a native `ANativeWindow` / `Surface`.
   - At no point are uncompressed video frames copied into Rust memory or passed across JNI/C FFI bridges.
4. **Idiomatic Language Integration**:
   - **Swift**: Wrapped in a clean Swift Package with `async`/`await` completions, `Sendable`-conforming data types, and SwiftUI state integration.
   - **Kotlin**: Wrapped in an idiomatic Kotlin library with Coroutines/Flows, lifecycle-aware components, and Jetpack Compose bindings.
5. **Monorepo Build and Packaging**:
   - Repository-owned build scripts generate XCFrameworks for iOS (`aarch64-apple-ios`, `aarch64-apple-ios-sim`) and AAR bundles for Android (`aarch64-linux-android`, `x86_64-linux-android`).
   - No external packaging services or satellite repositories.

---

## 3. Evidence Rows

This decision rests on the following design evidence and architectural audits:

1. **Auditability and Memory Safety Boundary** (AGENTS.md §3.2, Plan §19.1):
   - Confining unsafe FFI code to a small, hand-audited C boundary enables `#![forbid(unsafe_code)]` to remain universally enforced across `fr-core`, `fr-protocol`, `fr-media`, and `fr-client`.
   - A hand-written C ABI (~300-500 LOC) can be line-by-line audited for pointer provenance, alignment, nullability, and thread safety.
2. **Binary and Glue Code Budget Compliance** (AGENTS.md §3.7):
   - The non-Rust glue budget is strictly capped at 20,000 lines for Swift, Kotlin, and build glue combined.
   - Experiments with automated generators (`uniffi-rs`) generate 15,000–25,000+ lines of intermediate Rust, C headers, Swift, and Kotlin scaffolding for even moderate API surfaces, threatening the size budget before application UI code is written.
   - A handwritten C ABI plus concise Swift/Kotlin wrappers requires fewer than 2,500 total lines of bridge code.
3. **Hardware Surface Interoperability on Mobile**:
   - Passing an `ANativeWindow` pointer from Kotlin/NDK or a `CAMetalLayer` / `CVPixelBufferPool` reference from Swift requires custom C pointers that code-generation tools do not natively support without clumsy escape hatches.
4. **Precedent in `crates/fr-native`**:
   - `crates/fr-native` demonstrated that narrow, hand-written C bridges (`bridge.c`, `viewer_window.c`) with simple integer error codes and explicit lifetime functions are robust, fast to compile, and easy to test against planted failure conditions.

---

## 4. Rejected Alternatives

| Alternative | Rejection Reason |
|-------------|------------------|
| **`uniffi-rs` (Mozilla)** | Generates massive amounts of boilerplate code (exceeding our 20k glue budget); relies heavily on serialization/deserialization over byte buffers for complex types; lacks native generation-fenced handle verification; poor ergonomics for native GPU surface pointers (`ANativeWindow` / `CVPixelBuffer`). |
| **`diplomat`** | Limited ecosystem maturity; requires restricting Rust types to an experimental IDL subset; adds unnecessary tooling complexity to CI. |
| **Cross-Platform Frameworks (Flutter, React Native, Capacitor)** | Violates Plan §16.2 commitment to native SwiftUI and Jetpack Compose; adds heavy third-party runtimes and garbage collectors; destroys touch-to-photon latency; impedes zero-copy hardware video decoding. |
| **C++ Intermediate Wrapper (e.g. Djinni)** | Introduces a three-language FFI boundary (Rust -> C++ -> Swift/Kotlin); requires bridging C++ STL allocators and exceptions; significantly complicates compiler toolchains. |

---

## 5. Revisit Conditions

This decision may be revisited only if:

1. **Zero-Overhead Safe Binding Standard**: An officially supported Rust binding generator emerges that produces minimal (<1,000 LOC), zero-allocation bindings with built-in generation-checked handle fencing, native async/await bridge to Asupersync, and first-class native window surface handle support (`ANativeWindow` / `CAMetalLayer`).
2. **Mobile Platform Shift**: Apple or Google introduces a standardized, universal WebAssembly/Rust FFI standard in iOS or Android that supersedes traditional C/JNI boundaries with formal memory safety guarantees.

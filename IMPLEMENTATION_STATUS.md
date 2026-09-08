# Implementation status

Updated September 8, 2026. **Early Rust implementation, not an installable remote desktop.** The comprehensive plan remains the design authority; this file records implementation and evidence, not additional product scope. No application or live-transport/hardware phase gate is declared complete by the tests below.

## Encoded-media path now implemented

The source at `b2a2888e84601b9cb79be7fd248c54cc9aced67d` adds a complete bounded **encoded picture -> binary records -> fragment repair -> reference-ordered picture** path. It includes the new `fr-wire` crate and `fr-media::delivery::{SendCache, ReceivePipeline, MediaBudget}`. No runtime, foreign codec or external serialization dependency was added. The objects execute production packetization/reassembly policy; they are not mock codec implementations.

Sender ownership, reliable IDR startup, immutable progress announcements, out-of-order fragments, duplicate/conflict handling, bounded repair requests, reference retention, failed-decode fencing and generation replacement are implemented. Decoder-held compressed buffers continue consuming their reservation after leaving the receive queue. A repair of an old reference can unblock recent pictures without making the old picture fresh for display. Entire loss of the final picture is detectable from the reliable progress announcement.

See [PROTOCOL_MEDIA.md](PROTOCOL_MEDIA.md) for executable record layouts, [MEDIA_DELIVERY.md](MEDIA_DELIVERY.md) for integration and the reproduction command, and [CHANGELOG.md](CHANGELOG.md) for incremental commits. The rest of the session wire protocol, transport scheduling/credits, host identity adapters and native codec/capture workers remain incomplete.

### Current source verification

Source `b2a2888e84601b9cb79be7fd248c54cc9aced67d` also passed [GitHub Rust verification run 34237484950](https://github.com/Dicklesworthstone/franken_remote/actions/runs/34237484950). On Linux x86_64 with the pinned `nightly-2026-08-31`, `./scripts/verify.sh fast` passed formatting, all-target/all-feature compilation, strict Clippy with `-D warnings`, normal tests/doctests and example tests. `./scripts/verify.sh docs` also passed. There are **111 Rust tests**, zero failed or ignored: the previous 76 plus 10 wire tests, 13 receiver tests, 10 sender/receiver integration tests and 2 corpus-reader example tests. The new packet impairment tests run the real byte codecs and delivery owners with injected clocks; they are not Asupersync lab execution or a physical network benchmark.

### Real HEVC preservation and independent software decode

The opt-in `scripts/verify_hevc_delivery.py` lane passed locally on the same Rust sources with FFmpeg/ffprobe `7.1.5-0+deb13u1`. Both synthetic corpora were Main 8-bit 4:2:0, 640x360, generated at 30 fps with periodic IDRs. Delivery uses a virtual impairment clock, not a measured realtime throughput/latency claim.

| Corpus | Encoded bytes | Original records | Dropped video packets | Duplicate deliveries | Repaired packets | Independently decoded frames |
|---|---:|---:|---:|---:|---:|---:|
| 90 frames / 3 IDRs | 405,338 | 514 | 87 | 328 | 87 | 90, identical hashes |
| 240 frames / 8 IDRs | 1,083,917 | 1,364 | 222 | 893 | 222 | 240, identical hashes |

The whole source/delivered 90-frame corpus has SHA-256 `19c68ed85668520d38e0be513f191a0ed19e3a86cfc7f9f559572ae4c37b9683` on both sides. Both independent 90-frame decoded-hash files have SHA-256 `19b2bf195ee511db3132b80b6fda82d6745516bec586d43bda69a6d33caa2f4e`. Each lane checks all access-unit bytes and all decoded frame hashes, and requires nonzero drops/repairs. Decoder stderr was empty in both local corpus runs. Exact commands, versions and generated artifacts are retained by the lane in a fresh user-selected directory, never committed as screen recordings.

This adds **real HEVC byte-preservation and independent software-decode evidence**. It does not add a shipping FFmpeg wrapper, hardware encoder/decoder, capture source, browser presentation, live Tailscale/QUIC/H3 endpoint, physical latency measurement, or OS input injector. The Rust fixture diagnostic models decode completion after byte comparison; the separate FFmpeg decoder checks the delivered stream afterwards. Full parameter-set/DPB security validation before a production decoder is still required.

The next critical integration work is the qualified Asupersync transport composition and one real capture/HEVC/presentation path, joined to current authority and delivery owners. Repository task `fr-5nb` records the prior QUIC qualification defects; it needs fixes/requalification, not a second shipping QUIC implementation. `fr-fr-wire-framing-i0u` is only partially implemented because non-media message classes and its fuzz campaign remain outstanding. No Beads task is declared closed by this delivery pass; `br` was unavailable in this execution environment.

## Implemented slices

| Component | Present behavior | Boundary still requiring integration |
|---|---|---|
| `fr-core::ids`, `limits`, `time` | Distinct identities/generations, checked clock arithmetic, downward-only negotiated limits, checked geometry/allocation arithmetic | Qualified randomness, real host clocks, transport decoding, resource ownership across processes |
| `fr-core::authority` | Separate observation/readiness/control; exclusive deadlines; issue-time challenge renewal; submission-time authorization; refusal/closure cleanup; suspend fencing; clock-regression refusal | Tailnet identity and approval adapters, global controller arbitration, geometry/mapping checks, OS injection and actual held-key cleanup |
| `fr-core::input_sequence` | Lease-scoped monotonic consumed floor, bounded receipt ring, duplicate suppression after receipt eviction, single pending action, explicit uncertain/partial outcomes, no counter wrap | Attach to the authenticated ordered input stream and final submission gate; never reconstruct a ledger to resume an old lease |
| `fr-media` contracts | Checked declared configuration/geometry, capability/admission helpers, opaque surface/codec interfaces, frame metadata, copy accounting, test-only fake backend | Real capture, hardware encode/decode, HEVC syntax/reference validation, FFI ownership, device qualification |
| Diagnostic formatting | Access-unit `Debug` excludes compressed screen bytes; authority-owner `Debug` excludes live challenge/ticket/lease material; authority owner is not cloneable | Review every subsequent logging/trace boundary; metadata/IDs remain sensitive deployment information even where they are not bearer credentials |
| Repository verification | Pinned-nightly formatting/check/Clippy/test lane, feature-gated media contract tests, documentation link checks, explicit blocked full/release states | Native builder and hardware matrix, UBS, fuzzing, independent transport interoperability and end-to-end application evidence |

The core and media crates add no new external runtime or codec dependency in this slice. Neither the declared workspace Asupersync dependency nor a fake media backend demonstrates an integrated asynchronous transport or HEVC encoder.

## Earlier authority/input source evidence

The source commit [e7d57d5a1a284ec0b8da6374d3eda8613167c1e1](https://github.com/Dicklesworthstone/franken_remote/commit/e7d57d5a1a284ec0b8da6374d3eda8613167c1e1) passed [Rust verification run 34231571592](https://github.com/Dicklesworthstone/franken_remote/actions/runs/34231571592), job `102078753267`, on Ubuntu 24.04.4 x86_64 with `nightly-2026-08-31` (`rustc 1.100.0-nightly`, `908501772`, 2026-08-30).

| Gate | Result |
|---|---|
| `cargo fmt --all --check` | Passed |
| `cargo check --workspace --all-targets --all-features --locked` | Passed |
| `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` | Passed |
| `cargo test --workspace --all-features --locked` | Passed: 47 core unit tests, 17 media unit tests, 9 media contract tests, 3 compile-fail doctests; 76 total, zero failed or ignored |
| `./scripts/verify.sh docs` | Passed at the source evidence commit |

The nine media contract tests use an explicitly test-only fake backend. The authority/replay tests inject host-clock values into the real policy implementation. Neither category is a live Asupersync lab run, a GPU benchmark, or proof that an OS accepted/released input. Compile-fail doctests check identifier separation and that a live authority owner cannot be cloned.

UBS, Miri, fuzz-engine campaigns, macOS/Windows native builds, browser/mobile execution, live Tailscale/QUIC/H3, hardware capture/HEVC, and installer/rollback qualification were **not run in this implementation pass**. The `full` lane requires UBS and returns a blocked result when it is absent. The `release` lane intentionally remains blocked.

## September 8 implementation changes

| Commit | Change |
|---|---|
| `0de2ca6` | Replace the pre-implementation verification refusal with actual workspace gates; add the missing `fr-media` lockfile entry |
| `2f9e05f` | Fix refused-session credentials, challenge carry-over, issue-time renewal deadlines, exact-deadline submission, stale-view ticket reuse, observation/control coupling, suspend and clock-regression fencing |
| `53607b5` | Resolve pinned-nightly formatting and existing lint failures without weakening checks |
| `2ceb847` | Redact compressed desktop payloads from diagnostic formatting and add bounded-output regressions |
| `752d106` | Add bounded lease-scoped replay accounting and an authority-expiry integration regression |
| `f5f40f9` | Redact authority credentials and remove `Clone` from live authority owners |
| `e7d57d5` | Assert must-use admission outcomes, format the new regressions, and obtain the complete passing source run above |

The review-only source-maintenance workflow emits a diff and Git blob objects for inspection; it does not commit, update refs, create branches, or open PRs. Machine-applicable repairs were reviewed and committed separately. Repository-owned verification commands remain the source of truth, not the existence of a green historical job.

## Authority/input integration obligations

Keep one serialized authority owner per remote session under the OS share-session owner. The containing owner must authenticate the principal, enforce optional local approval, reserve the single global controller slot, and perform held-key cleanup before handoff. Sample the host clock immediately before submission; a previously accepted packet or cached timestamp is not current authorization. On actual suspend/wake, call the explicit suspend fence even when the selected clock has not advanced.

For each reliable input action, consume its sequence before any possible external effect, then recheck live observation/control, the ticket, geometry, and viewport mapping immediately before injection. Record whether submission happened, partially happened, was refused/expired/cancelled before submission, or has an unknown effect. A failed or uncertain predecessor fences dependent actions. Receipt eviction never authorizes re-execution. Release-only cleanup belongs to the local input owner, not a retry of the remote action.

These owners still need bounded protocol/input decoding and a real host-side submission boundary while the Phase 0 native transport/media experiments continue. Do not turn additional pure-state tests into a claim of a working desktop or defer the live endpoint/codec risks until after building clients.

Existing Beads task records were not rewritten by this connector-only implementation pass. Reconcile ownership and completion through `br` on the development checkout; the source and retained test evidence above identify what actually landed.

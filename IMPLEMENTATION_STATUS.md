# Implementation status

Updated September 8, 2026. **Early Rust implementation, not an installable remote desktop.** The comprehensive plan remains the design authority; this file records implementation and evidence, not additional product scope. No application or live-transport/hardware phase gate is declared complete by the tests below.

## Native-owner cancellation and returned input results

The native owner now checks parent runtime cancellation at each final native
submission and projects actual receipts into the existing `InputResult` codec
using the originating request's immutable binding. Late results, partial/unknown
text effects, receipt eviction and cancelled waits retain explicit semantics.
[INPUT_AGENT_RESULTS.md](INPUT_AGENT_RESULTS.md) records the implementation,
real XKB/XTest/Asupersync integration tests, reproduction commands and exact
local verification scope. This extends the canonical owner/watchdog rather
than adding another actor or runtime. Broader transport and application gates
remain open; the local test evidence is not a fresh remote CI result.

## Input result receipts

Sources `7b6fcdcf37d32e2cdf23e0f948fe933c1245ab66` and
`3577eb306f79839eb73251a5b41108f1540811e2` implement the host-to-viewer
`InputResult` kind (0x0048) in [fr-wire](crates/fr-wire/src/input_result.rs).
The allocation-free codec checks the attached channel/session/lease, separates
pointer and reliable-action sequence spaces, enforces negotiated record limits,
and rejects contradictory stage/outcome/count/reason combinations. Conversion
from a completed core receipt preserves the confirmed native-operation prefix
and an uncertain next operation; it never promotes submission to observation.
The fixed result carries no typed text, key identity, coordinates or ticket.
[PROTOCOL_INPUT.md](PROTOCOL_INPUT.md#inputresult-0x0048) defines its exact bytes.

Ten independently constructed byte fixtures and ten receipt tests cover the
outcomes, stages, malformed records, truncation, bindings, extensions and bounds.
The existing sixteen wire-to-submission fault tests now encode and decode their
actual core receipts. The single-byte mutation sweep is bounded adversarial
testing, not a coverage-guided fuzz campaign or independent-peer interoperability.
Those fault tests use an explicit recording sink, not live desktop input.

### Receipt revision verification

Exact source `3307d194b57819edf29f40fbd149c46eff6a1e2e` (including the receipt
changes and the separately landed watchdog) passed the following gates on
September 8. Builds ran serially on RCH worker `hz3`, Linux x86_64, with
`nightly-2026-08-31`, FFmpeg package `7:8.0.1-3ubuntu2` and Xvfb
`2:21.1.22-1ubuntu1`. Each build/test command below used
`RCH_REQUIRE_REMOTE=1 RCH_WORKER=hz3 rch exec --source-content-receipt --`.
All four returned remote exit 0 and receipts matching every one of the detached
checkout's 148 tracked files; before/after tracked-file hashes were unchanged.

| Command after the RCH prefix | Result |
|---|---|
| `cargo check --workspace --all-targets --all-features --locked -j 4` | Passed |
| `cargo clippy --workspace --all-targets --all-features --locked -j 4 -- -D warnings` | Passed |
| `cargo test --workspace --all-features --locked -j 4` | 236 passed, zero failed/ignored/filtered |
| `cargo test --workspace --all-features --locked --examples -j 4` | Examples compiled; two corpus-reader tests passed, zero failed/ignored/filtered |

These are the Rust commands from `scripts/verify.sh fast`, invoked separately
through RCH. Local `RCH_CARGO_WRAPPER_BYPASS=1 cargo fmt --all --check` and
`./scripts/verify.sh docs` also passed. The native tests exercised Xvfb and
software HEVC; this is not physical-GPU qualification, a live Tailscale session,
or independent wire interoperability. Full logs and RCH source receipts are
retained under `/tmp/fr-input-result-validation-3307d19-canonical/` on the
dispatcher, with their build IDs and receipt hashes recorded in the framing bead.

UBS scanned all five changed Rust files and **exited 1**, with 26 critical and
282 warning findings. Self-review classified the critical findings as 25 generic
binary `decode` sites misidentified as JWT handling and one existing explicit
test panic; warnings inventory test assertions/unwraps, fixture copies and
bounds-checked slices. No source suppression was added. `UBS_SKIP_RUST_BUILD=1`
disabled its duplicate local Cargo subprocesses because the explicit RCH gates
above supply those checks. The scan remains nonzero, not a clean full-lane pass;
its complete output is `/tmp/fr-input-result-ubs.txt`. Independent review has
been requested but has not returned.

An earlier mutable-checkout run returned four successful Cargo exits but changed
source during execution; it is indeterminate as a combined revision-bound gate.
It remains retained negative evidence, superseded by the detached run above.

`HeldState`, other missing message classes, the fuzz harness/campaign, and actual
session response transport remain open. Evicted receipts, obsolete pointer state
and missing process replies cannot be converted into fabricated zero-effect
results. `fr-fr-wire-framing-i0u` remains in progress with its original acceptance
criteria. Its new dependency `fr-rmk` records an existing contradiction:
PROTOCOL.md requires an explicit HID page/usage pair, while the key codec and
PROTOCOL_INPUT.md use an implicit keyboard page. Existing key bytes are preserved
pending explicit resolution under the repository's constitutional hierarchy.

## Earlier input framing, final submission, and native X11 effects

This subsection retains the verification and integration limits of its named
earlier revisions. Later XKB keyboard work, the `08c6f72` independent authority
monitor and the `3307d19` Asupersync watchdog are separate changes; their presence
does not establish a qualified input-agent process or host/client lifecycle.

The September 8 input implementation adds the missing path from bounded action
records to final authority checks and a real platform sink, without enabling an
unqualified network service. [PROTOCOL_INPUT.md](PROTOCOL_INPUT.md) specifies the
byte layout and [NATIVE_INPUT.md](NATIVE_INPUT.md) documents the native boundary.

| Source commit | Implemented behavior |
|---|---|
| `d9f8ea7bc3a2154637f998f5395cf30d091b24e5` | Seven allocation-free FRD0 input codecs: keys, buttons, absolute pointer state, cumulative relative checkpoints, scroll, committed UTF-8 and mode changes. Full session/lease/ticket/view bindings; authenticated direction/channel checks; independent exact byte fixtures. |
| `3b8f7e9742191677979a3729c88afcdc30834a30` | One lease-owned final submission path joins the authority and replay ledger. Each native call checks the actual clock after platform preflight. Pointer barriers, mode-ticket fencing, scalar-bounded text, confirmed-prefix/unknown-effect receipts and release-only cleanup are implemented. |
| `eca3253c0a6c2810b75d0c5233c277d58d6efa49` | Opt-in Linux X11/XTest pointer/button adapter. Actual wire records drive native motion/drag/release through the same submission owner, with X11 state queries verifying effects. Other native input capabilities explicitly refuse. |
| `8f501c0691b20e3b8b7da436af5c0ccb614e1479` | Fix native HEVC allocation admission to account for FFmpeg's aligned rows without loosening the Rust coded/crop/profile/DPB guard. The crop regression retains its original assertions and includes additional awkward dimensions. |

This builds on the existing supervised native-media and HEVC-guard work already
present at `812f35c398f35ae1ba78d9ccddace278e9180195`; those earlier changes are not
attributed to the input implementation. Historical evidence below remains scoped
to its named source revision, not the current workspace. Concurrent compound-repeat
and reversible-preparation cleanup support at `10b51d7` is preserved; it is not
included in the verification counts below.

### Input verification and remaining integration

The pinned nightly passed 157 core/wire/media tests, including seven new input
codec tests and 16 byte-codec-to-submission fault tests, plus strict Clippy for
those crates, formatting and documentation checks. Four live Xvfb input tests
and one local-display-selector unit test also passed by compiling the exact
first-party sources directly with the pinned compiler. Native-library Cargo
Clippy and test Clippy with the workspace's existing lint settings passed.
The direct native tests are real X11 API effects with synthetic local authority
grants, not Tailscale admission or physical-device qualification.

The `8f501c0` native sources additionally passed all nine native unit tests
(including the display-selector test above) against matching Debian FFmpeg
7.1.5 headers/runtime, plus the 12-frame X11 capture -> HEVC -> wire delivery ->
decode -> presentation/readback example. The codec crop test exercises five
awkward geometries, including 1366x768. A fresh negative-control build changing
only the C bridge back to its pre-fix `eca3253` source failed at 1366x768 frame 0
with `Allocation`; the otherwise identical fixed build passed. These direct
pinned-compiler executions used the actual first-party source and native APIs,
not a substitute codec or runtime. They are not a full Cargo-workspace test run.
The C compiler reports existing misleading-indentation warnings in the compact
bridge; they were retained, not hidden or relabeled as a warning-free C build.

A local full Cargo build was killed while compiling Asupersync under the 4-GiB
execution limit, including a serialized-backend retry and the later all-target
metadata-only check. Full-workspace Clippy could not run after that check failed.
The full GitHub native run
on `3b8f7e9` ([run 34274407094](https://github.com/Dicklesworthstone/franken_remote/actions/runs/34274407094))
passed formatting, compilation and strict Clippy before exposing the cropped-frame
allocation failure on FFmpeg 6.1. The `8f501c0` full native-workspace rerun
([run 34276085651](https://github.com/Dicklesworthstone/franken_remote/actions/runs/34276085651))
completed successfully on Ubuntu 24.04 x86_64 with FFmpeg 6.1.1-3ubuntu5
and the pinned `nightly-2026-08-31`. `./scripts/verify.sh fast` passed formatting,
workspace all-target/all-feature compilation, strict Clippy (`-D warnings`),
workspace tests/doctests and example tests, with zero failed or ignored tests.
`./scripts/verify.sh docs` also passed. The cropped-frame regression now passes
on both tested FFmpeg versions. This complete Cargo-workspace result validates
exact source `8f501c0`, not later concurrent source changes. The earlier failed
run remains negative evidence rather than being relabeled as passing.

`InputSession` does not start a watchdog. The interactive agent must independently
service lease expiry and local revoke, connect focus/lock/suspend/display lifecycle,
and arbitrate the single global controller before this becomes an unattended
control path. The X11 connection belongs in that input process, never the broker
or media worker. Xlib can block or terminate; entered native calls cannot be
rolled back and failed-process key release remains uncertain. Keyboard mapping,
repeat ownership, committed text, relative motion and scroll remain unsupported
in this native adapter even though their bounded wire/core paths exist.

The next critical join is qualified Tailscale identity/transport plus the real
input-agent watchdog and client lifecycle. HeldState encoding, session response
transport and native keyboard/text qualification also remain open. InputResult
encoding is now implemented as described above. No installable desktop,
live QUIC qualification, hardware latency result or Phase 1 completion is claimed.
The existing beads `fr-fr-wire-framing-i0u`, `fr-p1-input-pipeline-ay1`,
`fr-p1-session-agent-iq3` and `fr-p2-host-linux-4e4` are only partially implemented.
Their original acceptance criteria remain in force. Those earlier input passes
did not close or reassign beads because `br` was unavailable in that environment;
the receipt pass above claimed the framing bead without closing it.

## Encoded-media path now implemented

The source at `b2a2888e84601b9cb79be7fd248c54cc9aced67d` adds a complete bounded **encoded picture -> binary records -> fragment repair -> reference-ordered picture** path. It includes the new `fr-wire` crate and `fr-media::delivery::{SendCache, ReceivePipeline, MediaBudget}`. No runtime, foreign codec or external serialization dependency was added. The objects execute production packetization/reassembly policy; they are not mock codec implementations.

Sender ownership, reliable IDR startup, immutable progress announcements, out-of-order fragments, duplicate/conflict handling, bounded repair requests, reference retention, failed-decode fencing and generation replacement are implemented. Decoder-held compressed buffers continue consuming their reservation after leaving the receive queue. A repair of an old reference can unblock recent pictures without making the old picture fresh for display. Entire loss of the final picture is detectable from the reliable progress announcement.

See [PROTOCOL_MEDIA.md](PROTOCOL_MEDIA.md) for executable record layouts, [MEDIA_DELIVERY.md](MEDIA_DELIVERY.md) for integration and the reproduction command, and [CHANGELOG.md](CHANGELOG.md) for incremental commits. The rest of the session wire protocol, transport scheduling/credits, host identity adapters and native codec/capture workers remain incomplete.

### Earlier encoded-media source verification

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

## Earlier authority/media-contract slices

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

The input framing and final submission owner above now join those policies to an explicit X11 pointer/button path. The authenticated host/client session, independently progressing input watchdog and native lifecycle integration remain unfinished while the Phase 0 transport/media experiments continue. Do not turn policy tests or Xvfb effects into a claim of a working remote desktop.

The earlier implementation passes did not rewrite Beads task records because `br` was unavailable. Their broader original acceptance criteria remain intact; the receipt pass above records its claim and blocking protocol defect explicitly. The source and revision-scoped evidence identify what actually landed.

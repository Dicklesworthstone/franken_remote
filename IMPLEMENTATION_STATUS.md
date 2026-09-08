# Implementation status

Updated September 8, 2026. **Early Rust implementation, not an installable remote desktop.** The comprehensive plan remains the design authority; this file records implementation and evidence, not additional product scope. No phase exit gate is declared complete by the core tests below.

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

## Verified source evidence

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

## Integration obligations and next slice

Keep one serialized authority owner per remote session under the OS share-session owner. The containing owner must authenticate the principal, enforce optional local approval, reserve the single global controller slot, and perform held-key cleanup before handoff. Sample the host clock immediately before submission; a previously accepted packet or cached timestamp is not current authorization. On actual suspend/wake, call the explicit suspend fence even when the selected clock has not advanced.

For each reliable input action, consume its sequence before any possible external effect, then recheck live observation/control, the ticket, geometry, and viewport mapping immediately before injection. Record whether submission happened, partially happened, was refused/expired/cancelled before submission, or has an unknown effect. A failed or uncertain predecessor fences dependent actions. Receipt eviction never authorizes re-execution. Release-only cleanup belongs to the local input owner, not a retry of the remote action.

The next implementation slice should connect these owners to bounded protocol/input decoding and a real host-side submission boundary while the Phase 0 native transport/media experiments continue. Do not turn additional pure-state tests into a claim of a working desktop or defer the live endpoint/codec risks until after building clients.

Existing Beads task records were not rewritten by this connector-only implementation pass. Reconcile ownership and completion through `br` on the development checkout; the source and retained test evidence above identify what actually landed.

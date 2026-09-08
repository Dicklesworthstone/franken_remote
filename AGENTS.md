# AGENTS.md — FrankenRemote Contributor and Coding-Agent Contract

This file is normative for humans and software agents working in this repository. It applies even when a task appears small. FrankenRemote is currently **spec-first and pre-implementation**: the repository contains a reviewed design, not code. The most dangerous contribution right now is a convenient abstraction, scaffold, or "temporary" shortcut that contradicts the final system.

---

## RULE 0 — THE FUNDAMENTAL OVERRIDE PREROGATIVE

If the repository owner tells you to do something, even if it goes against what follows below, YOU MUST LISTEN. The owner is in charge, not you.

## RULE NUMBER 1: NO FILE DELETION

**YOU ARE NEVER ALLOWED TO DELETE A FILE WITHOUT EXPRESS PERMISSION.** Even a new file that you yourself created, such as a test code file. Agents have a terrible track record of deleting critically important files or throwing away expensive work. You have permanently lost any and all rights to determine that a file or folder should be deleted. **ALWAYS ASK AND RECEIVE CLEAR, WRITTEN PERMISSION FIRST.**

## Irreversible Git & Filesystem Actions — DO NOT EVER BREAK GLASS

1. **Absolutely forbidden commands:** `git reset --hard`, `git clean -fd`, `rm -rf`, or any command that can delete or overwrite code/data must never be run unless the user explicitly provides the exact command and states, in the same message, that they understand and want the irreversible consequences.
2. **No guessing:** If there is any uncertainty about what a command might delete or overwrite, stop and ask. "I think it's safe" is never acceptable.
3. **Safer alternatives first:** `git status`, `git diff`, `git stash`, and copies-to-backup come before any destructive option.
4. **Mandatory explicit plan:** Even after authorization, restate the command verbatim, list exactly what will be affected, and wait for confirmation.
5. **Document the confirmation:** Record the authorizing user text, the exact command run, and the execution time.

## Branch Policy

- Primary branch is `main`.
- Do not reference `master` in docs or scripts.
- Never modify the default branch history, rewrite public history, or force-push without explicit owner authorization.

---

## 1. Mission

Build a **tailnet-native remote workstation**: `frd` (host daemon and process-role family) and `fr` (client/CLI) that let a user open a machine on their Tailscale network and use its existing desktop — hardware-accelerated HEVC, no separate account or pairing ceremony, and a system that refuses to accumulate invisible latency.

The project values:

- freshness of useful state over throughput vanity metrics;
- workstation text quality over television video defaults;
- input authority that expires and ends cleanly over optimistic replay;
- explainable performance over impressive-but-opaque numbers;
- typed refusal over silent fallback, hidden degradation, or fabricated state;
- a small dependency surface over "helpful" integrations;
- retained evidence over confident prose.

## 2. Constitutional hierarchy

Before a material change, read the relevant portions of, in order of authority:

1. [`COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENREMOTE.md`](COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENREMOTE.md) — version 1.1; the review corrections in §1 and §27.1 are binding requirements, not commentary;
2. [`PROTOCOL.md`](PROTOCOL.md) once it carries normative content (today it is a status stub deferring to plan §17);
3. [`SECURITY.md`](SECURITY.md) and the threat model in plan §19;
4. this file;
5. the README, which is descriptive, never normative.

If these disagree, stop and surface the contradiction. Do not implement the most convenient interpretation. Open decisions the plan explicitly bounds (native GUI/windowing binding, exact FFmpeg wrapper revision) are resolved by the early experiments plus a short decision note — not by open-ended adapter competitions.

## 3. Non-negotiable construction rules

### 3.1 One runtime

- **Asupersync is the sole async runtime.** No Tokio, async-std, smol, or any dependency that drags an alternate runtime into production.
- Use regions, cancellation-aware operations, bounded channels, monotonic time, and the deterministic lab. Do not enable unrelated Asupersync database/messaging/metrics features merely because they exist.
- Cancellation is request → drain → finalize; dropping a future is not a complete protocol. Foreign driver calls do not acquire wall-clock cancellation just because a function receives `Cx` — configure blocking execution explicitly and supervise potentially stuck media work **outside the authority path**.
- If the native QUIC or WebTransport composition fails qualification, the answer is bounded, counted upstream work plus the labeled WSS profile — **never a second QUIC stack or runtime inside FrankenRemote**.

### 3.2 Memory-safety boundary

- All project-owned protocol, policy, admission, scheduling, and session code uses `#![forbid(unsafe_code)]`.
- Unsafe code is confined to named platform/media boundary crates (`fr-ffi`, per-OS adapters) with narrow safe interfaces, documented ownership, callback lifetimes/threading, pointer provenance, buffer padding, and shutdown order.
- Media work is isolated from input authority: a hostile or hung codec must never retain an input lease, hold certificate keys, reach the approval endpoint, or take down the broker.
- FFmpeg types never cross into session or wire layers. The wrapper implements the real send/receive state machine (drain output, retain reference-counted inputs, distinguish backpressure/EOF/device-loss/fatal), keeps configure/encode/decode off the authority thread, and treats capture buffers as borrowed until the platform's documented release point.
- A worker process is crash/hang isolation, not a security sandbox; where a real OS sandbox is not enforced, record the remaining trust explicitly instead of claiming isolation.
- Client decoders ingest potentially hostile host-generated media and get the same bounds and containment analysis as host encoders.

### 3.3 One video codec, one audio codec

- **HEVC only** (Main, 8-bit, 4:2:0 baseline; extensions by positive capability negotiation). **Opus only** for audio, with a small explicit `libopus` exception in the dependency record.
- No second video codec may enter the tree for any reason, including browser gaps ("WSS can solve a transport gap; it cannot solve a missing codec — publish the limit").
- No HEVC encoder is written from scratch. The media interface (`Encoder`, `Decoder`, `GpuSurface`, `EncodedAccessUnit`, `CodecConfiguration`, `MediaCapabilities`) stays replaceable; pure-Rust candidates are evaluated behind it, never load-bearing for the schedule.
- Capability comes from probes plus a real encode/decode of a representative workload — never from a vendor name, an API accepting a request, or a nominal "HEVC supported" bit.

### 3.4 Tailscale-only identity and connectivity

- Identity and admission come from the installed Tailscale client's authenticated local metadata (LocalAPI/WhoIs/status), pinned to tested versions and sharing/tagging fixtures. **Never** from reverse DNS, client-supplied hostnames, source-prefix matching (`100.64.0.0/10` on the wrong interface), email-domain equality, or zero-valued sharer fields.
- Missing or ambiguous membership evidence is a typed refusal (`tailnet_membership_unverifiable`), not permission to weaken policy. Never silently write or broaden a tailnet's grants.
- Bind only to current tailnet node addresses with a platform-qualified ingress boundary. No FrankenRemote account, PIN, pairing database, ICE/STUN, embedded VPN, or public relay — ever.
- No application operation is admitted in 0-RTT. No accept-any-certificate path exists, including under expiry pressure. Bearer material never appears in URLs, host links, process arguments, or logs.

### 3.5 Closed dependency universe

- The essential graph: Asupersync at an audited revision, the smallest selected serialization/CLI utilities, narrow OS bindings, **one** pinned FFmpeg binding family for the desktop targets that need it, Opus, and browser bindings.
- Forbidden by convenience: Tokio, libwebrtc, Electron or bundled Chromium, a Go Tailscale embed, application server frameworks, databases. Do not import a huge Franken crate to save a few hundred lines of integration code.
- Inspect transitive features on every target: a browser build must not inherit desktop FFI; the daemon must not inherit GUI dependencies. Track direct, transitive, native, build-only, and test-only dependencies separately — test-only independent QUIC/HEVC peers never become shipping libraries.
- No network downloaders in `build.rs`; no unsigned prebuilt native libraries accepted on filename match; native libraries load from protected package paths only.

### 3.6 Pinned nightly, deliberately

- Use the exact dated nightly in [`rust-toolchain.toml`](rust-toolchain.toml) (no floating `nightly`). A toolchain advance is a material change: run the target matrix, record regressions, retain a tested rollback toolchain.

### 3.7 Size discipline

- **180k target / 240k planned maximum / hard stop below 250k** handwritten Rust lines, tests and project-induced upstream work included. Separate allowance of at most 15k for JS/Swift/Kotlin/build glue; product logic does not migrate there to evade the limit.
- One fixed counting command lives in the repository once code exists; handwritten Rust, generated bindings, non-Rust glue, vendored source, and upstream changes are reported separately. The counting method is never redefined near the end. When core delivery approaches the ceiling, optional scope is cut first — never security or qualification tests.

## 4. Authority, session, and input rules

- Observation authority, media readiness, and input authority are **separate state variables**. Receiving a first frame never grants control. Optional local approval gates **any new observation** (pixels, thumbnails, audio, clipboard, semantic data) as well as control — read-only is not an approval bypass.
- Authority uses host-monotonic-clock challenge/renewal (one-second cadence, three-second provisional deadlines as the starting point). Expired leases are terminal; a delayed heartbeat cannot resurrect one. Reacquisition is a new grant.
- The input agent re-checks lease expiry, authorization, and generations **immediately before every OS submission**. Input-validity tickets (0.5–1.5 s starting range, always bounded by the lease) expire actions independently of connection liveness; a timed-out action is reported refused/expired, never transparently retried with a fresh ticket.
- Distinct typed generations fence host boot, OS session, remote session, input lease, display geometry, codec configuration, recovery chain, and viewport mapping. Stale-generation datagrams, callbacks, repair requests, and channels are rejected — numeric ID reuse never revalidates them.
- Teardown order is fixed: revoke input authority → release remotely held keys/buttons → invalidate generations → stop capture admission → cancel cooperative tasks → drain bounded sends → kill a stuck foreign worker if necessary → publish closure. Fence first, then clean up.
- Acknowledgements name their stage: **admitted** by FrankenRemote, **submitted** to the OS API, or **observed** through instrumentation. Never label any of these "exactly once execution." Committed external effects (text already typed into the OS) are never silently discarded from a result; releasing modifiers is cleanup, not rollback.
- Shared media pipelines belong to the OS share session's region, not to any one viewer. Closing one viewer never cancels another's encoder; handoff is serialized in the single authority owner, never a check-then-set race across broker tasks. Local revoke has priority and never waits for a round trip, a slow viewer, or a media callback.

## 5. Media, freshness, and recovery rules

- **Every stage has a bounded queue and a defined rule for obsolete work** — including hidden stages: codec-internal surfaces, packet caches, QUIC/WSS send buffers, browser decoder output, renderer-held frames, shared-viewer retention. Bound counts **and** bytes; metadata allocations count too.
- Respect codec references: raw captures are replaceable; encoded reference frames are not. Separate "skip presentation" from "discard decode/reference state"; never free a surface a driver still owns to satisfy a queue metric.
- Distinguish **age of last pixel update** from **age of last trustworthy source observation**. A network heartbeat is not capture freshness. Sustained unknown/stale presentation suspends input before another action lands on an untrustworthy view. Never label an arbitrarily old frame fresh because it eventually arrived; never invent zero-cost freshness evidence.
- Loss recovery uses **separate deadlines for display freshness and reference usefulness**, a byte/time-bounded sender fragment cache, clamped repair requests, and a dedicated bounded reliable stream for startup/recovery IDRs. Never pass incomplete pictures or known-broken dependencies to a decoder hoping concealment saves you. A failed recovery gate is not "future FEC work."
- The startup handshake is non-circular: `DecoderConfiguration` → `DecoderConfigured` (API configured, not first frame) → `RecoveryAccessUnit` (verified IDR) → `FirstFrameDecoded`/`PresentedState`. Configuration, first decode, and visible presentation are distinct milestones with their own timeouts.
- Idle means idle: a static screen stops video except genuine refresh/refinement, with bounded source-verification. No endless dummy video while claiming zero idle encoding. The adaptive controller is deterministic (hysteresis, bounded steps, dwell times) and replayable from sanitized traces.
- Do not add an AI model to the critical path. Do not synthesize application responses or hide latency by pretending an action succeeded.

## 6. Security rules

- Threat model per plan §19: protect against unrelated network clients, shared external tailnet principals, malicious web origins on admitted machines, malformed protocol/media, stale worker callbacks, local unprivileged IPC forgery, and resource exhaustion by admitted peers. The selected desktop user, host OS, installed Tailscale authority, and GPU/media stack are explicit trust limits — never silently claimed to be isolated by Rust or a UID check.
- Browser ingress: exact-Origin checks for state-changing requests and socket establishment; short-lived one-use nonces via origin-checked same-origin HTTPS bootstrap; role-specific one-use attachment tickets for every auxiliary channel; restrictive CSP, `frame-ancestors 'none'`, no third-party scripts, no persistent service-worker authority. A bare session ID never attaches a second socket. Missing Origin is not an alternate authentication mode.
- All length/stride/product arithmetic is checked before allocation and before FFI. Validate VPS/SPS/PPS against the admitted subset before decoder configuration; reject unannounced parameter changes. Limits live in one tested structure (starting points in plan §17.2).
- Rate-limit and bound everything pre-admission: handshakes, codec probes, cursor uploads, recovery requests, half-attached channels, pending approvals, worker restarts. Revoke/expiry stays serviced fairly during floods; no high-priority queue is unbounded.
- Clipboard content, typed text, screen pixels, credentials, TLS keys, and session nonces never appear in logs, error messages, URLs, or diagnostic traces. Diagnostic export is sanitized (hostnames, paths, window titles, addresses, library error strings) and bounded.
- No admitted tailnet peer can, through the desktop protocol: change the shared OS user, enable audio globally, disable approval, modify tailnet policy, install codecs, run arbitrary commands, or update the host binary.

## 7. Honest claims and evidence

- **Do not describe a proposal as implemented.** The entire plan is proposals; the README repeats this. As slices land, true documents up in place with revision-bound evidence.
- Evidence categories are separate and non-fungible: source reviewed / builds passed / simulated properties passed / independent wire interoperability passed / hardware measurements passed. A capability row is `passed`, `failed`, `blocked`, or `not tested`; an untested row is never "supported with caveats."
- Performance numbers state their measurement scope (process family, GPU memory, cold/warm, direct/relay, capture path, refresh) or they do not ship. Report Tailscale's cost separately — neither charged to FrankenRemote nor hidden.
- Unsupported behavior is a typed refusal with a specific reason (`permission missing`, `no supported HEVC decoder`, `tailnet policy blocked`, `browser transport degraded`, …) — never a silent fallback, a generic "browser supported" badge, or an oscillating retry.
- Truthful null results and explicit blocked reports naming the exact missing thing are successful outcomes. Unsupported claims are worse than silence. Never silence stderr in an evidence-bearing command.
- Forbidden: faked tests, fixtures/mocks presented as live proof, weakened assertions, golden regeneration to force green, hard-coded success paths, `todo!()`/`unimplemented!()` in commits, editing a spec or gate instead of implementing it, narrowing scope while claiming full success, splitting work to harvest closures.

## 8. Final-abstraction slice doctrine

A new crate/module appears only with one real vertical slice of its final abstraction.

Forbidden substitutes include:

- an empty crate with future TODOs;
- a mock codec wired as the production media path (a mock codec explicitly does not satisfy the Phase 1 gate);
- a loopback transport adapter presented as QUIC/WebTransport interoperability evidence;
- a manually-advanced handshake test presented as TLS/security qualification;
- an unbounded queue "to be limited later";
- an input path without expiry "for now";
- a second competing wrapper implementation kept "just in case";
- a benchmark-only optimization without the queue/ownership invariants.

A subset is acceptable when its unsupported surface is typed and the implemented path already has the final ownership, failure, cancellation, generation, and bounds discipline. Work in small real vertical experiments, not a large scaffold.

### 8.1 Workspace bootstrap (read before creating the first crate)

There is deliberately **no root `Cargo.toml` yet**: a virtual workspace with zero members breaks every cargo command in the checkout. When the first real crate lands, create in ONE commit:

- `crates/fr-<name>/Cargo.toml` + `src/lib.rs` (with `#![forbid(unsafe_code)]` as line 1 for non-FFI crates);
- the root `Cargo.toml`: `resolver = "3"`, `members = ["crates/*"]`, `[workspace.package]` (edition 2024, `license-file = "LICENSE"`), a `[workspace.dependencies]` path entry per first-party crate (consumers write `fr-core.workspace = true`), `asupersync` pinned with `default-features = false`, and workspace lints (`unsafe_code = "forbid"`, with the named FFI boundary crates opting out explicitly and documenting why);
- `Cargo.lock`.

Because the glob makes every directory under `crates/` a workspace member, a directory there without a manifest breaks the whole checkout instantly. Never leave one, not even for a minute; build elsewhere and `git mv` in complete.

## 9. Required change workflow

For every material change:

1. Identify the owning plan section, invariant, and (once code exists) crate boundary.
2. Read the plan's relevant sections and cited sources rather than relying on README summaries.
3. State the final abstraction and the rejected shortcuts.
4. Write or update golden fixtures/reference behavior first where applicable.
5. Implement the smallest complete vertical slice.
6. Add success, refusal, cancellation, crash/retry, resource-bound, adversarial, and generation-fencing tests.
7. Update docs and any capability/limits tables the change touches; record negative evidence for disproven approaches.
8. Run the verification gates below.
9. Inspect the complete diff and stage only intended files by explicit path.

## 10. Mandatory checks after substantive changes

Once Rust code exists in the workspace:

```bash
cargo fmt --check
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
cargo test
ubs $(git diff --name-only)
```

`cargo test` is a hard gate: it must exit `0` before any change is handed off. If a check fails, fix root causes before handing off. Repository-owned scripts (a future `scripts/verify.sh` / `xtask`) become authoritative when they land; GitHub-hosted Actions availability is never part of the correctness contract. Pre-implementation, docs-only changes are verified by link/reference consistency against the plan.

Local builds: prefix cargo invocations with `RCH_CARGO_WRAPPER_BYPASS=1` on hosts where the rch offload wrapper is installed.

## 11. Git and repository hygiene

- Never stage unrelated files or use blanket staging (`git add -A`, `git add .`) in a mixed worktree.
- Keep build artifacts, native media bundles, benchmark scratch, capture/recording output, certificates, keys, and diagnostic exports out of source control.
- One commit describes one coherent slice when practical; reference the tracking issue/bead ID.
- `br` never runs git; after `br sync --flush-only`, stage `.beads/` explicitly.

## 12. Review checklist

A reviewer should be able to answer:

- Which plan section and invariant own this behavior, and does the change contradict a v1.1 correction?
- Is it Asupersync-only, safe-Rust-only outside the named FFI crates, and dependency-compliant on every target?
- Are observation, readiness, and input authority still separate, generation-fenced, and expiry-checked at submission time?
- Is every queue bounded in count **and** bytes, including hidden and shared-viewer stages, without violating surface/reference ownership?
- Do refusals carry typed, specific reasons; do acknowledgements name their stage?
- Are limits validated before allocation, FFI, and decoder configuration?
- Does any claim (support, latency, freshness, "zero-copy") outrun the retained evidence category?
- Does the change keep clipboard/input/pixels out of logs and traces?
- Is the line budget still credible, and was optional scope cut before core discipline?

## 13. Stop conditions

Stop and escalate instead of improvising when:

- documents contradict one another, or an implementation would contradict a plan §1/§27.1 correction;
- a path appears to need a second runtime, a second QUIC stack, a second video codec, or a new identity/pairing system;
- identity/membership evidence is ambiguous and the "fix" would be inference from DNS, source prefix, or reachability;
- an input path would execute without a submission-time expiry check, or replay uncertain actions across a reconnect;
- a queue cannot be bounded without violating codec-reference or driver ownership;
- media/FFI work would enter the authority path or an authority lock would span a blocking OS call;
- approval, origin, or ticket checks would be weakened to make a client work;
- a required Phase 0 experiment is being assumed instead of run;
- required evidence cannot be produced, or a gate would be edited instead of satisfied;
- the change would need an empty scaffold or a fake final abstraction.

The correct outcome may be a typed refusal, a plan amendment proposal, or a negative-evidence record. It is never silent architectural drift.

## 14. Beads (`br`) — dependency-aware issue tracking

This project uses [beads_rust](https://github.com/Dicklesworthstone/beads_rust) (`br`) for issue state once the tracker is initialized (`br init` creates `.beads/`, tracked in git). `br` is non-invasive — it never runs git.

```bash
br ready --unassigned --no-db --json              # Authoritative claimable work
br show <id> --no-db --json                       # Full record and dependencies
br create --title="..." --type task --priority 2 --json
br update <id> --claim --actor <AgentName> --json
br dep add <issue> <depends-on>                   # Dependencies
br sync --flush-only                              # Export DB mutations to tracked JSONL
```

- Priority: P0=critical … P4=backlog (numbers, not words). Types: task, bug, feature, epic, chore, docs, question.
- Claim only IDs present in `br ready --unassigned --no-db --json`; a graph score or recommendation is never authorization.
- After every Beads mutation: `br sync --flush-only`, then stage `.beads/` explicitly.

## 15. Session completion ("Landing the Plane")

Before finishing a work session you MUST:

1. File beads for remaining work (anything needing follow-up).
2. Run the quality gates (§10) if code changed.
3. Update issue status — close finished work, update in-progress.
4. `br sync --flush-only`, then `git add .beads/`.
5. Hand off: what changed, exact source commit, gates actually run and their results, remaining risks/gaps, and the next concrete claimable item if one exists.

A handoff that skips these is not a handoff.

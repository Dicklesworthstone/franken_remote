# FrankenRemote

**A tailnet-native remote workstation in Rust: open a machine on your Tailscale network and use its existing desktop, with hardware-accelerated HEVC, no separate account or pairing ceremony, and a system that refuses to accumulate invisible latency.**

> **Status: researched design, pre-implementation.** This repository currently contains the reviewed architecture and implementation proposal, not working software. The single source of truth for what is being built and why is [`COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENREMOTE.md`](COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENREMOTE.md) (version 1.3, 2026-09-07, all 27 sections re-reviewed with corrections integrated in place). Every number, latency target, protocol limit, and platform claim below is a **proposed engineering objective or experimental starting point taken from that plan — not a measured FrankenRemote result**. No code, benchmark, or qualification evidence exists yet, and this README will be trued up in place as implementation phases land.
>
> **License:** `LicenseRef-MIT-OpenAI-Anthropic-Rider` — the MIT license plus the OpenAI/Anthropic rider (see [`LICENSE`](LICENSE)). Because the rider withholds rights from named parties, these terms are **not** OSI-approved open source; the repository is source-available and must be described that way.

The two shipped names are:

- **`frd`** — FrankenRemoteDaemon, the host-side broker and its internal process-role family (interactive-session agent, on-demand media worker);
- **`fr`** — the FrankenRemote client and command-line interface.

The engineering thesis, from the plan:

> **Tailscale owns connectivity and machine identity. Asupersync owns asynchronous lifetimes and transport integration. Platform APIs and existing codec implementations own capture and hardware acceleration. FrankenRemote owns the small amount of policy that makes those pieces behave like one excellent remote workstation.**

---

## TL;DR

**The problem.** Remote desktop tools accumulate the wrong things: their own accounts, pairing databases, relays, and identity systems layered on top of a network that already has all of those; television-shaped video defaults (cinematic rate control, frame-count vanity metrics, multi-second hidden buffers) applied to terminal text and document work; and input paths that happily replay stale clicks into a desktop that has moved on. The result feels slow in ways nobody can explain, and stops in ways nobody can trust.

**The solution.** FrankenRemote deliberately owns almost nothing except policy. The installed Tailscale client provides connectivity, machine identity, admission evidence, and HTTPS certificates — there is no FrankenRemote password, PIN, pairing store, coordination service, or relay. Asupersync provides the sole async runtime, cancellation-aware ownership, bounded channels, and a deterministic lab. Platform APIs (ScreenCaptureKit/VideoToolbox, Desktop Duplication, PipeWire/portals, MediaCodec, WebCodecs) and a trimmed FFmpeg boundary provide capture and hardware HEVC. What FrankenRemote itself builds is the connective discipline: a freshness contract with bounded queues at every stage, input authority that expires and ends cleanly, codec-aware loss recovery, and diagnostics that answer "why does this feel slow?" directly.

## Executive decisions

Condensed from plan §1; each row is a settled decision, not an open question.

| Area | Decision |
|---|---|
| Video | **HEVC/H.265 only.** Main, 8-bit, 4:2:0 baseline; additional profiles by positive capability negotiation. No second video codec, ever, including "just for browsers." |
| Encoding | Fixed-function hardware encoders preferred. x265 is a possible opt-in software fallback candidate, not the name of the hardware path. |
| Audio | **Opus only.** Host playback audio; no microphone forwarding in v1. |
| Connectivity & identity | The **installed Tailscale client** and its authenticated local metadata, grants, and addresses. No embedded VPN, ICE/STUN, second identity system, or public relay. |
| Admission default | Sharing scope defaults to the host's **own tailnet user's devices**; admitting all tailnet members and tagged nodes is an explicit local choice. Optional local approval (off by default) gates **observation as well as control**. |
| Runtime | **Asupersync only**, including cancellation-aware ownership and deterministic tests. No hidden Tokio. |
| Native transport | Asupersync native QUIC after live-wire/security qualification: reliable control/input, datagram media with bounded reference-aware recovery. WSS is an explicit, labeled compatibility profile. |
| Browser transport | Actual HTTP/3 WebTransport interoperability, qualified early. Bounded secure-WebSocket channels are the explicit degraded fallback. |
| Safety | Safe Rust (`#![forbid(unsafe_code)]`) in protocol and authority code; a small audited unsafe/foreign boundary for OS and media APIs, with media work isolated from input authority. |
| Product scope | Selected full displays of an existing interactive desktop. No window-isolation promise, independent remote login, preboot access, USB/printer forwarding, or public-internet brokering. |
| Size discipline | ~180,000 handwritten Rust lines including tests; 240,000 planned ceiling; hard stop below 250,000. |

Four assumptions the plan makes explicit rather than burying: HEVC is not x265; HEVC-only does not imply every browser (the supported set comes from a real decode-and-present probe); high bitrate does not repair 4:2:0 chroma subsampling; and a daemon cannot erase OS consent boundaries.

## What can genuinely distinguish this project

Hardware video alone is not novel — Sunshine/Moonlight is the honest baseline to beat, not a deliberately weak VNC configuration. FrankenRemote earns its place in a narrower, coherent combination (plan §3):

1. **Freshness rather than throughput.** The optimization target is the age of the useful state shown to the user, not frames-per-second entering an encoder. Every stage has a bounded queue and a defined rule for obsolete work — a rule that respects codec references (raw captures are replaceable; encoded reference frames are not). The system never disguises a growing multi-second backlog as a healthy stream.
2. **Workstation quality rather than television defaults.** Terminal text, thin grid lines, and low-motion windows get spatial resolution preserved and redundant frame rate spent down. When motion stops, a sharper version of the current screen is sent ("settle-to-sharp"). Cursor movement doesn't force a full-screen encode; static desktops don't keep an encoder busy for an artificial 60-fps number.
3. **Input authority that ends cleanly.** Every controlling session owns a finite input lease with host-clock expiry checked immediately before OS submission. Revocation, disconnect, focus loss, and process failure have explicit release behavior. Reconnection never blindly replays old clicks, text, or modifier state.
4. **Explainable performance.** The product answers "why does this feel slow?" directly: relayed tailnet path, overloaded encoder, GPU-to-CPU copies, decoder backlog, thermal throttling, or queue growth — and a small sanitized metadata trace can replay session and adaptation decisions in Asupersync's deterministic lab without recording the screen.
5. **A small, useful FrankenSuite extension.** The reusable contributions are deliberately modest: a well-tested real-time transport profile, cancellation-aware foreign-worker supervision, bounded latest-value media channels, and stable observation/control receipts — upstreamed into Asupersync, not accreted into a distributed operating system.

## Architecture

```text
                           Existing Tailscale network
                                     |
                    Native QUIC / WebTransport / WSS
                                     |
                  +------------------v------------------+
                  | frd: identity, admission, sessions  |
                  | small idle footprint; no codec FFI  |
                  +---------------+---------------------+
                                  | authenticated local IPC
                  +---------------v---------------------+
                  | interactive-session agent           |
                  | OS consent, input lease, revoke UI  |
                  | independent input watchdog          |
                  +---------------+---------------------+
                                  | bounded control + media IPC
                  +---------------v---------------------+
                  | on-demand media worker              |
                  | capture -> GPU conversion -> HEVC   |
                  | capture and encoder co-located      |
                  +-------------------------------------+
```

The diagram shows ownership and supervision, not byte routing: the media worker sends bounded encoded output directly to the broker; the session agent retains consent and input authority. The user installs one product — separate process roles are an internal safety and OS-session requirement, not services to configure by hand.

Key structural commitments (plan §5–§7):

- **The idle daemon is genuinely idle.** `frd` owns listeners, configuration, the peer-identity cache, the session registry, and browser assets. It does not capture, encode, or hold GPU surfaces while nobody is connected (proposed objective: <25 MiB resident, <0.1% of one core).
- **Authority is small, typed, and generational.** Distinct typed identifiers fence host boot, OS session, remote session, input lease, display geometry, media configuration, and recovery chains. Stale work — old datagrams, late callbacks, replayed heartbeats, resurrected leases — is rejected by construction, not by hope.
- **Cancellation has a fixed teardown order**: revoke input authority → release remotely held keys/buttons → invalidate generations → stop capture admission → cancel cooperative tasks → drain bounded sends → terminate a stuck foreign worker if necessary → publish closure. Acknowledgements name their stage — admitted, submitted to the OS, or observed — and are never labeled "exactly once."
- **Shared pipelines belong to the OS share session,** not to whichever viewer joined first. Closing one viewer never cancels another viewer's encoder; a slow read-only viewer is degraded or disconnected independently rather than backpressuring the controller.

## Platform implementation map

Target adapters from plan §8.3 — a statement of intended integration paths, not a certification of any driver or OS version:

| Platform | Host capture | Preferred HEVC path | Client presentation |
|---|---|---|---|
| macOS | ScreenCaptureKit | VideoToolbox | VideoToolbox to a native GPU surface |
| Windows | Desktop Duplication (WGC when justified) | FFmpeg hardware bridge to NVENC / AMF / QSV | Qualified hardware decode + D3D presentation |
| Linux | PipeWire/portal on Wayland; explicit X11 adapter | FFmpeg hardware bridge to VAAPI / NVENC | Qualified GPU decode + native presentation |
| iOS | Client only | — | VideoToolbox + native display layers/Metal |
| Android | Client only | — | MediaCodec direct to Surface |
| Browser | Client only | — | WebCodecs (real decode-and-present probe, never a brand check); Chrome and Safari are the qualification bar, other browsers best-effort |

FFmpeg integration is a deliberately narrow boundary (plan §9): a curated, allowlisted `libavcodec`/`libavutil` build per target, one pinned binding family, opaque GPU surfaces, observable copies, and no FFmpeg types crossing into session or wire layers. macOS/iOS use system VideoToolbox and Android uses MediaCodec, so bundled FFmpeg targets stay few. There is no hot-path `ffmpeg` subprocess. The pure-Rust HEVC candidate (`oxideav-h265`) is tracked as a possible future fallback/verification tool; **no HEVC encoder is written from scratch for this project.**

## Product boundary

**Version 1 includes:** Linux/macOS/Windows hosting; desktop native clients; iOS and Android clients; a browser client; keyboard and pointer control; a practical touch interface; host playback audio; explicit text clipboard exchange; display selection; reconnection; useful diagnostics. One host exposes one existing interactive user session; one remote controller owns input at a time, with a small explicit viewer limit (initially one controller plus two read-only viewers, subject to resource admission).

**Non-goals (plan §2.3):** file synchronization, remote filesystem mounting, USB passthrough, printer redirection, webcam/microphone forwarding, remote shell execution, session recording, an enterprise dashboard, mobile hosting, public-internet guest links, a proprietary cloud account, or a replacement for Tailscale. No AI model in the critical path; no synthesized application responses or fabricated state to hide latency (local cursor rendering is fine; fake application state is not).

**Honest limits stated up front:** sharing even one display can control the wider logged-in session through application actions — setup says so explicitly. Lock, logout, or user switching ends observation and control. A daemon does not bypass Wayland compositor consent, macOS TCC/login windows, or Windows secure desktops.

## Performance objectives

**Every number here is a proposed target from plan §21, not a measurement.** Measurement scope rules (process-family CPU/RSS, GPU allocations counted separately, cold vs. warm startup, direct vs. relay, optical input-to-photon lanes, Sunshine/Moonlight comparison baselines) are part of the plan and bind any future claim.

| Scenario | Proposed target |
|---|---|
| Idle host, no viewers | <25 MiB RSS and <0.1% of one core for the idle broker; no active capture or encoder |
| Healthy direct path (2–5 ms RTT) | 1080p60 and 4K60 on qualified hardware; input-to-photon p50 ≤45 ms, p95 ≤70 ms |
| Typical WAN (~40 ms RTT) | 1080p60 where capacity permits; p95 ≤130 ms |
| Constrained (~120 ms RTT, 5 Mbps, 1% loss) | Readable document interaction at an admitted 720p/1080p viewport; no support claim until real fragmentation/repair shows bounded stalls |
| Warm media path | First useful frame within 500 ms on a healthy direct path; cold worker startup measured separately |
| Static screen | No continuous full-frame encoding to maintain a nominal frame rate; capture freshness stays observable |

## Proposed workspace

From plan §22 — responsibility boundaries, not a requirement to create every crate before the first working slice. Crates enter the workspace only with a real vertical slice.

```text
frankenremote/
  crates/
    fr-core/          typed IDs, limits, session authority, common state
    fr-wire/          bounded codecs and golden protocol fixtures
    fr-transport/     Asupersync-native and browser transport adapters
    fr-tailnet/       local identity, discovery, certificates
    fr-media/         safe media contracts, scheduling, adaptation
    fr-platform/      per-OS capture, input, audio and lifecycle modules
    fr-ffi/           narrowly gated native media/OS boundary modules
    fr-client/        common viewer, input and presentation policy
    frd/              daemon and internal process-role entrypoints
    fr/               CLI and thin desktop launch integration
    fr-web/           WASM entrypoint and browser boundary
    fr-lab/           fixtures, deterministic scenarios, benchmark harness
  mobile/             thin signed iOS/Android application shells
  web/                small self-hosted JS/CSS/HTML shell
  native/             reproducible media build recipes and manifests
  xtask/              repository-owned verification/release commands
```

Budget: **180k handwritten Rust lines** (tests and project-induced upstream work included) as the target, **240k planned maximum**, hard stop below 250k, plus a separate ≤15k allowance for JS/Swift/Kotlin/build glue. One fixed counting command in the repository; the counting method does not get redefined near the end.

## Implementation sequence

Six phases with explicit exit gates (plan §23). Phase 0 exists so that architectural risks die first, in small reproducible vertical experiments — not after most of the product is written.

| Phase | Content | Exit gate (abridged) |
|---|---|---|
| **0 — Retire architectural risks** | Real capture→HEVC→decode→present spikes; live Asupersync QUIC endpoint qualification; real HTTP/3 WebTransport interop; browser hvcC/access-unit probes; tailnet identity fixtures (sharing/tagging/ingress); OS lifecycle grants; packaging; fragmented-loss recovery | Each experiment has a reproducible command, exact hardware identity, result, and retained failure reason |
| **1 — One complete controlled desktop** | One native host/client pair end to end, with the generation model from the beginning | Survives worker restart, network interruption, focus loss, ticket expiry, and resize without stale authority or a growing queue; instrumented latency result exists; a mock codec does not satisfy this gate |
| **2 — All host platforms + native desktop clients** | Three host adapters, service lifecycle, audio, clipboard, display selection, viewer admission and handoff | Common session/input fault suite passes on each qualified OS with device evidence |
| **3 — Browser and mobile clients** | WASM state machine, decode probes, secure-origin defenses, WSS fallback, touch/lifecycle behavior | Real browsers/phones connect over their own tailnet connectivity; background/resume cannot preserve stale control |
| **4 — Quality, adaptation, recovery tuning** | Deterministic controller, idle behavior, settle-to-sharp, loss recovery, telemetry — improving what already works, not first implementing correctness | Defined network scenarios show stable operating points; controller decisions replay deterministically |
| **5 — Release qualification** | Adversarial parsing, origin attacks, tailnet-sharing tests, thermal/long-duration, installer/update/rollback, hardware comparisons | Signed artifacts reproduce tested behavior; unsupported states fail specifically; no claim outruns retained evidence |

An optional extension lane (native 4:4:4 precision, the HEVC-only chroma-carrier experiment, small-group FEC, authorized FrankenTerm semantics) opens only after the core is useful, each with a bounded budget and an independent off switch.

## Agent ergonomics

FrankenRemote is designed to be operated by coding agents as well as humans (plan §18). The planned command surface is small and JSON-first — `fr hosts --json`, `fr doctor --json`, `fr robot session open/observe/input/close`, `frd status --json` — with every robot response distinguishing success from partial submission, cancellation, refusal, or unknown external effect. Observations carry validity boundaries (geometry generation, source-freshness status, capture/presentation timestamps with uncertainty); actions can require preconditions and are refused rather than clicking an old coordinate system. An optional, explicitly granted adapter can expose FrankenTerm pane semantics through `ft robot` instead of forcing agents to read pixels. **These are planned interfaces, not commands that exist today.**

## Verification discipline

The plan's evidence rules bind this repository from day one (plan §24):

- Source reviewed, builds passed, simulated properties passed, independent wire interoperability passed, and hardware measurements passed are **separate evidence categories**; a plan review establishes none of the latter four.
- A capability row is `passed`, `failed`, `blocked`, or `not tested` — an untested row is never "supported with caveats."
- Deterministic lab tests exercise the production state machines and limits (lease expiry, generation fencing, recovery budgets, receiver credits), not generated traces that never touch production logic.
- Native fault tests, security tests (forged sources, origin attacks, hostile HEVC parameter sets, replayed tickets), and the hardware/network matrix are enumerated in the plan as **requirements to execute during implementation, not tests already run.**

## Repository map

| File | Role |
|---|---|
| [`COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENREMOTE.md`](COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENREMOTE.md) | The constitution. 27 sections: decisions, boundaries, protocol shape, budgets, phases, verification matrix, references |
| [`AGENTS.md`](AGENTS.md) | Normative contract for humans and coding agents working here |
| [`PROTOCOL.md`](PROTOCOL.md) | Reserved for the concise wire specification; currently a status stub pointing at plan §17 |
| [`SECURITY.md`](SECURITY.md) | Reporting policy and highest-priority areas |
| [`LICENSE`](LICENSE) | MIT + OpenAI/Anthropic rider |

## About contributions

Please don't take this the wrong way, but I do not accept outside contributions for any of my projects. I simply don't have the mental bandwidth to review anything, and it's my name on the thing, so I'm responsible for any problems it causes; thus, the risk-reward is highly asymmetric from my perspective. I'd also have to worry about other "stakeholders," which seems unwise for tools I mostly make for myself for free. Feel free to submit issues, and even PRs if you want to illustrate a proposed fix, but know I won't merge them directly. Instead, I'll have Claude or Codex review submissions via `gh` and independently decide whether and how to address them. Bug reports in particular are welcome. Sorry if this offends, but I want to avoid wasted time and hurt feelings. I understand this isn't in sync with the prevailing open-source ethos that seeks community contributions, but it's the only way I can move at this velocity and keep my sanity.

## License

The FrankenRemote source is licensed under the **MIT License with an OpenAI/Anthropic Rider**, Copyright (c) 2026 Jeffrey Emanuel (see [`LICENSE`](LICENSE)). The rider withholds all rights from OpenAI, Anthropic, their affiliates, and anyone acting on their behalf, including any use of the software or derivative works in a machine-learning dataset, training corpus, evaluation harness, or pipeline. In any conflict between the rider and the rest of the license, the rider controls.

Shipped binaries will additionally carry the recorded license/build provenance of their curated native media components (FFmpeg configuration, Opus, platform SDKs); that distribution review is a release gate (plan §9.3), not a solved question.

---

The final synthesis, from the plan:

> **Tailscale decides which machines can reach and identify one another. Asupersync makes their session lifetimes tractable. Existing hardware APIs move and compress pixels. FrankenRemote makes the resulting interaction fresh, readable, safe to stop, and easy to understand.**

That is enough of a project to be valuable, and enough of a constraint to make it deliverable.

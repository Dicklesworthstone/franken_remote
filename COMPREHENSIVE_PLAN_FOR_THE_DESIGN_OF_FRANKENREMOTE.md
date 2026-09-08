# Comprehensive Plan for the Design of FrankenRemote

**Version:** 1.4 — reviewed and corrected architecture and implementation proposal; v1.2 narrowed the default admission scope, v1.3 named the Chrome/Safari browser qualification bar, v1.4 makes the first-class native Swift and Kotlin mobile applications explicit and promotes ATP file synchronization, the shared clipboard, and bidirectional audio into core scope  
**Date:** September 7, 2026, America/New_York  
**Project initiator:** Jeffrey Emanuel  
**Host daemon:** FrankenRemoteDaemon (`frd`)  
**Client and command-line interface:** FrankenRemote (`fr`)  
**Implementation target:** 194,000 handwritten Rust lines; planned ceiling 240,000; hard limit below 250,000  
**Status:** researched design, not an implementation or benchmark report; all 27 sections re-reviewed, with corrections integrated in place

> **The product:** Open a machine on your tailnet and use its existing desktop, with hardware-accelerated HEVC, no separate account or pairing ceremony, and a system that refuses to accumulate invisible latency.
>
> **The engineering thesis:** Tailscale owns connectivity and machine identity. Asupersync owns asynchronous lifetimes and transport integration. Platform APIs and existing codec implementations own capture and hardware acceleration. FrankenRemote owns the small amount of policy that makes those pieces behave like one excellent remote workstation.

---

## Contents

1. [Executive decisions](#1-executive-decisions)
2. [Product boundary and user experience](#2-product-boundary-and-user-experience)
3. [What can genuinely distinguish this project](#3-what-can-genuinely-distinguish-this-project)
4. [Findings from the FrankenSuite source inspection](#4-findings-from-the-frankensuite-source-inspection)
5. [System architecture and process boundaries](#5-system-architecture-and-process-boundaries)
6. [Tailscale identity, admission, discovery, and HTTPS](#6-tailscale-identity-admission-discovery-and-https)
7. [Session ownership, cancellation, and authority](#7-session-ownership-cancellation-and-authority)
8. [One video format, several hardware implementations](#8-one-video-format-several-hardware-implementations)
9. [FFmpeg integration and the pure-Rust alternative](#9-ffmpeg-integration-and-the-pure-rust-alternative)
10. [Platform capture and input adapters](#10-platform-capture-and-input-adapters)
11. [The frame pipeline and freshness contract](#11-the-frame-pipeline-and-freshness-contract)
12. [Transport and media recovery](#12-transport-and-media-recovery)
13. [Adaptive quality and latency control](#13-adaptive-quality-and-latency-control)
14. [Desktop text quality and precision extensions](#14-desktop-text-quality-and-precision-extensions)
15. [Input, clipboard, audio, files, and multiple displays](#15-input-clipboard-audio-files-and-multiple-displays)
16. [Desktop, mobile, and browser clients](#16-desktop-mobile-and-browser-clients)
17. [Protocol shape and interoperability](#17-protocol-shape-and-interoperability)
18. [Agent ergonomics and FrankenTerm integration](#18-agent-ergonomics-and-frankenterm-integration)
19. [Memory safety, security, and resource limits](#19-memory-safety-security-and-resource-limits)
20. [Dependency policy, build system, and distribution](#20-dependency-policy-build-system-and-distribution)
21. [Performance objectives and measurement](#21-performance-objectives-and-measurement)
22. [Workspace and code-size budget](#22-workspace-and-code-size-budget)
23. [Implementation sequence and release gates](#23-implementation-sequence-and-release-gates)
24. [Verification and acceptance matrix](#24-verification-and-acceptance-matrix)
25. [Risks, bounded open decisions, and rejected scope](#25-risks-bounded-open-decisions-and-rejected-scope)
26. [Definition of done](#26-definition-of-done)
27. [Research provenance and references](#27-research-provenance-and-references)

## 1. Executive decisions

FrankenRemote should be built as a **tailnet-native remote workstation**, not as a universal remote-administration suite, a replacement VPN, or a new multimedia framework.

The recommended decisions are:

| Area | Decision |
|---|---|
| Video format | HEVC/H.265 only. Main, 8-bit, 4:2:0 is the baseline; additional HEVC profiles require positive capability negotiation. |
| Encoding | Prefer fixed-function hardware encoders. x265 is a software fallback candidate, not the name of the hardware path. |
| Apple media | ScreenCaptureKit and VideoToolbox through narrow platform adapters. |
| Windows/Linux media | Native capture, GPU-surface interoperability, and a trimmed FFmpeg `libavcodec`/`libavutil` boundary for the initial hardware codec integration. |
| Browser decoding | WebCodecs when the exact HEVC configuration works. WASM runs protocol and session logic, not an assumed universal HEVC software decoder. |
| Audio | Opus only, independently of the one-video-codec rule, in both directions: host playback audio to the client and explicit-enable client microphone to the host. The host-side virtual-microphone endpoint is per-OS qualified work with its own Phase 0 spike. |
| Files and clipboard | Explicit file send/receive plus folder synchronization, reusing asupersync's ATP object-transfer machinery over a separate bounded channel. Automatic bidirectional text clipboard synchronization is a default-on capability of the controlling session. |
| Connectivity and identity | The installed Tailscale client, its authenticated local metadata, existing tailnet grants, and tailnet addresses. No embedded second VPN or identity service. |
| Admission default | Default sharing scope is the host's own tailnet user: verified, reachable nodes belonging to the same tailnet user identity as the host may request desktop control. Admitting all tailnet members and tagged nodes is an explicit local setup choice, not the default. Optional local approval, disabled by default, gates observation as well as control. |
| Runtime | Asupersync only, including cancellation-aware ownership and deterministic tests. No hidden Tokio runtime. |
| Native transport | Target Asupersync native QUIC, after live-wire/security qualification: reliable control/input, datagram media with bounded reference-aware recovery. WSS is an explicit compatibility profile for native clients too. |
| Browser transport | Actual HTTP/3 WebTransport interoperability over Asupersync, qualified early. Separate, bounded secure WebSocket channels are the explicit degraded fallback. |
| Safety | Safe Rust in protocol and authority code; a small audited unsafe/foreign boundary for OS and media APIs, with media work isolated from input authority. |
| Product scope | Selected full displays of an existing interactive desktop. No window-isolation promise, independent remote login, preboot access, USB/printer forwarding, or public-internet brokering. |
| Size discipline | Approximately 194k handwritten Rust lines including tests and project-induced upstream work; 46k contingency; stop below 250k. |

Four assumptions need to be made explicit rather than buried in implementation:

**HEVC is not x265.** x265 is one CPU encoder for HEVC. NVIDIA, AMD, Intel, and Apple hardware use their own encoding paths. FFmpeg can expose several of them; its optimized software routines are not what turns x265 into a hardware encoder. [S8] [S12]

**HEVC-only does not imply every browser.** WebCodecs does not mandate HEVC support. The supported browser set must be determined by an actual decode-and-present probe on the client, not by the presence of a browser brand or a nominal hardware decoder. [S14] [S15]

**High bitrate does not repair chroma subsampling.** The baseline 4:2:0 representation discards spatial chroma information. Near-lossless quantization can improve the picture without recovering that missing information. Native-pixel rendering, careful color handling, and optional precision modes matter for terminal text and diagrams.

**A daemon cannot erase OS consent boundaries.** An always-running broker is compatible with user-session capture helpers and one-time OS permissions. It is not a promise of unattended access to every Wayland compositor, macOS login screen, or Windows secure desktop. [S23] [S25] [S26] [S27]

These constraints still leave a compelling product. They also prevent the project from silently expanding into several much larger projects.

### Review corrections that materially change implementation

Version 1.1 resolves errors and under-specified boundaries, not just wording. The most consequential corrections are: presentation deadlines versus reference-retention deadlines; a non-circular decoder startup handshake; expiring input authority checked immediately before OS submission; optional approval that cannot be bypassed by read-only viewing; explicit shared-pipeline ownership; browser queue/flush semantics; Wayland session and permission ownership; and qualification of the real Asupersync endpoint rather than treating a type alias as a deployable transport.

The product requirements remain HEVC-only video, Tailscale-only connectivity and machine identity, Asupersync, narrow foreign-code boundaries, and the existing code-size ceiling. No new relay, account system, codec project, database, or general automation framework is introduced.

Version 1.2 narrows one default and changes nothing else: the out-of-box admission scope is the host user's own tailnet devices rather than every same-tailnet principal, with tailnet-wide sharing as an explicit local choice (Sections 2.1, 6.1, 19.2). The rationale is that desktop control is the most dangerous capability on a tailnet and FrankenRemote deliberately carries no second credential, so the admission default bears the entire authentication burden; Taildrop, a strictly less dangerous capability, already defaults to same-user scope. The scope check consumes only identity evidence Section 6.2 already requires the adapter to produce and fixture-test, so no new subsystem, pairing step, or account is introduced.

Version 1.3 records one scoping decision by the project initiator: current Chrome and Safari, on OS/hardware combinations where their HEVC path actually passes the Section 16.3 probe, are the browser qualification bar — the browser client is good enough when those work. Other browsers remain probe-determined best effort; their gaps are acceptable and not release-blocking. This changes no architecture: runtime support is still decided per combination by the real decode-and-present probe, never by browser brand, and WSS remains the labeled fallback where a target browser lacks a qualified WebTransport path.

Version 1.4 makes the mobile client shape explicit rather than implied. The iOS application is a first-class native Swift/SwiftUI application and the Android application a first-class native Kotlin/Jetpack Compose application. Both sit over the same shared Rust core crates used by the desktop and browser clients — connection setup, protocol and session state, recovery, input semantics, quality feedback, diagnostics — through one narrow, audited FFI boundary per platform, and both are developed in this monorepo under `mobile/ios` and `mobile/android` with repository-owned build commands; there are no satellite mobile repositories. "Thin shell" continues to mean logic-thin, never UI-poor: platform-conventional interface quality is a product requirement, while session, protocol, media, and input logic stay in Rust. The non-Rust allowance in Section 22.2 rises from 15k to 20k handwritten lines to account for the two native UI layers honestly.

Version 1.4 also promotes three capabilities from excluded or minimal status into the core product, by decision of the project initiator. First, file transfer and folder synchronization are v1 scope, built on asupersync's existing ATP object-transfer machinery over a separate bounded channel rather than a new protocol (Section 15.6); the Section 12.2 exclusion of durable object transfer now applies to the real-time media path, not to this capability. Second, the shared clipboard is table stakes: automatic bidirectional text clipboard synchronization is default-on for the controlling session (Section 15.3). Third, audio is bidirectional: explicit-enable client microphone forwarding into a per-OS qualified host virtual-microphone endpoint joins host playback audio (Section 15.4), with the Windows endpoint acknowledged as driver-class work carrying its own Phase 0 spike. The Section 22.2 budget rises to a 194k-line target inside the unchanged 240k planned maximum and sub-250k hard stop.

## 2. Product boundary and user experience

### 2.1 The ordinary experience

The user installs FrankenRemote on a host already running Tailscale. Setup verifies that the tailnet is connected, obtains the required local OS permissions, installs the background service and interactive-session helper, and checks hardware encode support. The full native-and-browser deployment also provisions HTTPS through Tailscale; this is an explicit setup prerequisite, not another user account.

A client then opens FrankenRemote, selects a discovered machine, and sees its existing desktop. On a phone, it can use a saved host or a host link. In a browser, it opens the host's tailnet HTTPS address. There is no FrankenRemote password, device PIN, pairing database, central coordination service, or separate relay configuration.

The default connection requests control. The host makes the controller visible locally and offers an immediate revoke button. Enabling `approval = "local"` requires local approval before **any new remote observation session** receives pixels, thumbnails, audio, clipboard, or semantic data, and before control is granted. Approval records the verified device, requested role, selected displays, audio scope, and session identity. Read-only access is not an approval bypass. OS-required consent remains separate from this optional product setting.

The interface should initially show only the machine, selected display, connection state, and a small toolbar. Detailed metrics belong behind a connection-quality panel. Common failures receive specific explanations: permission missing, no supported HEVC decoder, host not in an interactive session, tailnet policy blocked, controller busy, or browser transport degraded.

Setup asks the local installer to enable sharing of the selected OS session and to choose a sharing scope. The default scope admits only nodes that verifiably belong to the host's own tailnet user identity; admitting all tailnet members and tagged nodes is an explicit choice made at setup or later through local configuration, and setup explains exactly what the wider scope means. A tagged host machine has no owning tailnet user, so it cannot use the own-user default and requires an explicit scope selection. Installing a client alone never enables hosting. No approval prompt or privacy-sensitive capability probe runs before identity checks; repeated requests are deduplicated and rate-limited. Approval expires or is cancelled when its requesting session ends. Switching from approval-off to approval-on revokes existing grants unless the local user explicitly approves those particular sessions.

### 2.2 Scope of version 1

Version 1 includes Linux/macOS/Windows hosting, desktop native clients, iOS and Android clients, a browser client, keyboard and pointer control, a practical touch interface, audio in both directions (host playback audio to the client, and explicit-enable client microphone to the host where the host endpoint qualifies), a shared clipboard with automatic bidirectional text synchronization, explicit file send/receive with ATP-backed folder synchronization, display selection, reconnection, and useful diagnostics.

A host exposes one existing interactive user session selected by local configuration. Multiple monitors belong to that session. Simultaneous independent user logins are not part of the product. One remote controller owns input at a time. The initial viewer limit should be small and explicit, such as one controller and two read-only viewers, subject to resource admission.

Headless support means capture from an already available virtual or physical display. FrankenRemote does not initially ship a kernel display driver, change firmware settings, defeat disk encryption, or create a display server behind the user's back.

The baseline shares full selected displays. Window-only capture and window-scoped control are deferred: cropping video does not confine keyboard shortcuts, clipboard access, accessibility APIs, or system audio to a window. Even a one-display view can control the wider logged-in session through application actions; setup states this explicitly. Touch initially means mouse/trackpad emulation plus qualified text entry, not universal native multitouch injection.

Lock, logout, or user switching ends observation and control. Cooperative clients clear sensitive display/audio buffers on that transition; a recipient cannot be forced to forget pixels already delivered. Unlocked-session sharing does not promise remote unlock, preboot access, or wake of a sleeping machine. An optional, OS-supported active-session sleep inhibitor may prevent idle sleep while sharing; it never defeats lid-close policy or a deliberate local lock. Discovery distinguishes offline, no interactive session, permission required, and ready where evidence allows.

### 2.3 Non-goals

Do not include remote filesystem mounting, USB passthrough, printer redirection, webcam forwarding, remote shell execution, session recording, an enterprise dashboard, mobile hosting, public-internet guest links, a proprietary cloud account, or a replacement for Tailscale. (File synchronization and microphone forwarding were non-goals before version 1.4; both are now core scope under Sections 15.4 and 15.6.)

Do not add an AI model to the critical path. Do not synthesize plausible remote application responses or hide latency by pretending an action has already succeeded. Local cursor rendering is appropriate; fabricated application state is not.

These exclusions are how a complete cross-platform product remains compatible with the size constraint.

## 3. What can genuinely distinguish this project

Hardware video alone is not a novel advantage. Sunshine already provides a relevant hardware-accelerated streaming baseline; Moonlight is a more useful comparison than an intentionally weak VNC configuration. FrankenRemote should earn its advantage in a narrower, coherent combination. [S30]

### 3.1 Freshness rather than throughput

A remote workstation should optimize the age of the useful state shown to the user, not the number of frames that entered an encoder. An old frame arriving perfectly is often less useful than a new frame arriving approximately.

Every stage therefore has a bounded queue and a defined rule for obsolete work. That rule must respect codec references: raw captures can be replaced freely; encoded reference frames cannot.

When the connection cannot sustain fresh presentation, the client lowers work and quality or reports a stale view. It never disguises a growing multi-second backlog as a healthy stream.

### 3.2 Workstation quality rather than television defaults

Terminal text, thin grid lines, small colored glyphs, and low-motion application windows need different tradeoffs from cinematic video. The default controller preserves useful spatial resolution for document work and spends less on redundant frame rate. When motion stops, it sends a sharper version of the current screen.

Cursor movement should not force a full-screen video encode. Static desktops should not keep an encoder busy merely to maintain an artificial 60-fps number.

### 3.3 Input authority that ends cleanly

Every controlling session owns a finite input lease. Revocation, disconnect, loss of client focus, and process failure have explicit release behavior. Reconnection never blindly replays old clicks, text, or modifier state.

This matters for humans, and it becomes essential when agents operate the system.

### 3.4 Explainable performance

The product should answer “why does this feel slow?” directly: relayed tailnet path, overloaded encoder, GPU-to-CPU copies, decoder backlog, inappropriate display refresh, thermal throttling, or network queue growth.

A small metadata trace can reproduce the session and adaptation decisions in Asupersync's deterministic lab without recording the user's screen by default.

### 3.5 A useful extension of the FrankenSuite

The reusable contributions should be small: a well-tested real-time transport profile, cancellation-aware foreign-worker supervision, bounded latest-value media channels, and stable observation/control receipts. They should improve Asupersync and agent tooling without importing an entire terminal platform or creating a generalized distributed operating system.

The ambition belongs in the interaction between these mechanisms, not in the count of subsystems.

## 4. Findings from the FrankenSuite source inspection

### 4.1 Asupersync is the right foundation, with named qualification boundaries

The inspected snapshot is `bf6b361deb3154c56d1450ea679e6d4a3cbf09b9`, whose package manifest identifies version 0.4.11 and Rust edition 2024. Its public networking module aliases the `quic` feature to the native Tokio-free QUIC implementation. It also exposes UDP, WebSocket, native local IPC, and browser-worker coordination surfaces. [S2] [S3]

This matters because a physical `src/net/quic/` directory is not, by itself, the authoritative active API. Integration should follow the exported native path, not whichever similarly named source file is easiest to find.

The inspected tree contains ATP/H3 and WebTransport adapters, including session state and outbound frame queues. Those components are useful starting points. They are not sufficient evidence that a real browser completes the required HTTP/3/WebTransport handshake and exchanges datagrams with a production listener. That interoperability is an early release-blocking experiment. [S4] [S5]

The runtime's published cancellation guarantees are deliberately scoped. Foreign driver calls and non-cooperative work do not acquire a universal wall-clock cancellation bound just because a function receives `Cx`. FrankenRemote must configure its blocking execution explicitly and supervise potentially stuck media work outside the authority path. [S1]

Use regions, cancellation-aware operations, bounded channels, monotonic time, and deterministic lab facilities. Do not enable unrelated database, messaging, metrics, or application-framework features merely because they exist.

The fresh review followed the alias further. `NativeQuicConnection` explicitly performs no socket I/O. The inspected `endpoint_api.rs` distinguishes deterministic loopback helpers from the live UDP/TLS binding and explicitly declines to claim external QUIC/H3 interoperability, migration, or production deployment readiness. `managed_endpoint.rs` contains routing, timers, and authenticated-accept machinery, but that source is not a substitute for testing the selected application-facing path. These observations refine, rather than invalidate, the reuse choice. [S32] [S33] [S34]

Choose one documented live endpoint composition and qualify its complete packet path, TLS certificate/hostname verification, negotiated ALPN, DATAGRAM limits, retransmission, congestion accounting, pacing, multi-client routing, cancellation, and bounded allocations. Loopback tests that manually advance handshake states are not security evidence. External independent QUIC/H3 peers belong in the test harness, not the shipping dependency graph.

Do not implement a second QUIC stack inside FrankenRemote if the integration gate fails. Keep upstream work bounded and counted; use the declared Asupersync WSS compatibility path while a native path remains unqualified, without advertising low-latency datagram support. Also verify that selected TCP/UDP operations actually suspend through the runtime's OS reactor on each target rather than relying on an async-looking signature.

### 4.2 FrankenTerm: reuse the small useful parts

FrankenTerm's inspected material provides a valuable robot-response convention, explicit error codes, condition-based operations, diagnostic workflows, and carefully qualified installation state. Its architecture is much larger than this project needs. [S6]

Adopt the response shape and the discipline of distinguishing accepted, submitted, and observed effects. Audit individual input/key mapping or platform helper modules before reuse; this plan does not claim they already exist as independent drop-in crates.

Do not depend on the whole terminal core to obtain a JSON envelope or key enum. Integrate terminal semantics through an optional existing `ft robot` boundary instead of bringing its mux, search, databases, and fleet automation into `frd`.

### 4.3 FrankenGit: borrow rigor, not administrative mass

The sample plan is useful for separating proposals, invariants, targets, and evidence. FrankenRemote should retain that distinction, an explicit source audit, a dependency budget, a failure model, and implementation gates. [S7]

It does not need a hierarchy of dozens of constitutions, federated authority stores, content-addressed screen histories, proof bundles for every frame, or a distributed database. This document, a concise wire specification, a capability matrix, and executable tests are enough to begin.

## 5. System architecture and process boundaries

### 5.1 Logical topology

```text
                           Existing Tailscale network
                                     |
                    Native QUIC / WebTransport / WSS
                                     |
                  +------------------v------------------+
                  | frd: identity, admission, sessions  |
                  | small idle footprint; no codec FFI |
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

The diagram shows ownership and supervision, not a requirement to route every compressed byte through the input agent. The media worker sends bounded encoded output directly to the broker; the session agent retains consent and input authority. The client has the corresponding receive/decode/presentation path, with local input and lifecycle handling independent of rendering.

The user still installs one product. Separate process roles are an internal safety and OS-session requirement, not separate services to configure manually. They may be modes of a signed process family rather than many unrelated binaries.

### 5.2 The idle daemon

`frd` owns its tailnet listeners, the small configuration, current peer-identity cache, session registry, browser assets, and local control socket. It does not capture, encode, poll the full screen, or hold large GPU surfaces while nobody is connected.

Service startup precedes user login when the OS supports that arrangement. The daemon can report that no shareable desktop exists without pretending it can capture a login screen.

Elevated startup or installation privileges must not turn the ordinary network broker into an all-purpose root command runner. Use the smallest platform-specific broker necessary for service registration, user-session launch, and protected Tailscale metadata/certificate operations. Drop privileges where practical and expose no arbitrary privileged RPC.

### 5.3 The interactive-session agent

The agent runs in the locally selected user's interactive context. It owns local approval, the visible controller indicator, the input lease, key/button tracking, and platform permission state. Its control path must remain responsive if the encoder crashes or stalls.

On a local session change, logout, lock transition, or loss of permission, the old authority is revoked. A new interactive session never inherits the old controller automatically.

Local IPC uses parent-created/inherited channels or protected endpoints, OS peer credentials where available, role-specific capabilities, and process/session generations. Credentials alone authenticate a user, not a particular trusted helper. Prevent other users and unauthorized process roles from approving sessions or substituting workers; do not claim isolation from an arbitrary malicious process running with all the selected desktop user's privileges. That user is a trust boundary unless an additional OS sandbox is actually enforced.

The session agent owns permission-bearing sessions even when capture execution is delegated. In the Wayland path it retains the portal/D-Bus session and input connection, passing only the selected PipeWire remote/stream capability to the media worker. A worker must not create a combined capture/input session and accidentally inherit input authority. Permission attribution and whether handles can be delegated are per-platform Phase 0 tests, not assumed universal IPC features. [S27] [S43]

On platforms where a necessary OS API can block, keep the lease timer and revoke decision on an independently progressing authority path; do not hold an authority lock while calling it. If the input process itself fails, report uncertain release state and use an OS-qualified watchdog/restart path. Process separation does not guarantee that an arbitrary failed OS injection can be undone.

### 5.4 The media worker

Capture and encode stay together wherever possible. This preserves GPU-surface ownership and avoids making cross-process texture export a prerequisite for the first implementation. Only encoded bytes and bounded metadata need cross the broker boundary.

A stuck foreign call can require killing the worker. Before attempting a graceful drain, the authority owner fences its generation and revokes input. Late callbacks or packets from that generation are ignored.

Do not promise that an application can safely interrupt an arbitrary GPU driver call. The controllable guarantee is that a stuck codec does not retain authority, accumulate unbounded work, or prevent a new session from being admitted after cleanup is possible.

Treat capture buffers as borrowed until the platform's documented release point. A GPU-local copy into an owned surface is preferable to holding a compositor buffer indefinitely or reusing a texture still being read. On Linux, DMA-BUF ownership, modifiers, plane layout, fences, and cache synchronization require explicit handling; sharing a file descriptor is not synchronization. On every backend, clear padding/cropped borders in newly allocated surfaces so old GPU contents cannot become encoded pixels. [S49]

A separate process gives crash and hang isolation by itself, not a security sandbox. Document the actual restrictions on filesystem, IPC, network, process creation, and input APIs that the OS permits while retaining GPU/capture access. Workers receive no Tailscale control socket, certificate key, approval endpoint, or input lease capability. A compromised same-user unsandboxed worker is not claimed to be harmless. Decode on native clients deserves the same audit: host-supplied HEVC is untrusted input to foreign code, even on a trusted tailnet.

Restarts have a bounded rate and backoff. Repeated GPU hangs end the affected media profile rather than spawning unlimited workers; killing a process does not establish that GPU driver resources were reclaimed.

## 6. Tailscale identity, admission, discovery, and HTTPS

### 6.1 Admission policy

The intended default is:

```text
verified current same-tailnet node
AND within the locally selected sharing scope
    (default: nodes of the host's own tailnet user;
     explicit local option: all tailnet members and tagged nodes)
AND allowed to reach this host by Tailscale policy
AND locally enabled desktop sharing
AND requested capability supported by this OS session
AND (a controller slot is available, when requesting control)
AND local approval, only when configured
```

Different tailnet users and tagged machines are valid principals, but only when the host has locally selected the tailnet-wide scope or an explicit Tailscale app-capability grant admits them; the out-of-box scope is the host user's own devices. The reasoning is deliberate: desktop control is the most dangerous capability a tailnet service can grant, and FrankenRemote intentionally has no second credential, so the admission policy is the entire authentication story. Scope membership is decided from the same authenticated local metadata as admission itself (Section 6.2) — the peer node's user identity compared against the host node's — never from names, DNS, or reachability, and missing or ambiguous user evidence is a typed refusal exactly as in Section 6.2. The policy intentionally trusts devices inside the selected scope with desktop control; a compromised in-scope device has that authority until revoked, which is why the widest scope must be chosen rather than assumed.

The daemon binds only to its current Tailscale IPv4/IPv6 node addresses and enforces a platform-qualified tailnet ingress boundary using interface/socket/firewall facilities where available. Binding a destination address alone is not universally proof of ingress through the tunnel. It must not listen on all interfaces and trust a `100.64.0.0/10` source. Loss of the tailnet, self-node identity change, or tailnet change closes affected listeners and invalidates sessions.

### 6.2 Establish identity through the local Tailscale authority

Use a narrow Rust client for the installed Tailscale LocalAPI or its documented platform equivalent. `WhoIs` returns a node, user profile, and capability map; local status exposes node identities, node addresses, the current tailnet, and sharing-related metadata. Do not shell out once per packet. [S17] [S18] [S31]

For each connection, resolve the actual transport peer address through this API and bind it to the node's stable identity. Require the source address to be one of that node's own addresses, not merely an address behind a subnet route. Verify it against current local peer information and the active local tailnet.

The supported-version adapter must distinguish ordinary members from shared-in nodes and external sharees. Relevant current metadata includes `ShareeNode`, `AltSharerUserID`/`Sharer`, and canonical node names under the current tailnet namespace. Those names are evidence only when read from authenticated local Tailscale metadata, not from reverse DNS or a client-supplied hostname. `InNetworkMap` alone is not proof of same-tailnet membership. [S17] [S31]

Pin this interpretation to tested Tailscale versions and fixtures: two local members with different owners, a tagged member, a shared-in host, an external sharee, and a user present in more than one tailnet. Missing or ambiguous membership evidence is a typed refusal, not permission to weaken the policy. A Tailscale app-capability grant is an available stricter deployment profile, not a replacement password system. [S22]

Disable replayable early-data input. Revalidate identity on a transport path change; QUIC connection continuity must not silently transfer control to a different node. A new app session always obtains a new input lease.

Use an internally consistent status/WhoIs snapshot, retrying on local identity changes. Absence from a partial peer list is not proof of nonmembership, but insufficient evidence still cannot grant control. Check node expiry, local backend state, and policy-derived capabilities when supplied by the qualified adapter. DNS suffixes, zero-valued sharer fields, and human email-domain equality are not independent authorization proofs. `WhoIs` identifies a source; its existence does not certify that a chosen collection of metadata fields is a permanent public same-tailnet-membership API. [S17] [S18] [S31]

The local-metadata profile remains the zero-policy-edit default only on versions where these semantics pass real sharing/tagging fixtures. Where authoritative membership cannot be obtained, report `tailnet_membership_unverifiable`; the supported alternative is an explicit Tailscale app-capability grant scoped by that tailnet's member/tag policy, not an ad hoc local allowlist or hostname guess. Such a deployment is labeled policy-configured, not zero-setup. Never silently write or broaden a tailnet's grants. [S22] [S50]

Tailscale LocalAPI access is platform- and installation-variant-dependent. Qualify Linux sockets, Windows local transport/permissions, and macOS Standalone/App Store/CLI variants separately. Use a bounded, correctly selected installed CLI invocation at setup or connection admission when it is the supported metadata/certificate bridge; never invoke a shell with peer-supplied arguments, parse human-oriented text as authority, or launch a process per packet. Missing API capability is an actionable host qualification failure, not a reason to install another VPN. [S40] [S41]

No service proxy, userspace-networking gateway, or subnet router is accepted as an equivalent direct peer when it destroys the required source identity. A genuine Tailscale DERP relay is different: it carries the node-to-node overlay, so the host still sees the inner node identity. Native client discovery and host TLS validation are bound to the selected canonical FQDN and node addresses, never arbitrary redirects supplied by another host.

### 6.3 Revocation and stale metadata

Maintain separate read/observation authority and control authority. All channels must belong to a currently admitted session; losing identity or local authorization ends media and auxiliary access as well as input. Revocation never waits for media-worker draining.

Use a one-second host challenge cadence and provisional three-second renewable authorization deadlines for observation and control, with deadlines anchored to the host's monotonic authority clock. A read-only viewer renews observation authority without ever acquiring an input lease. Control additionally requires current view readiness and its own lease. A response can renew only an outstanding, unexpired challenge for the current lease. It does not extend authority for another three seconds merely because a previously buffered heartbeat finally arrived. Expired leases are terminal; delayed heartbeats cannot resurrect them. Reacquisition is a new grant, and local-approval deployments follow their approval policy again.

The input agent checks expiry independently and immediately before every OS submission. Client focus/lifecycle release is best effort; host expiry does not depend on the client successfully delivering that release. Input freshness tickets in Section 15 impose a shorter bound on individual actions than the connection-liveness lease.

Subscribe to local Tailscale changes where available. If local identity authority is unavailable, admit nothing new and stop renewing affected observation/control authorization; terminate at the already recorded deadlines. A short lease is not a claim that a remote administrator's change reaches an offline host within three seconds. Locally cached Tailscale state can remain usable without live control-plane contact; expose that distinction. Known key expiry, identity changes, local revoke, and explicit policy removal take effect locally without waiting for the lease timer.

Suspend/resume is a generation boundary for authority. Use an OS-qualified clock or resume detection; a clock that pauses during suspend must not allow pre-suspend leases or input tickets to survive wake. Apply the same rule after an authority-thread scheduling stall: check the deadline before processing its queued work.

### 6.4 Discovery

Native desktop clients can obtain peers from local Tailscale state, then perform bounded capability probes only against those node addresses. Cache results by stable node identity, invalidate on tailnet changes, and avoid broad subnet scans. Tailscale discovery identifies machines; a small FrankenRemote probe determines whether `frd` is installed and usable.

Mobile apps and ordinary web pages may not have access to another application's local API. Their baseline is saved hosts, explicit host links, and an optional authenticated directory served by a known `frd`. A directory response is discovery information, not delegated permission to control other hosts. It must not proxy connections around the viewer's own Tailscale policy or expose inventory contrary to an explicitly restricted directory policy.

Every client device, including a browser's computer or phone, must itself have working tailnet connectivity. A browser is not a way to admit arbitrary outside users through an authorized gateway.

Serve no screenshots, application titles, clipboard previews, or audio in discovery responses. A remotely served directory is opt-in and limited by local policy; a host's ability to see a peer does not prove that the requesting viewer can reach it. Browser/mobile clients need no broad tailnet administrator API token. Use bounded concurrent probes with timeouts/backoff, cache capabilities with an expiry, and revalidate live encode/decode admission after OS/driver changes.

### 6.5 HTTPS without a second identity system

Use Tailscale-provisioned certificates for the host's approved tailnet FQDN. The main deployment terminates HTTPS and QUIC directly in `frd`, preserving the transport source identity and enabling the actual WebTransport endpoint. Existing Tailscale encryption does not remove a browser's secure-context and certificate requirements. [S19]

Setup checks that tailnet HTTPS is enabled and explains certificate-transparency visibility of the machine name. Certificate renewal and atomic replacement are part of the product. A one-time certificate export is not a renewal strategy. Native clients verify the server certificate and the selected Tailscale destination; do not introduce an undocumented accept-any-certificate path. [S19]

Use one configurable unprivileged service port for TCP and UDP where possible; 8443 is a provisional default, not a claimed standard allocation. Detect collisions and print the real endpoint. Do not silently overwrite another application's listener or Tailscale Serve configuration.

Tailscale Serve is an optional compatibility ingress, not the architectural foundation. Its human-user identity headers do not cover tagged-device identity in the same way, and a reachable shared user is not automatically a same-tailnet member. A Serve adapter must protect its backend and establish equivalent identity semantics. Never accept identity headers from an ordinary direct connection. Tailscale Funnel, which exposes a service publicly, is outside the product. [S20]

Specify TLS 1.3 for QUIC, a verified server certificate/hostname, and application-protocol separation: a project-specific native ALPN and `h3` for WebTransport. Serving HTTPS on TCP does not implement HTTP/3 on UDP. One port is an optimization only if the selected QUIC endpoint safely routes both ALPNs; otherwise use two explicit unprivileged ports rather than ad hoc byte sniffing. No application operation, including opening a view or clipboard access, is admitted in 0-RTT. [S35]

The certificate helper has access only to approved host names and protected key storage, not a generic Tailscale administration proxy. Install the certificate chain and matching key atomically; retry renewal with backoff, alert ahead of expiry, and continue using a still-valid certificate during a temporary renewal outage. Expiry never enables skip-verification. A device rename or tailnet switch revalidates endpoints and certificate names before accepting new sessions. No long-lived bearer tokens are placed in host links.

Tailnet DNS/HTTPS enablement and certificate-transparency disclosure remain explicit prerequisites for the default full product. Deployments that cannot meet them are not solved by silently adding TOFU, pin copying, or an unrelated certificate authority. Certificate/private-key rotation, browser asset versions, and the native TLS trust store are included in release tests.

## 7. Session ownership, cancellation, and authority

### 7.1 State machine

```text
Identified -> CapabilitiesChecked -> WaitingApproval? -> ViewOpening -> Viewing
                                                      |                 |
                                                      v                 v
                                                   Refused       ControlGranted
                                                                        |
                                                               Suspended/Revoke
                                                                        |
                                                        Draining -> Closed
```

Observation permission, media readiness, and input authority are separate state variables. Receiving a first frame does not grant control; a controller slot is committed only after authorization, readiness, and cleanup of any previous controller. Pending approvals/reservations have short bounded lifetimes, preventing a silent client from monopolizing the slot. Handoff is serialized in the input authority owner, not by a check-then-set race in two broker tasks.

The **OS share session** owns an Asupersync region for its portal/capture resources and shared media pipelines. Each **remote viewer connection** has a child region for transport, feedback, and subscriptions. The controller has an independently revocable input-lease child. An encoder shared by three viewers belongs to the share session, not whichever viewer joined first. Last-subscriber removal stops that pipeline; closing one viewer never cancels another's shared encoder. A failed shared pipeline suspends affected control/readiness while healthy unrelated display pipelines remain bounded and independent.

Keep authority state small and ephemeral: current OS session, admitted viewers, controller, capabilities, and generations. Configuration is a local file. No database or durable event-sourcing engine is needed.

### 7.2 Generations that fence stale work

Use distinct typed identifiers for host boot, OS session, remote session, input lease, display geometry, and media configuration. A display resize is not the same event as a controller takeover or a decoder reconfiguration.

Every input batch binds to the current session and lease. Coordinate-dependent input also binds to the display-geometry generation. Every video access unit binds to its stream and media-configuration generation. The client rejects stale geometry and configuration instead of interpreting old bytes under new dimensions.

These identifiers need not be repeated at full width in every datagram. An authenticated connection can negotiate compact stream bindings. They must remain unambiguous across reconnects, channel replacement, and worker restarts.

Also distinguish codec-configuration generation from decoder/reference-chain recovery generation and from a viewer's viewport/mapping generation. An IDR after loss can restart the reference chain without changing the SPS or display geometry. A server-side crop/pan can change input mapping without changing encoded width/height. Each has an explicit binding; old datagrams, repair requests, callbacks, or WSS channels cannot become valid merely because a numeric stream ID was reused.

A host-boot/session identifier is unpredictable, not an authority credential in itself. Request sequence numbers are monotonic within a lease; wraparound closes or replaces the epoch rather than reusing identities. All auxiliary channels attach with a one-use, short-lived, role-specific capability obtained through the admitted control channel and tied to the verified peer/session. A session ID alone is not enough to attach.

### 7.3 Cancellation ordering

The teardown order is: revoke input authority; issue releases for remotely held keys and buttons; invalidate worker/configuration generations; stop accepting capture work; cancel cooperative tasks; drain bounded sends and callbacks; terminate a stuck foreign worker if necessary; publish closure.

A scope ending is not permission to discard committed external effects silently. Keyboard text already submitted to the OS cannot be rolled back. Releasing modifiers is cleanup, not transaction rollback.

An acknowledgement has a named stage: admitted by FrankenRemote, submitted to the OS API, or observed through an instrumented application. Never label these different stages “exactly once execution.” Deduplicate input within a live epoch, and never replay uncertain actions across a reconnect.

The input agent tracks only remote injections and handles collisions with local input conservatively. Commodity OS input APIs do not provide a perfect ownership model for a key simultaneously held physically and synthetically. Local activity can suspend the remote lease; this behavior needs platform tests rather than a claim of universal isolation.

Fence first, then clean up: clear pending input and mark the lease unusable before submitting synthetic releases, so the cleanup itself cannot be followed by another old key-down. If an OS batch reports partial submission, account for the accepted prefix/count and retain an explicit unknown result when that cannot be known. Cancel queued text in bounded units, not halfway through a UTF-16 surrogate pair or protocol event; already submitted text remains irreversible. Release-only cleanup does not reauthorize a blocked press.

A media reconnect can preserve a viewing session but never silently restore an input lease that was revoked for stale view, focus loss, or failure. Healthy viewers cannot override a local revoke. Local approval decisions, scope changes, handoffs, and closure all execute through the same serialized authority owner.

## 8. One video format, several hardware implementations

### 8.1 Baseline HEVC contract

The interoperability baseline is HEVC Main, 8-bit, 4:2:0, SDR, with independently signaled color primaries, transfer function, matrix, and range. Width, height, bit depth, profile, level, display crop, and frame-rate limits are negotiated from actual capabilities.

Main10, HDR, 4:4:4 range extensions, and higher refresh rates are optional extensions. They never activate merely because a vendor name is present. An encoder supporting Main10 does not prove that the connected browser can decode or correctly present it.

The initial encoder profile uses no frame reordering, no future-frame lookahead, and a simple low-delay I/P reference chain. Begin with one previous reference where the backend allows it. Later features require evidence of a net benefit within the latency budget.

Begin with low-delay constrained VBR, or a qualified low-latency CBR mode without unnecessary filler/padding. Test a small VBV reservoir near one or two frame intervals where the backend supports it. A bitrate target is a ceiling/budget, not a requirement to transmit filler while the desktop is idle. Tiny VBV settings can damage a large IDR or force severe quantization; qualify recovery-frame size and quality separately. NVIDIA documents relevant low-latency tuning, but its settings are not universal values for every encoder. [S8]

Require a verified independently decodable IDR on startup, configuration replacement, and reference-chain recovery. An ordinary intra picture, a generic packet keyframe flag, or a CRA with unqualified leading-picture behavior is not the baseline reset contract. Observe the emitted bitstream, including effective reorder/reference requirements. A provisional maximum GOP of two seconds applies to active encoding, not a requirement to wake a static screen every two seconds. Cyclic intra refresh is an optional extension only after its convergence and loss behavior are specified; it does not automatically replace a clean IDR. Recovery policy and configuration handshakes are defined in Section 12.

Keep the initial bitstream subset deliberately simple: one layer, one temporal layer, progressive pictures, no B-frame reordering, bounded decoded-picture buffering, and a qualified I/P dependency chain. Native APIs that cannot produce that subset are not silently accepted under a generic “HEVC supported” result. More reference pictures, slices, or encoder tools can be added only with explicit dependency/decoder tests.

Advertise coded dimensions separately from visible crop and desktop geometry. Round to the backend's alignment requirements and initialize padding; never expose uninitialized surface edges. Default to SDR with a specified color conversion. An HDR desktop must take a qualified tone-mapping path into SDR, or report that it cannot currently be represented correctly. Do not label clipped or incorrectly transformed HDR pixels “SDR support.”

### 8.2 Capability probing, not optimistic configuration

Probe supported codec profiles, maximum geometry, accepted GPU surface formats, asynchronous operation, rate-control settings, keyframe requests, simultaneous session limits, and zero-copy interoperability. Then encode and decode a short representative workload.

Read back the effective configuration where possible. An API accepting a request does not establish that the hardware honored it. Measure time-to-first-packet, steady-state encode delay, reference behavior, and reconfiguration latency.

Backend selection should minimize end-to-end cost: capture conversion, copies, encode delay, output quality, decoder compatibility, and thermal behavior. The “fastest” preset can be worse overall if it creates much larger frames and network delay.

Probe capture, conversion, encode, decode, and presentation on the actual GPU/device combination. Hybrid-GPU laptops can capture on one adapter and encode on another; a decoder's profile limit does not establish that its output surfaces interoperate with the chosen window compositor. Test an actual desktop-sized moving/text sequence, not only a tiny synthetic frame. Cache results with OS/driver/device identity and invalidate after device loss; admit each live encoder session against current capacity. Hardware-required means fail or explicitly select the software profile, not quietly fall back inside a library.

### 8.3 Preferred implementation map

| Platform | Host capture | Preferred HEVC path | Client presentation |
|---|---|---|---|
| macOS | ScreenCaptureKit | VideoToolbox | VideoToolbox output to a native GPU presentation surface |
| Windows | Desktop Duplication initially; Windows Graphics Capture when justified | FFmpeg hardware bridge to NVENC, AMF, or QSV as available | Qualified hardware decode and D3D presentation |
| Linux | PipeWire/portal on Wayland; explicit X11 adapter | FFmpeg hardware bridge to VAAPI or NVENC, with device-specific qualification | Qualified GPU decode and native presentation |
| iOS | Client only | No encoder required | VideoToolbox and native display layers/Metal as appropriate |
| Android | Client only | No encoder required | MediaCodec output directly to Surface |
| Browser | Client only | No encoder required | WebCodecs output to a browser presentation surface |

This table describes target adapters, not a certification of every driver or OS version. [S23] [S25] [S27] [S28]

Apple's specialized low-latency rate-control mode described in its 2021 VideoToolbox session is H.264-specific. Do not assume that setting provides the same HEVC path. HEVC needs its own real-time, hardware-required, no-reordering configuration and measurement. [S24]

## 9. FFmpeg integration and the pure-Rust alternative

### 9.1 Recommendation: own the interface, not the codec

Implement a small FrankenRemote media interface around `Encoder`, `Decoder`, `GpuSurface`, `EncodedAccessUnit`, `CodecConfiguration`, and `MediaCapabilities`. These are bounded, ownership-aware contracts, not a new general multimedia graph framework.

Use `libavcodec` and `libavutil` for the initial Windows/Linux hardware integration. Keep GPU surfaces opaque inside the adapter, and make copies observable. Use software conversion/resampling libraries only where the selected fallback genuinely needs them. Do not make container demuxing, arbitrary filters, network protocols, or device discovery through FFmpeg part of the normal path.

The default binding choice should be a pinned `ffmpeg-sys-next`-family binding under this project-owned interface, qualified against one selected FFmpeg ABI. Evaluate the maintained `ffmpeg-next` and `ffmpeg-the-third` families during the build spike, then choose one. Their existence is useful; neither removes the need to audit GPU ownership and hardware-context handling. [S10] [S11]

Do not maintain two competing wrapper implementations. Do not expose FFmpeg types to the session or wire layers. This preserves the option to replace an adapter without rewriting FrankenRemote.

The wrapper must implement FFmpeg's send/receive state machine, not assume one packet per submitted frame or retry `EAGAIN` by sleeping. Drain available output to make progress, retain reference-counted inputs until ownership permits reuse, and distinguish temporary backpressure, EOF, device loss, and fatal corruption. End-of-stream draining is not a per-frame low-latency operation. Configure/encode/decode calls stay off the authority/event-loop thread. All callback contexts and GPU resources remain alive until completion or contained worker termination. [S45]

Compressed input buffers have the alignment/padding required by the selected native API, with checked lengths and initialized padding. Use the library's packet/buffer allocation facilities rather than handing a decoder an arbitrary unpadded network slice. [S48] Preserve timestamps, duration, output status, color information, and configuration changes through the wrapper. A small safe interface must not hide these correctness requirements.

### 9.2 Avoid the build nightmare by reducing the build

Build a curated FFmpeg distribution with an explicit allowlist of the required HEVC codec components and hardware integrations. Disable unused codecs, muxers, demuxers, protocols, filters, executables, and optional integrations. The exact configuration belongs in source control and in the release manifest.

Produce native media artifacts per target on matching builders, with compiler versions, source hashes, configuration arguments, checksums, and license notices. Consume those exact artifacts during release packaging. Do not ask every end user to compile FFmpeg or depend on whichever unrelated FFmpeg build happens to be installed.

Dynamic linking is the preferred desktop packaging route where practical. macOS/iOS should use the system VideoToolbox path, and Android should use MediaCodec, reducing the number of targets that need bundled FFmpeg. Browser builds must not pull in native codec libraries.

There is no hot-path `ffmpeg` subprocess accepting raw frames on standard input. Media-worker process isolation is different: that worker calls a narrow library API and retains GPU surfaces locally.

Keep a per-target component manifest: enabling a hardware encoder alone does not establish availability of the HEVC decoder, parser, hardware-context support, or bitstream normalization needed by a client build. Include only the components actually exercised by the selected pipeline and test the stripped build itself. Link/load native libraries from protected package paths, not the current directory or user-controlled search paths. The default Linux package targets a declared libc/driver environment; reproducibility does not make one binary compatible with every distribution.

### 9.3 Software fallback and licensing

The default hardware-focused bundle should avoid `--enable-gpl` and `--enable-nonfree`. FFmpeg's license depends on its build configuration; x265 has GPL and commercial licensing options, with separate patent considerations. The existing FrankenSuite license riders must also be evaluated as actual license terms, not assumed equivalent to plain MIT. [S3] [S9] [S12]

An optional software encoder is useful for rescue access or unsupported hardware, but should be opt-in and visibly CPU-limited. It should negotiate lower geometry/frame rate instead of silently consuming all cores trying to deliver 4K60.

Do not assume that placing x265 in a plugin or another process automatically settles redistribution obligations. The exact shipped combination needs a recorded distribution decision. Neither hardware acceleration nor a clean-room implementation automatically resolves HEVC patent questions.

Dynamic linking is not automatic license compliance. Record the actual FFmpeg configuration, corresponding source/patch availability, notices, applicable relinking/reverse-engineering terms, and SDK redistribution constraints for the shipped combination. A license-compatible optional encoder must also be compatible with the project's own dependency license terms. Hardware availability, copyright licensing, and patent exposure are separate questions. This is a release gate, not a claim that this design has resolved the legal status of a future binary. [S9]

### 9.4 A real pure-Rust candidate exists

`oxideav-h265` is more than a header parser according to the README rechecked for version 1.1: it describes both decoding and an I/P/B encoder with rate control. It is a legitimate candidate to evaluate. Its documented encoder subset is 8-bit 4:2:0 with dimensions divisible by 16; its published examples do not establish an x265-class, low-latency 4K60 replacement. [S13]

Treat it as a possible future software fallback or verification tool. Before adoption, test independent decoder interoperability, supported bitstreams, adversarial input behavior, browser-target builds, throughput, memory, and quality at matched bitrate. Do not make the project contingent on improving its codec algorithms.

A Rust crate that parses HEVC configuration or SPS headers is not a complete pixel decoder, and a decoder is not an encoder. Candidate selection must distinguish these categories explicitly.

**Decision:** do not write an HEVC encoder from scratch for FrankenRemote. Preserve a replaceable media interface and revisit software-only Rust when measurements justify it.

Treat candidate documentation as claims to verify, not evidence of codec conformance, fuzzing maturity, or comparable speed. Pin a specific candidate revision before any experiment. Do not let adopting a software codec smuggle a second runtime, a large general multimedia stack, or an uncapped optimization project into the dependency graph.

## 10. Platform capture and input adapters

### 10.1 Linux

The preferred Wayland path is the compositor-supported RemoteDesktop/ScreenCast portal and PipeWire. Capture, input permission, and restoration tokens are backend-specific capabilities. A stored restoration token does not establish universal, permanent, unattended access; the compositor can refuse or revoke it. [S27]

Test GNOME and KDE independently. Use portal-supported input mechanisms, preferring the supported EIS path when available. Any libei or PipeWire foreign boundary belongs in the platform exception list. Do not add a secret fallback that injects through privileged devices when the portal refused permission.

The explicit X11 adapter can use established capture/damage and input mechanisms with different security assumptions. Expose the difference in diagnostics. Do not spend the first release supporting every historical X server and window manager.

A login-manager or headless-session adapter is separate future work unless it can be delivered using an existing supported capture session without new privileged infrastructure.

Implement the permission/session sequence, not just a PipeWire reader: create the portal session, select input devices and display sources, request optional clipboard integration **before** `Start`, complete the portal response, then obtain the restricted PipeWire remote and qualified input connection. The session agent keeps the portal session alive. Persist an offered replacement restore token atomically; restore tokens are single-use and must be rotated rather than replayed forever. Portal/interface versions and returned grants, not requested flags, determine what was actually authorized. A combined RemoteDesktop session must follow that interface's persistence semantics. [S27] [S42] [S43]

Resolve the selected stream by the strongest identifier the portal provides. Recent ScreenCast versions expose a PipeWire serial because numeric node IDs can be reused. Check stream identity and mapping again after reconnect. Keep compositor-space coordinates, stream pixel dimensions, crop/scale, and EIS region mapping distinct; portal coordinates are not automatically captured pixels. Cursor metadata is used only when that capture mode was granted. Treat the remote FD as a scoped capability, not permission to inspect the user's entire PipeWire graph. [S43]

The minimum qualification table includes GNOME, KDE, and Hyprland/wlroots-family environments as separate rows, with exact versions and independent results for capture, pointer, keyboard, restore, clipboard, and playback audio. Do not infer RemoteDesktop/EIS support from a working ScreenCast portal. An unsupported row is an actionable refusal or an explicitly view-only capability, never an undocumented privileged input fallback. A systemd user service must attach to the correct graphical session and its D-Bus environment rather than assume system-service environment variables identify the desktop.

### 10.2 macOS

Use ScreenCaptureKit for selected full-display capture and supported audio capture, preserving native buffer ownership into VideoToolbox where possible. Window-scoped sharing is deferred. Install a launchd-managed process family with a user-session component. Screen Recording and Accessibility permissions are setup states that can be denied or revoked. [S23]

Keep signed bundle identities, entitlements, and permission attribution stable across updates. A media worker must be packaged so its capture permission is understandable and maintainable, not accidentally attributed to a temporary build path.

Support existing unlocked desktops first. Do not imply that running a root LaunchDaemon bypasses TCC, FileVault, or the login window. Explicitly handle sleep/wake, fast user switching, display removal, and display-color changes.

Keep event-loop/thread affinity explicit for capture callbacks, VideoToolbox completions, and AppKit UI. Prompt for permissions only through a signed user-visible setup path; a headless service cannot complete a consent interaction on the user's behalf. Validate permission persistence after updates with the actual helper layout. Protected/uncapturable content is reported as such, never treated as a stalled network. Audio permission and scope are distinct from Screen Recording and Accessibility state; do not assume screen capture consent authorizes every audio source.

### 10.3 Windows

Use Desktop Duplication initially for full-display capture, including its cursor and dirty-region information, with a D3D11 texture path into the encoder. Recreate capture on the documented desktop/display transitions. Add Windows Graphics Capture only for a concrete required capability, such as a better supported window-sharing path. [S25]

The service runs separately from the interactive user agent. Do not capture the desktop from Session 0 and assume it represents the logged-in user's screen. Treat lock screens, user changes, and secure-desktop transitions as explicit capability changes.

`SendInput` is constrained by integrity levels. The initial product must not promise control of every elevated window or secure desktop. Any future elevated-input mechanism requires a separate narrow design, packaging requirements, and tests; it is not an undocumented side effect of installing a service. [S26]

Qualify the capture device against the actual display adapter, and handle access-lost, device-removed, resize, rotation, and duplication-session exhaustion explicitly. Release duplication frames promptly; copy to an owned GPU surface when retaining a borrowed texture would block capture. Desktop Duplication's original format behavior and `DuplicateOutput1`'s selectable formats differ, which matters for HDR and color handling. Choose a qualified SDR conversion/tone-mapping path without silently changing the user's display settings. [S25] [S44]

Report protected-content blackouts and integrity-restricted input separately from transport failure. Test mixed-DPI monitors with negative virtual-desktop coordinates and rotation. The input agent's executable, launch permissions, and named-pipe endpoints must remain bound to the intended interactive session. Secure attention sequences, lock-screen access, and elevated-window control are not implied by a successful ordinary `SendInput` test.

## 11. The frame pipeline and freshness contract

### 11.1 Keep pixels on the GPU

The desired host path is capture surface -> GPU color conversion, only if needed -> hardware encoder -> compressed access unit. The client path is compressed access unit -> hardware decoder -> display surface.

“Zero-copy” is a measured property of a qualified path, not a global marketing claim. Device mismatches, capture formats, texture modifiers, driver APIs, and compositing may require copies. Count them, report them, and choose a slower-named encoder when it avoids a more expensive transfer.

Do not repeatedly convert through CPU BGRA. Preserve explicit surface ownership until the corresponding asynchronous callback or GPU completion makes reuse safe.

### 11.2 Queue policy

| Stage | Initial policy |
|---|---|
| Capture admission | One replaceable latest pending capture |
| Encoder submission | A bounded backend-qualified number of in-flight surfaces, initially one or two where supported |
| Compressed outbound work | Byte limit plus deadline limit; no unbounded FIFO |
| Frame reassembly / dependency retention | Negotiated frame-count, byte, fragment, and time limits sized for the admitted repair horizon; normally 2–12 units, not a universal two-frame cap |
| Decoder submission | Bounded queue respecting valid reference dependencies |
| Presentation | Prefer the newest ready frame; discard obsolete presentation work |
| Audio | Small adaptive jitter buffer with a strict ceiling |

A driver may require more surfaces than the initial ideal. Admit the actual requirement only after accounting for both memory and latency. Do not free an in-flight surface merely to satisfy a nominal queue-size target.

A queue limit applies to all hidden stages too: codec-internal surfaces, packet caches, QUIC send buffers, WSS/socket queues, browser decoder output, renderer-held frames, and shared-viewer retention. Account for physical allocations and ownership once while charging each subscriber for work it can force. A stalled viewer cannot pin an unbounded history in a shared reference-counted cache.

Separate “skip presentation” from “discard decode/reference state.” A decoded picture may remain in the decoder's reference pool after its display surface is released. Bounded queue counts are not permission to free surfaces still owned by a driver or discard a picture required by later frames. Under pressure, reduce work or restart the chain; never violate ownership to meet a metric.

### 11.3 Two different kinds of age

A static desktop can remain correct for minutes without transmitting new pixels. Therefore distinguish **age of the last pixel update** from **age of the last trustworthy observation that the source remains unchanged**.

A network heartbeat alone does not prove the capture source is fresh. The capture adapter must supply meaningful damage/change progress or a bounded verification check. If capture has stalled, the client marks the view stale even while the connection heartbeat remains healthy.

Under congestion, the system can bound its own queued work; it cannot guarantee a bound on network delivery. A deadline miss triggers degradation, recovery, or an explicit stale-state indicator. The product never labels an arbitrarily old frame fresh merely because it eventually arrived.

Record capture freshness as evidence with a scope: a successfully serviced capture operation, an OS-qualified unchanged/damage observation, or unknown. Timer callbacks and an alive PipeWire connection are not evidence that the compositor still supplies the intended desktop. For adapters that cannot prove unchanged state cheaply, use a bounded verification capture at a declared cadence and report its cost; do not invent zero-cost freshness. A legitimately static frame's pixel timestamp remains old even while fresh source-verification observations continue.

The client also reports decoder/presentation progress. A fresh host capture does not prove the client can see it. Sustained unknown/stale presentation suspends input before another action can be applied to an untrustworthy view; read-only status/diagnostics may continue. Background or occluded rendering cannot be treated as successful presentation merely because decoding completed.

### 11.4 Cursor and damage handling

Send cursor shape reliably and cursor position as replaceable state, when the OS supports separate cursor capture. Render immediate local pointer feedback while retaining the distinction between predicted local position and confirmed remote behavior.

When the capture API has already composited the cursor, avoid drawing a second cursor. A capability bit and a single rendering owner make this explicit.

Use damage metadata to avoid redundant capture and encoding. Begin with full-frame HEVC pictures and damage-aware scheduling; do not start with an independently encoded tile grid, a compositor, and a distributed tile cache.

Cursor identity, hotspot, scale, visibility, and shape bounds are explicit. A position referencing an unknown shape uses a safe fallback until the reliable shape arrives. Hide or reconcile local prediction when the remote OS confines, warps, or suppresses the pointer. Relative/pointer-lock mode and touch emulation select one cursor owner; they must not create double cursors or divergent click coordinates.

If an adapter receives partial-damage buffers, reconstruct the full valid surface before full-frame encoding. Dirty rectangles describe changed regions, not permission to encode uninitialized unchanged pixels. Surface reuse after resize/device loss clears content outside the new visible crop.

## 12. Transport and media recovery

### 12.1 One application protocol over a small transport family

The preferred native profile uses a qualified Asupersync live QUIC endpoint. Browser clients use WebTransport over an actual HTTP/3 implementation. Both carry the same application semantics, not necessarily identical transport envelopes. WSS remains an explicitly degraded native/browser compatibility profile. No second shipping async runtime or QUIC implementation is added to evade an upstream qualification failure. [S2] [S16] [S32] [S33]

Partition traffic by semantics. Reliable ordered channels carry critical input and small session operations; separate bounded channels carry clipboard, configuration, cursor shapes, and bootstrap/recovery pictures. Datagrams carry ordinary video access-unit fragments, audio, and replaceable pointer state. Bulk transfers cannot consume all connection-level flow-control credit or sit ahead of key-up on the same stream. Priorities still share congestion and bottleneck capacity; they cannot make a full congestion window or a lost packet disappear.

QUIC DATAGRAM is congestion-controlled but neither retransmitted nor application-flow-controlled by the transport. A transport ACK does not prove the application reassembled, decoded, or displayed the picture. Reserve a bounded repair budget and use receiver feedback; distinguish transport receipt, complete-access-unit receipt, decode progress, and presentation progress. All new data and repair traffic count against the same transport congestion/pacing budget. [S36]

Tailscale owns path establishment and relay selection; FrankenRemote adds no ICE, STUN, VPN, or public relay. A DERP-carried path can experience the buffering and head-of-line effects of its underlying transport even when FrankenRemote uses inner QUIC datagrams. Diagnose the actual path and measured tails; “QUIC” alone does not establish end-to-end unreliable delivery behavior. [S21]

### 12.2 A small real-time profile, not all of ATP

Reuse Asupersync framing where suitable, sockets, scheduling, cancellation, congestion machinery, and deterministic models. Upstream only the missing reusable real-time mechanisms. Do not import durable object transfer, historical frame replay, swarms, content addressing, or per-frame proof bundles into the real-time media path. The file-transfer capability of Section 15.6 deliberately reuses ATP's durable object-transfer machinery, but on its own bounded channel with its own budgets; it shares congestion capacity with media and must never sit ahead of input or starve the freshness contract.

FEC is not mandatory in the initial implementation, but **working loss recovery is mandatory** before advertising a constrained-network operating point. Baseline repair combines bounded missing-fragment retransmission for ordinary media with a bounded reliable bootstrap/recovery stream. Existing RaptorQ can support a later small-group experiment only if measured stall frequency, latency, bytes, and CPU improve. A failed recovery gate cannot be waved away as a future FEC optimization.

Fragment loss amplifies picture loss. For illustration, assuming independent 1% datagram loss and 1,100 bytes of media payload, a 50-KiB access unit occupies 47 datagrams and has about a 37.6% chance of losing at least one; a 1-MiB IDR occupies 954 and almost certainly loses something (about 99.993%). These are probability calculations, not measured network results or a claim that every picture contains those bytes. They show why “drop on loss and request another large IDR” is not a viable default without repair.

Likewise, a large recovery picture cannot beat serialization time. One MiB alone takes about 1.68 seconds at 5 Mbps before headers or competing traffic. Lower bootstrap geometry/quality or defer a high-quality refresh when necessary; neither an aggressive deadline nor a reliable stream creates missing bandwidth.

### 12.3 Codec-aware loss handling

**Use different deadlines for display freshness and reference usefulness.** A picture too old to display may still be worth repairing because it unlocks recent dependent pictures. Retain/repair it only within a separate bounded reference horizon and only while the expected result beats abandoning the chain and transmitting a new IDR. Do not repair forever, and do not require that the repaired reference itself still meet its original presentation deadline.

The sender keeps a byte- and time-bounded cache of recently emitted fragments. The receiver coalesces missing-fragment ranges after a small reordering allowance and requests only identifiers from the current stream/configuration/recovery generation. Clamp requested ranges, retry counts, frequency, and repair bytes. A duplicate retransmission is deduplicated without another allocation. If a necessary reference is beyond the admitted repair horizon or budget, abandon the affected chain and begin recovery; never pass incomplete pictures or known broken dependencies to a decoder and hope concealment preserves correctness.

Size dependency retention for the actual admitted horizon: an initial estimate is `ceil(frame_rate * repair_horizon_seconds) + 2` pictures, then clamp by negotiated count and aggregate bytes. Start within 2–12 pictures and a provisional horizon no greater than 250 ms; lower frame rate or operating point when required. Two pictures cannot cover a 120-ms repair round trip at 60 fps. Retained compressed pictures are not a promise to present the backlog; decode only the useful valid chain and present the newest result. More than the count/byte budget triggers controlled recovery, not unbounded growth.

Send startup and recovery IDRs on a dedicated bounded reliable stream, not the critical input stream. This avoids requiring an entire large IDR to survive one datagram flight unassisted. Normal P pictures use datagrams with selective repair. Only one recovery attempt per admitted pipeline is outstanding; its byte/deadline budget includes reliable retransmissions. If it becomes useless, reset/abandon that recovery stream, advance the recovery generation, and reduce the next attempt's cost. A stream reset cancels future useful delivery, not bytes already in flight. No per-frame round-trip acknowledgement is added to steady-state encoding.

Recovery generations belong to a receiving subscription; the shared encoder's capture/frame identities and codec-configuration generation are tracked separately. One subscriber can enter recovery while a healthy subscriber receives the same new IDR as ordinary valid media without resetting its decoder or control lease. For a shared encoder, late joining or one viewer's loss must not repeatedly reset all viewers. Coalesce/rate-limit IDR requests at the shared pipeline, provide a current recovery point rather than replaying a long GOP, and let existing viewers continue the valid chain across that IDR. A chronically failing viewer receives an admitted independent operating point or is paused/refused; it cannot monopolize shared recovery capacity.

**Startup/reconfiguration handshake, without circular waiting:**

```text
Host prepares encoder and, if needed, one bounded bootstrap encode
Host -> DecoderConfiguration(config generation, exact hvcC, limits)
Client -> DecoderConfigured(config generation): API configured, not first frame decoded
Host -> RecoveryAccessUnit(recovery generation, verified IDR)
Client -> FirstFrameDecoded / PresentedState for that generation
Host/client mark view usable; control grant may now complete
```

Some encoders expose parameter sets only after output begins; retain the bounded bootstrap result or replace it with a fresh matching IDR after negotiation. Never wait for a decoded frame before sending the frame required to make the decoder ready. Configuration, first decode, and visible presentation are distinct milestones, each with a timeout and cleanup path. Dependent pictures may be sent without another RTT but are buffered only within the receiver's admitted bounds until the IDR is decoded. Unknown-generation media is discarded or held within an explicitly tiny preconfiguration allowance, never interpreted using the old decoder.

An IDR after loss advances the recovery generation even if the codec configuration is unchanged. A resolution/profile/color change advances configuration and requires reconfiguration plus an IDR. Never overlay old precision data, present old-generation callbacks, or reuse the old coordinate mapping after the switch.

### 12.4 Packet size and backpressure

Calculate the actual payload budget from the inner path MTU, IP/UDP overhead, QUIC protection/header overhead, negotiated DATAGRAM frame limit, WebTransport context/session framing where applicable, and application fragment header. Tailscale documents a 1280-byte MTU; generic 1350-byte QUIC packet defaults are not safe assumptions on that interface. QUIC's 1200-byte minimum Initial UDP payload is **not** 1200 bytes of application datagram payload. Use the selected API's negotiated maximum and conservative packetization, then qualify both IPv4 and IPv6 paths. [S35] [S36] [S39]

Do not rely on IP fragmentation. QUIC does not fragment a DATAGRAM frame for the application. Access-unit fragmentation has an explicit total length, fragment offset/index, count, stream binding, frame identity, and recovery/configuration generation. Check every addition/product before allocation and reject overlapping/conflicting fragments; identical duplicates are harmless. Bound reassembly time, metadata, and bytes even for tiny or malicious fragments. [S36]

WebTransport/H3 requires its real negotiated SETTINGS, extended-CONNECT/session semantics, HTTP Datagram association, stream prefixes, and any required capsule/control handling for the chosen protocol revision. Native application QUIC ALPN is not WebTransport. Pin the draft/implementation compatibility actually tested, since the WebTransport-over-H3 specification is still versioned as a draft in the inspected material. Do not hardcode an unexplained stream prefix or assume an ATP adapter's queues implement the handshake. [S37] [S38]

Expose bounded receiver credits and progress where browser APIs do not offer native pacing/queue visibility. Streams and datagrams remain separate: reliable-stream flow control cannot protect the application from excessive datagrams. Reserve control credit, limit task polling work per turn, and integrate codec-worker feedback without blocking the network reactor. Lost final fragments and a lost final frame before idle must still be detected through bounded announced-frame/progress state, not only by waiting for the next video packet.

### 12.5 Secure WebSocket compatibility

Offer separately bound control, video, and audio WSS channels to native and browser clients. Each attaches to the same admitted peer/session with a role-specific one-use capability. A media replacement does not acquire a fresh controller implicitly. If the implementation negotiates multiplexed WebSockets over one lower TCP connection, separate logical channels do not provide transport independence; either use the qualified separate-connection profile or label the shared-transport limitation.

Use bounded application send queues plus **receiver-granted outstanding-byte/frame credit** for media. `bufferedAmount` reports outbound buffering, not successful delivery or decoder progress. The browser WebSocket API provides no general application-controlled receive backpressure, so a host must not continuously send merely because its own socket accepts bytes. Return credits only when the application frees the corresponding receive budget. Make receipt, decode, and presentation acknowledgements distinguishable. [S47]

Media already queued in TCP cannot be withdrawn by deleting an application entry. Browser `close()` is not an instant purge of previously queued messages. On stale-channel replacement, fence the old generation immediately, stop writing, close/abort from the endpoint that can do so, and ignore late old-generation data. Bound concurrent closing/replacement channels and use backoff so repeated resets do not become their own resource attack. Retain or revoke the input lease according to view freshness, not merely socket-open state. [S47]

Disable redundant WebSocket compression for already compressed media and sensitive control payloads. Keep clipboard out of the input channel. This profile still has TCP head-of-line and relay/bottleneck limits; it must never be presented as equivalent to a healthy native datagram path.

## 13. Adaptive quality and latency control

### 13.1 Objective

Minimize useful input-to-visible-response latency while maintaining readable spatial detail and avoiding sustained congestion. Frame rate, bitrate, resolution, and quantization are controls, not objectives in isolation.

A useful decomposition is:

```text
input transit + OS/application response + capture wait + conversion + encoding
+ return transit + reassembly/jitter + decoding + display wait
```

The network contribution to an actual remote response includes travel in both directions. A 100-ms round trip cannot become a 10-ms application response because the connection has abundant bandwidth.

### 13.2 Measurements and their limits

Collect RTT, delivery rate, loss, pacing and send pressure, frame sizes, capture delay, conversion/copy counts, encoder delay, decoder queue depth, presentation delay, display refresh, and thermal/resource warnings.

Use local monotonic clocks for stage durations. Cross-host one-way latency requires clock-offset estimation and an uncertainty bound; subtracting unsynchronized timestamps is not a measurement. Input sequence receipts can associate an action with subsequent capture scheduling, but do not prove that an arbitrary application had processed the action before those pixels were captured.

Record unavailable metrics as unknown rather than deriving fake certainty. In particular, browsers may not expose congestion-window or hardware-decoder internals. A completed API write is not bytes delivered, and a low observed delivery rate while the screen is idle is not a low link-capacity estimate. Mark application-limited samples, separate active-transfer intervals, and qualify the source and uncertainty of every metric used in a control decision.

### 13.3 Controller design

Start with a deterministic controller with hysteresis, bounded steps, and minimum dwell times. On persistent queue growth, reduce offered load quickly. Increase quality slowly after sustained headroom. Distinguish network pressure from encoder or decoder overload before changing settings.

For document work, first reduce unnecessary frame rate and preserve a useful pixel grid. For high-motion work, trade some spatial detail for smoother presentation. The user-facing choices can be Auto, Text, and Motion, with an expert override; do not expose a page of vendor-specific switches by default.

Reserve capacity for control, audio, codec configuration, and bounded repair. Include transport and repair overhead in the budget. Provisional operating points can begin around 75–85% of a measured sustainable delivery estimate, but must not treat a momentary throughput spike as link capacity.

Reconfigure in place only when the backend has a qualified low-stall path. Otherwise prepare a new configuration, announce its generation, and switch at a clean recovery point. Reset path-specific estimates after a material path change rather than carrying optimistic LAN settings into a relay.

Start conservatively after idle, route changes, or reconnect. Increase through small bounded probe bursts only when queues are healthy; do not infer spare capacity by sending an arbitrarily large IDR. Keep encoder pressure, sender pressure, decoder pressure, and presentation stalls separate. Bitrate changes cannot fix a client that has stopped presenting, and lowering frame rate alone can increase each picture's serialization cost if rate control spends the same total bitrate on fewer pictures.

Include keyframes, retransmissions, auxiliary precision, audio, headers, and each viewer in the budget. Apply a per-host/per-path aggregate ceiling so three independently adapting sessions do not each claim all capacity. A change that requires a new encoder is admitted against temporary overlap memory/session limits, or uses an explicit stop/restart; seamless replacement is not free.

### 13.4 Idle and overload behavior

A stationary screen reduces capture work and may stop video transmission except for genuine refresh or refinement. The first new damage event exits idle promptly. Periodic source-verification metadata distinguishes idle from a hung capture pipeline.

A client that becomes hidden or backgrounded can pause video and releases control according to its lifecycle policy. Audio does not force old video to be displayed to preserve a false notion of synchronization.

Under severe constraints, prefer a readable low-frame-rate viewport with responsive input over a high-frame-rate blur. When no useful operating point exists, explain the limitation instead of oscillating endlessly.

Test the first changed frame after a long idle interval and the final refinement before idle. Some codec/browser paths can retain output even when no more frames arrive. The adapter must prove prompt output at low cadence; otherwise use a bounded, measured flush-and-IDR recovery policy or a minimal qualified keepalive encode, and report its cost. Never add endless dummy video while claiming zero idle encoding.

## 14. Desktop text quality and precision extensions

### 14.1 Baseline workstation quality

Render at native pixels when practical, apply explicit scaling only once, handle display scale factors correctly, and transmit accurate color metadata. Avoid accidental limited/full-range mismatches, repeated RGB/YUV conversions, and inappropriate sharpening.

After motion settles, issue a higher-quality encode of the current capture if it improves the displayed image within the available budget. This “settle-to-sharp” behavior uses HEVC again; it is not a PNG or JPEG backchannel.

It improves quantization artifacts but does not restore the spatial chroma discarded by 4:2:0. That limitation must remain visible in the definition of precision quality.

A refinement encodes a newly verified current capture, not a queued pre-scroll frame. Rate-limit settle detection and reserve recovery headroom; repeated refinement cannot starve subsequent interactive damage. Decide whether backend rate-control changes actually improve the output before introducing an encoder restart. Baseline quality is not “pixel exact”: 4:2:0, quantization, scaling, and color transforms all remain visible limitations.

### 14.2 Optional true 4:4:4

When both endpoints genuinely support the required HEVC range-extension profile, offer a precision mode. Treat encode, decode, GPU interop, and presentation support as separate checks. This is an optional native-oriented extension, not a prerequisite for the baseline browser client.

Do not enable it through CPU fallback without showing the resulting performance envelope.

### 14.3 Optional HEVC-only chroma carrier

A bounded research extension can carry full-resolution chroma in the luma plane of an auxiliary ordinary HEVC stream. This preserves the single-codec policy while changing the representation. It is a known family of techniques, not a claimed first invention.

For example, full-resolution Cb and Cr planes can occupy a packed auxiliary luma surface, with neutral chroma in that surface. The arrangement must fit encoder/decoder geometry limits. It adds substantial coded-pixel and decoder-session cost, and does not automatically become mathematically lossless.

Base and auxiliary frames must have matching capture/configuration identities. Never overlay stale chroma on a newer luma frame. The base view can remain responsive while precision waits within a small bound or falls back explicitly.

Keep this experiment outside the initial release gate and inside a fixed portion of the contingency budget. Abandon it if two-stream decode, color presentation, mobile thermals, or complexity outweigh the text-quality benefit.

This experiment additionally requires dependable access to decoded component samples and their exact scaling/range semantics. A decoder that exposes only a color-converted RGB canvas can clip or transform an auxiliary carrier, and an opaque hardware surface may not allow the required reconstruction efficiently. Qualify raw-plane extraction or a correct GPU reconstruction path; otherwise disable the extension on that endpoint. Two individually decodable streams alone do not prove a working chroma-carrier client. No second stream is silently created in the default profile.

### 14.4 Mobile viewport quality

On a small display, a native-resolution viewport is often more useful than shrinking an entire 4K desktop to unreadable text. Support pan/zoom with a clearly defined coordinate transform. Initially use one active stream/viewport per viewer rather than an elaborate multiresolution tile hierarchy.

Distinguish local zoom of a full-display stream from a host-encoded crop. Local zoom changes only the client transform. A host crop changes the negotiated viewport/mapping generation and must be acknowledged before coordinate-dependent input uses it. Every click resolves through displayed-frame crop, letterboxing, rotation, DPI scale, and display origin. Out-of-content touches do not clamp into an unintended clickable edge. Advertise the wider OS-session control scope even when only a crop is visible.

## 15. Input, clipboard, audio, files, and multiple displays

### 15.1 Input is not video traffic

Use one ordered, bounded event sequence for key/button transitions, scroll actions, committed text, and input-mode changes within a lease. Relative movement uses a defined cumulative/checkpointed representation, not independently droppable deltas. Replaceable absolute pointer states may use datagrams, but every state has a sequence and geometry/viewport binding. Reliable actions and unreliable pointer states have separate sequence spaces: missing pointer datagrams cannot create gaps in the reliable action sequence. A click includes its coordinate and pointer-sequence barrier; a late pre-click motion packet cannot move the pointer backward after the click. Drag and touch-emulation transitions obey the same ordering.

**Reliability is not permission to execute stale actions.** The host issues short-lived input-validity tickets tied to the current lease, view/recovery readiness, geometry, and mapping. A ticket can be a bounded host-side record addressed by an opaque random handle; it needs no new PKI, signed-proof format, or custom cryptography. Renew active-control tickets at a cadence comfortably shorter than their lifetime, without a round trip for each action. An action references a ticket and a unique increasing request/event sequence. The input agent verifies the host-clock expiry, current authorization, and generation again immediately before OS submission. Delayed traffic cannot obtain a new lifetime at receipt. Begin with a measured path-aware ticket lifetime in the 0.5–1.5-second range, always bounded by the lease; this is a safety window, not a latency target. A path unable to renew usable tickets pauses control explicitly rather than executing an old backlog.

A timed-out or invalid action is reported as refused/expired, not transparently retried with a fresh ticket. An out-of-order gap in an ordered action sequence is a protocol error or bounded resynchronization event, not permission to skip an unknown click. Duplicate identifiers return the retained bounded result within the live epoch. Keep a monotonic consumed-sequence floor independently of the result cache: an evicted old receipt returns a no-replay/unknown-history result, never another admission of the action. A press/mode transition discarded after expiry fences queued dependent actions; release-only cleanup remains allowed so an expired key-down cannot strand a button. Periodic held-state reconciliation can release inconsistent state but cannot synthesize new presses under an invalid lease.

Separate physical key identity from committed text. Define physical positions, modifiers, repeat ownership, and platform mapping; do not let both the client and host generate the same auto-repeat stream. Committed Unicode has bounded units and a qualified injection path, independent of shortcuts. Wayland input devices do not automatically provide a universal committed-text/IME interface. Where direct text injection is unavailable, expose that limitation and keep physical-key support; never silently translate arbitrary Unicode through a guessed keyboard layout or the clipboard. Browser/OS-reserved shortcuts, keyboard lock, IME composition, and native multitouch are advertised separately.

Check focus, geometry, input mode, and display lifecycle before each bounded batch, and never hold a global authority lock across blocking OS calls. On focus loss, page suspension, disconnect, stale view, lease expiry, or revoke, clear pending presses/text and submit releases for remotely held state through the independent input path. Distinguish requested cleanup, OS submission, and uncertain result. An OS crash cannot be covered by a universal release guarantee, and a process restart must not invent certainty about physical versus synthetic key state. No durable keystroke journal is introduced to solve this limitation.

### 15.2 Local and remote control

One remote controller owns the lease. A second request can observe, wait, or request a handoff; it cannot interleave another keyboard stream. Handoff invalidates the old lease and releases its held state before admitting the new one.

Local revoke always has priority and does not depend on a round trip to the remote client. Keep a visible local indicator even when approval is disabled. A local-input-priority option can suspend remote input when the local user actively intervenes.

Local-input-priority logic must distinguish injected events from genuine local activity where the OS allows it; otherwise each injected event could revoke its own lease. When the distinction is unavailable, present the limitation instead of enabling an unstable heuristic. A new remote controller is denied or explicitly handed off, never automatically substituted because its connection arrived last.

Revoke is synchronous at the authority decision point, while OS cleanup completion is separately reported. Do not wait for a slow viewer, an approval timeout, a media callback, or a clipboard operation to fence input. Changing selected displays/audio scope is a new authorization decision in local-approval mode when it expands what was approved.

### 15.3 Clipboard

A shared clipboard is table stakes for a remote workstation: automatic bidirectional text clipboard synchronization between the controlling client and the host is a core capability of version 1, enabled by default for the controlling session, with a visible off switch on both ends. The limit starts at one MiB of validated UTF-8 per item, with no automatic file, HTML, or URL opening. Image clipboard content is a negotiated optional capability carried over the bounded transfer channel of Section 15.6, never smuggled through the control parser. Clipboard access belongs to the controlling session, not every read-only viewer, and local approval, when enabled, covers clipboard synchronization like any other observation capability.

Use sequence identifiers and source labels to prevent echo loops. Clear session buffers on closure. Clipboard contents do not appear in routine logs or diagnostic traces.

Use a separate bounded transfer with declared total length and chunk sequence, not a one-MiB exception smuggled through the ordinary 64-KiB control-message parser. Acquire platform clipboard permission explicitly; the Linux portal request must be made before its parent RemoteDesktop session starts and honored only if the returned grant permits it. Clipboard support can be absent even when keyboard/mouse control works. [S42]

“Paste text” means the chosen explicit product operation; setting the remote clipboard is not proof that an application consumed it. Reject invalid encodings and cancellation-sensitive partial transfers before publishing to the OS. Clear private transfer buffers on closure where practical, but do not promise to erase every OS/clipboard-manager copy or overwrite a newer local clipboard value during cleanup. Clipboard content and text input never appear in error messages, URLs, or routine traces.

### 15.4 Audio

Use Opus as the only audio format in both directions, with a small explicit `libopus` exception and a recorded license/build configuration. Audio is bidirectional in version 1: host playback audio flows to the client, and the client's microphone can flow to the host. Playback audio capture is disabled until locally enabled and is active only for an admitted session that requests it. Microphone forwarding is explicit-enable per session on the client — a talk toggle backed by the client OS's microphone permission — never activated automatically by connecting, and surfaced by a visible indicator on both ends. Audio scope in each direction is a separately advertised/approved capability, not inferred from which display is visible. [S29]

Client-to-host audio terminates in a per-OS qualified virtual-microphone endpoint that ordinary host applications can select as an input device: a PipeWire virtual source on Linux, a signed user-space CoreAudio server plugin on macOS, and on Windows a signed virtual audio endpoint driver. The Windows endpoint is the riskiest row — genuine driver-class work with its own signing and packaging requirements, not handwritten Rust, counted like other native components — and it receives its own Phase 0 spike. Where a host OS's endpoint cannot be qualified, microphone forwarding is a typed unsupported capability on that host, never a silent fake device or an undisclosed third-party driver download. The uplink reuses the same Opus framing, sequence/timeline, jitter, and generation rules as the downlink with the roles reversed; browser clients capture through getUserMedia under its own permission prompt and encode within the bounded worklet/WASM budget described below.

Start with 48-kHz mono/stereo, normally 10-ms packets. Convert/resample the actual capture-device rate and format at one controlled boundary; do not assume every device produces 48 kHz. Negotiate channels and maximum packet duration/decoded samples, validate them before allocation, and use sequence numbers plus a monotonic sample timeline. Keep capture gaps, silence suppression, device changes, and configuration resets explicit.

Implement the actual OS source: supported ScreenCaptureKit audio on macOS; a selected output monitor through qualified Linux audio APIs; WASAPI render-endpoint loopback on Windows. Windows endpoint loopback can contain sound from other terminal-services sessions, so it must not be represented as audio isolated to the selected desktop user or application. Local setup either explicitly authorizes the disclosed endpoint-wide scope or leaves audio unavailable. Do not add a new per-process mixer or privacy-isolation subsystem simply to hide this limitation. [S23] [S46]

Use a small bounded jitter buffer, packet-loss concealment, and slow drift correction anchored to sample timestamps and the client's audio clock. Maintain a bounded A/V offset; do not hold fresh video behind delayed audio or chase every timestamp fluctuation by resetting playback. Late packets are discarded, not accumulated. A device change or reconnect resets the audio generation and drops old samples so the user cannot hear obsolete buffered conversation after resume.

For browsers, probe an actual supported Opus decode/output path. A small WASM Opus decoder with AudioWorklet is the fallback, not a HEVC software decoder. Worklet processing has bounded, preallocated work and no network fetches or blocking calls. Use a bounded ring/credit protocol, with transferable buffers as a baseline; SharedArrayBuffer is an optional optimization requiring the appropriate cross-origin isolation policy, not an unmentioned deployment prerequisite. Audio starts through a browser-permitted user gesture and stops on the defined lifecycle transition. Measure WASM/audio-buffer overhead on phones before claiming the 10-ms packet choice yields low end-to-end audio latency.

### 15.5 Displays and viewers

Represent each display with a stable local identity, pixel bounds, logical bounds, scale, rotation, and geometry generation. Hotplug or reconfiguration creates a new geometry generation before coordinate-dependent input is accepted.

Transmit only selected displays. Multiple visible displays use independently identified streams and a shared session budget, not one enormous bounding-box framebuffer. Suspend unobserved displays.

Share an encoded stream among viewers only when their codec configuration, viewport, and operating point match. Different quality requirements can consume an additional encoder session, which must pass admission. A slow read-only viewer must not backpressure the primary controller; it can be degraded or disconnected independently.

A display ID is stable only within the OS's actual identity guarantees; connectors and numeric IDs can be reused after hotplug. Bind each catalog to the OS-session/geometry generation, and handle missing or ambiguous mappings with refusal. The single controller's pointer/keyboard target is explicit when it views several displays. Read-only viewers cannot move the host cursor, change the controller's viewport, activate clipboard, or force global quality changes.

Per-viewer subscriptions reference share-session pipelines, not another viewer's task region. A late join waits for a fresh qualified recovery point; it does not force existing viewers to replay old frames. Read-only cursor feedback and local UI focus remain separate from control authority. Add counters for total pipelines, encoder sessions, surfaces, cached bytes, and queued repair work across all viewers.

### 15.6 File transfer and synchronization

Explicit file send/receive between an admitted controlling client and the host is version-1 scope, together with locally configured folder-synchronization jobs. The transfer machinery reuses asupersync's ATP — resumable, integrity-verified object transfer — over a separate bounded channel inside the admitted session. FrankenRemote designs no new transfer protocol, and the real-time media path is unchanged: Section 12.2's exclusions apply to media, not to this capability. Transfer traffic shares congestion capacity with everything else and must never sit ahead of input on a shared resource or starve the freshness contract; it is admitted against its own byte, rate, and concurrency budgets.

Authority follows the existing model. File transfer is a separately advertised capability of the controlling session — read-only viewers get nothing — and local approval, when enabled, gates it like any other sensitive capability. The client cannot browse or address arbitrary host paths: sends land in a locally configured drop directory, receives come from explicit host-side selection or a configured share directory, and folder-synchronization jobs name explicit local directories on both ends. Every received path is validated against traversal and symlink escape before any write; nothing received is automatically opened or executed; partial transfers are written to temporary names and published atomically, so cancellation or crash never leaves a half-visible file. Transfer contents never appear in logs or diagnostics; progress metadata does.

Folder-synchronization jobs are configured locally on the host and accepted explicitly on the client, one-way or bidirectional, with a simple explicit conflict policy — keep both with a conflict marker rather than silently overwriting — and ATP's resumption semantics across reconnects. Explicit send/receive lands with the Phase 2 desktop work (and on mobile/browser in Phase 3, within each platform's file-access limits); synchronization jobs complete in Phase 4. None of this is required for the Phase 1 exit gate.

## 16. Desktop, mobile, and browser clients

### 16.1 Shared core, thin native shells

Share connection setup, protocol parsing, session state, stream recovery, input semantics, quality feedback, and diagnostics in Rust. Do not force every platform into an identical event-loop or rendering abstraction when the native surface API is the important performance boundary.

Use small native shells for window creation, menus, permissions, and display presentation. AppKit/Metal, Win32/D3D, and an appropriate Linux windowing layer are candidates. Choose one minimal Rust windowing integration where it preserves hardware-surface interoperability; qualify it before committing to a broad UI framework. No Electron or bundled Chromium in the daemon or desktop client.

The GUI needs a machine picker, recent hosts, a display viewer, an input toolbar, connection quality, permissions, and settings. It does not need a web application platform. Small Swift/Objective-C and Kotlin/Java bridges are acceptable where they reduce fragile FFI or platform lifecycle code; count and audit them separately rather than pretending all platform glue is Rust. On mobile the native layer is deliberately more than glue: Section 16.2 commits the iOS and Android applications to first-class native user interfaces.

The windowing choice is a release-blocking interoperability decision, not permission to hand-write three widget toolkits. Select one minimal maintained shell/window integration where possible, retaining native video-surface presentation adapters. Count the real accessibility, DPI, input, menu, and lifecycle work in the client budget. Avoid GPU-to-CPU readback just to fit a preferred GUI abstraction.

Qualify the minimum OS versions, CPU architectures, ABI targets, and graphics APIs in Phase 0. Initial certified lanes can prioritize Linux x86-64, Windows x86-64, macOS Apple Silicon, and modern mobile devices; Intel macOS and ARM desktop targets are separate tested rows, not implied by an OS name. A supported source target is not automatically a signed, hardware-qualified release target.

### 16.2 iOS and Android

Mobile clients are viewers/controllers, not daemons. Tailscale remains the system's installed VPN client. The application must handle network changes, foreground/background transitions, orientation, safe areas, software keyboards, hardware keyboards, and thermal/resource pressure.

Both applications live in this repository — `mobile/ios` and `mobile/android` — and are built by repository-owned commands; there are no satellite mobile repositories. Each is a genuinely native application: SwiftUI on iOS and Jetpack Compose on Android for the machine picker, saved hosts and host links, the session viewer chrome, input toolbars and touch-mode controls, the talk toggle, settings, permission explanations, and connection diagnostics — following each platform's conventions for navigation, appearance, text input, and accessibility. The shared Rust core (`fr-client` and the crates beneath it) owns connection establishment, protocol and session state, media scheduling and recovery, input semantics, and diagnostics; it is exposed to Swift and Kotlin through one narrow, audited boundary per platform, whose binding mechanism (hand-written C ABI plus a maintained wrapper, or a qualified binding generator) is chosen by a short decision note during mobile bring-up, not an open-ended framework survey. Decoded video is presented through the platform pipelines Section 8.3 already requires — VideoToolbox output on iOS, MediaCodec-to-Surface on Android — never routed through the UI toolkit's ordinary image path.

On Android, decode to a Surface rather than repeatedly copying decoded pixels into Rust memory. On iOS, use the system video pipeline. Media APIs remain foreign/system trust boundaries even when called through safe-looking Rust wrappers. [S28]

Provide trackpad mode and direct-touch mode with visibly different semantics. Long press, secondary click, scrolling, modifier keys, and explicit text entry should be discoverable. Do not map every touch gesture to a guessed remote gesture without a stable coordinate model.

A backgrounded client releases control. Resume obtains a fresh lease and recovery frame. It does not synthesize missing pointer movement or replay queued taps.

Do not assume permission to enumerate Tailscale peers through another app's private storage or local API. Use saved FQDNs/links and the optional restricted directory. Re-evaluate the actual VPN route after Wi-Fi/cellular transitions; transport migration cannot override changed peer identity or expired authority. Mobile background suspension may prevent a clean client teardown, which is why the host lease and queue expiry are mandatory.

Keyboard/IME composition is local UI state until a committed-text action is submitted; cancel it on view/session replacement rather than replaying it into a different desktop. Orientation changes require a current client mapping even when host geometry is unchanged. Tablet pens/native touch gestures may be future extensions; baseline mouse emulation remains explicit.

### 16.3 Browser capability contract

The browser bundle is self-hosted by `frd`, with no CDN dependency, third-party analytics, remote fonts, or public service required to view the desktop. The WASM module handles the common Rust state machine. A narrow JavaScript boundary handles WebTransport/WebSocket, WebCodecs, display, audio, and DOM input.

Probe the actual HEVC configuration with `VideoDecoder.isConfigSupported`, then decode and present a short test sequence. Configuration support, smooth performance, and hardware acceleration are different claims. A preference for hardware acceleration is not a guarantee. [S14]

Generate the complete HEVC codec identifier from actual stream configuration. The baseline wire profile fixes a decoder configuration record (`hvcC`) and four-byte-length-prefixed NAL units in complete access units; adapters can normalize to Annex B for native APIs that require it. Do not hardcode an incomplete codec string or mix packet formats without corresponding configuration. [S15]

Close decoded `VideoFrame` objects promptly, bound the decode queue, and prefer worker-based decoding where supported. Preserve keyboard, pointer-lock, fullscreen, clipboard, and audio restrictions as explicit capabilities.

The client is supported only when its HEVC decode, presentation, secure transport, and required input capabilities pass. With HEVC-only, some browser/OS/hardware combinations will be unsupported. WSS can solve a transport gap; it cannot solve a missing codec. Do not conceal that distinction behind a generic “browser supported” badge.

The browser qualification bar is current Chrome and Safari on OS/hardware combinations where their HEVC path passes this probe; the browser client is complete when those pass. Other browsers are probe-determined best effort, and their gaps are acceptable rather than release-blocking. Where a qualification-target browser lacks a qualified WebTransport path, the bounded WSS profile is its supported transport, under the same degraded-labeling rules. This bar selects where qualification effort goes; it does not replace the per-combination probe or license a brand check.

Keep `hvc1`/`hev1` parameter-set rules and the declared NAL-length width consistent with the actual bytes. Parse/normalize encoder output once in the media adapter; do not concatenate arbitrary network chunks and call each chunk a frame. Each `EncodedVideoChunk` contains one complete admitted access unit, with a correct key/delta designation and presentation timestamp in the API's units. Reject unexpected in-band parameter-set changes until a new configuration is negotiated. The HEVC WebCodecs registration distinguishes configuration-record and Annex-B forms. [S15]

Do not call `VideoDecoder.flush()` after every frame or use it as an arbitrary “show now” operation: the WebCodecs flush algorithm requires the next submitted chunk to be a key chunk. Test a single IDR, long static idle, one P picture, low-rate updates, and the final frame of a burst. Configure/reset/recovery is explicit; a flushed/reset decoder cannot accept an ordinary continuation as if reference state were unchanged. [S14]

`decodeQueueSize` measures pending API work, not all native surfaces or physical presentation. Also bound output frames and renderer retention, close `VideoFrame` objects once rendering no longer needs them, and distinguish decode callbacks from submitted-to-compositor or optically measured display. Browser surface loss, decoder reclamation, `pagehide`, back/forward-cache restoration, and worker failure trigger a fresh usable-view check before control resumes. Hidden-tab throttling must not preserve input authority.

The web assets themselves have bounded sizes and a version handshake with the host. A stale cached client either negotiates a supported protocol or reloads safely without replaying input; no service worker may silently retain screen, clipboard, or authority-bearing session data.

### 16.4 Browser security

Tailscale authenticates the connecting node, not the web origin running on that node. A hostile page must not obtain control merely because its user's browser has a tailnet route. Serve the UI from the configured host HTTPS origin and apply a same-origin bootstrap/session protocol in addition to node admission.

An ordinary navigation GET may legitimately have no `Origin` header; it serves only static application content, never issues an authority ticket, grants a lease, or starts capture. Validate the configured Host/authority and, where exposed, SNI; do not trust an arbitrary Host-derived origin or redirect. Require the exact expected Origin for browser state-changing requests and WebSocket/WebTransport establishment. Deny wildcard CORS, null origins, unexpected cross-origin fetches, and state changes via GET. Check Fetch Metadata where supported as defense in depth, not the sole authority test.

Obtain a short-lived, one-use nonce through an origin-checked same-origin HTTPS bootstrap POST, bind it to the verified peer and requested session/role, and require it before any sensitive operation. Return only non-executable JSON with no-store/nosniff protections; no JSONP, nonce-bearing script, or cross-origin-readable bootstrap resource is allowed. Browser WebTransport/WebSocket APIs do not provide arbitrary custom handshake headers, so authenticate the application session using a bounded first reliable message after the origin-checked transport opens. Until that succeeds, allow no pixels, audio, input, expensive codec work, or directory expansion. Native WSS clients follow the same origin/nonce attachment contract; the native QUIC ALPN has its own authenticated application handshake. Missing Origin is not an alternate WSS authentication mode.

Every auxiliary channel uses a role-specific attachment ticket. Nonces/tickets are never put in query strings, host links, logs, or referrers. A bare session ID cannot attach a second socket. Rate-limit unauthenticated handshakes and expire unused tickets. Local approval, when enabled, follows successful identity/origin/session checks and still precedes observation.

Use a restrictive CSP, `frame-ancestors 'none'`, safe referrer policy, no third-party scripts, no permissive embedding, and `no-store` on sensitive responses. Permit only the explicit script/WASM/worker features the client needs. Avoid persistent service-worker authority and invalidate browser state on session closure. Test malicious iframes, DNS rebinding/Host tricks, cross-origin WebSockets, stale tickets, replay, and back/forward-cache restoration. These are application-origin protections, not a new account or pairing system.

## 17. Protocol shape and interoperability

### 17.1 A small wire specification

Publish one concise `PROTOCOL.md` covering version negotiation, limits, identity binding, capability selection, session generations, media configuration, input semantics, errors, and teardown. Ship golden messages and an interoperable test peer before freezing version 1.

Use a bounded binary framing format for high-frequency traffic. A fixed header plus bounded length-delimited fields is sufficient; an extensible serialization ecosystem is unnecessary. CLI and robot output use JSON with stable field meanings. Never deserialize network input directly into an unconstrained object graph.

Messages should include:

| Class | Representative messages |
|---|---|
| Negotiation | ClientHello, HostCapabilities, SelectedConfiguration, Refused |
| Session | SessionOpened, ApprovalRequired, LeaseGranted, LeaseRevoked, Challenge, ChallengeResponse, InputTicket, ChannelAttach, Closed |
| Displays | DisplayCatalog, GeometryChanged, SelectDisplay |
| Media | DecoderConfiguration, DecoderConfigured, RecoveryAccessUnit, FirstFrameDecoded, AccessUnitFragment, RepairRequest, RecoveryRequest |
| Input | KeyTransition, ButtonTransition, PointerState, RelativeCheckpoint, Scroll, CommitText, HeldState |
| Auxiliary | CursorShape, ClipboardBegin/Chunk/Commit, AudioConfiguration, AudioPacket |
| Feedback | PresentedState, ReceiverPressure, StageMetrics, QualityDecision |

These names are proposed protocol categories, not existing implemented APIs.

Fix byte order, field widths/varint encoding, timestamp units, signed-coordinate representation, maximum nesting, and canonical error handling. Each message has an allowed sender role and state; a malicious host cannot send a host-input command that the client applies to its own OS. Separate parse validity, negotiated capability, and authorization checks. Validate limits before constructing platform objects.

Define a compact stream binding for session, display, codec configuration, recovery chain, and viewport. Datagrams carry enough identity to reject stale work without repeating full random tokens. Sequence numbers have no ambiguous wrap/reuse. Unknown required flags, contradictory fragment metadata, invalid role changes, and inconsistent selected settings fail closed. Golden fixtures cover every transport profile, not only a self-round-tripping encoder/parser pair.

### 17.2 Explicit limits

Start with a 64-KiB ordinary control-message ceiling, one-MiB complete text clipboard transfer on its separate chunked channel, 16-MiB maximum encoded access unit, dimensions no larger than 8192 per axis, and at most 16,777,216 coded pixels per picture. These are protocol/resource ceilings, not default operating points or promises of native-resolution support for every monitor. Endpoint capability and application budgets negotiate downward; alignment padding counts in the allocation check.

Replace the old universal two-incomplete-picture limit with a negotiated dependency/reassembly window, initially 2–12 pictures sized by Section 12, **also** constrained by bytes. A provisional per-viewer compressed-media budget of 32 MiB covers incomplete/held access units and associated metadata, while the shared sender repair cache has a separately admitted byte budget. A receiver accepting one 16-MiB picture does not thereby accept twelve such pictures. Global host/client limits cap all streams, viewers, pending handshakes, and closing generations. Values are experimental starting points and must be tested with legitimate keyframes and mobile memory pressure before freeze.

Bound stream count, cursor dimensions/shape bytes, clipboard chunk count, audio decoded samples, outstanding input events, repair ranges, codec-configuration size, NAL count, and decoder surface requirements. Validate VPS/SPS/PPS dimensions, bit depth, layers, reorder/decoded-picture-buffer demands, crop, and consistency with the admitted subset before decoder configuration. Reject unannounced parameter changes. A small bounded header validator protects the boundary; it is not a from-scratch HEVC decoder or proof that a system decoder has no vulnerabilities.

All length/stride/product arithmetic is checked before allocation and before FFI. Decoded-surface memory, including reference pools and actual hardware requirements, is admitted separately from compressed bytes. Legitimate content that exceeds its negotiated ceiling triggers a lower operating point or typed refusal; it cannot bypass the cap. Keep values in one tested limits structure, with safe local administrator overrides constrained by absolute implementation bounds.

### 17.3 Versions and compatibility

Version the protocol, robot schema, and capability names separately. Required unknown features cause a typed refusal; optional unknown fields can be ignored only under a bounded framing rule. No network message grants permission to install an unrecognized codec or download executable code.

Keep compatibility to a small declared window. Do not accumulate a permanent implementation of every draft protocol. New sessions negotiate; active sessions keep their admitted configuration until an explicit generation change.

Version the transport profile and underlying WebTransport draft compatibility separately from FrankenRemote application messages. A successful TCP/WSS fallback retains the same authorization, message bounds, and input-expiry semantics; it is not a less secure legacy protocol. Record the negotiated profile in diagnostics and reject downgrade attempts that remove required security checks. Transport selection can change only through an explicit new bound channel/session transition.

## 18. Agent ergonomics and FrankenTerm integration

### 18.1 Proposed command surface

```text
fr hosts --json
fr connect workstation
fr status --json
fr doctor --json
fr inspect workstation --json
fr robot session open workstation --role control --json
fr robot observe workstation --display 1 --json
fr robot input workstation --lease LEASE --request-id REQUEST --json
fr robot session close workstation --json
fr disconnect workstation
frd install
frd status --json
frd approval set local
frd approval set none
frd sharing set own-user
frd sharing set tailnet
```

These are planned interfaces, not commands claimed to exist today. The initial command set stays small; a full workflow language and an MCP server are not required to make JSON automation useful.

Every robot response distinguishes success from partial submission, cancellation, refusal, or unknown external effect. Use the useful FrankenTerm envelope convention, with schema version, timestamps, error code, and a specific next action. [S6]

An agent explicitly opens a session with view/control role and receives status, limits, and an opaque local lease handle. Commands reuse that live local client session rather than secretly opening a new host controller for every input event. A handle printed on the command line is not sufficient authority: the local client authenticates its caller and holds the live channel/tickets. Actual bearer material is kept out of process arguments and ordinary JSON diagnostics. Local-approval mode works identically for humans and agents; automation cannot set host approval policy remotely.

### 18.2 Observations with validity boundaries

An agent observation includes display geometry, configuration generation, frame identity, source-freshness status, capture/presentation timestamps with uncertainty, and current control authority. An optional screenshot is a user-requested observation artifact, not a second interactive video transport codec.

An action can require an expected geometry generation, current lease, and maximum observation age. If these preconditions no longer hold, refuse rather than clicking an old coordinate system. Window/focus preconditions are best-effort checks unless the platform provides a genuinely atomic facility; do not claim an application cannot change between check and injection.

After input, report “submitted to OS” separately from “observed application result.” Ordinary arbitrary pixels do not prove an application committed a semantic action. A test application or an authorized semantic adapter can provide stronger evidence.

Bind a screenshot/artifact to the observation that produced it and report whether it was actually decoded, merely submitted to a compositor, or instrumentally observed. Do not infer semantic success from a new frame number. An expired action returns a useful typed reason and requires a new observation/reasoned decision; the client must not auto-refresh its precondition and repeat a potentially destructive action. Explicitly label partial batch submission and unknown effects.

### 18.3 Optional FrankenTerm semantics

When a shared application is FrankenTerm and the user explicitly grants the additional capability, an adapter can expose its existing pane state and text through the `ft robot` interface. An agent can then use actual terminal semantics instead of reading pixels for every task.

This capability must not bypass the same session authorization, extend to unrelated windows, or become a general remote command execution endpoint. Treat terminal output as untrusted application content. Keep the integration optional and out of the baseline daemon's dependency graph.

Read-only pane observation is the initial semantic extension. Restrict it to the explicitly selected pane/session and revalidate visibility/authorization; a terminal mux can contain hidden or unrelated panes. Do not expose the entire `ft robot` command namespace or its write/execute operations as a shortcut. Schema reuse is independent of granting terminal access.

### 18.4 Diagnostics as a reusable asset

A diagnostic bundle should contain versions, hardware capabilities, negotiated settings, connection path, stage timings, queue high-water marks, drops, recovery events, and controller decisions. Screen content, clipboard contents, typed text, and credentials are excluded by default.

A bounded ring buffer is enough. Export to a file on demand; no database, remote telemetry service, or full observability stack is required. Feed the same sanitized events into Asupersync's deterministic simulation to reproduce adaptation and teardown decisions.

Sanitization also covers host/user names, local paths, window titles, network addresses, certificate identifiers, and error strings returned by libraries. These can be useful locally but require explicit review before export. Bound trace bytes and event rates, and never include codec payloads, TLS keys, session nonces, or input text. A deterministic replay reproduces recorded policy/state transitions; it does not reproduce a GPU driver or application behavior merely from timing metadata.

## 19. Memory safety, security, and resource limits

### 19.1 The memory-safety boundary

All project-owned protocol, policy, admission, scheduling, and session code uses `#![forbid(unsafe_code)]`. Unsafe code is confined to named platform/media crates with narrow safe interfaces, documented ownership, explicit callback lifetimes, and reviewed thread-affinity rules.

FFmpeg, system frameworks, drivers, and some dependency internals remain outside a blanket Rust memory-safety guarantee. Asupersync's selected TLS provider may itself introduce native/unsafe internals. Audit the complete feature-resolved graph rather than claiming that a Rust top-level API makes the entire process pure safe Rust. [S3]

Use process isolation where it gives meaningful containment. Do not move every function into a new process merely to make an architectural diagram look safer. The important split is that hostile or hung media work cannot retain input authority or take down an all-powerful broker.

Enforce the unsafe/dependency boundary in CI through feature-resolved target builds, not only crate-level claims in the README. FFI wrappers document callback threading, pointer provenance, buffer padding, ownership transfer, and shutdown order. Apply sanitizers to compatible native builds and fuzz the safe parsers; neither tool proves hardware-driver safety. Client decoders ingest potentially hostile host-generated media and require the same bounds and containment analysis as host encoders.

A worker process alone is not a security sandbox. Use OS-supported privilege/token reduction, restricted inherited handles, protected executable paths, and network/filesystem restrictions compatible with GPU access, then test the actual shipped boundary. Where an adapter cannot be sandboxed meaningfully, record the remaining trust explicitly. No worker gets certificate keys, arbitrary Tailscale administration, or the input-approval channel.

### 19.2 Threat model

Protect against unrelated network clients, shared external tailnet principals, malicious web origins on admitted machines, malformed protocol/media, stale worker callbacks, local unprivileged IPC forgery, and resource exhaustion by an admitted peer.

The host OS, its selected interactive user, installed Tailscale authority, and relevant GPU/system media stack are trusted to the extent required by their roles. A malicious local administrator can already control the machine. A compromised node inside the locally selected sharing scope has the access that scope deliberately grants; the own-user default keeps other users' devices and tagged nodes — the class of machine most commonly compromised — outside that trust until the local user explicitly widens it. Optional Tailscale restrictions and local approval narrow the trust further; none of this turns admission into hidden pairing.

Do not expose FFmpeg's arbitrary file/protocol/filter machinery through the wire format. Treat clipboard text and terminal semantic data as untrusted content, never executable instructions for the daemon.

Installation and update authority remain local. No admitted tailnet peer can change the shared OS user, enable audio globally, disable approval, modify tailnet policy, install codecs, run arbitrary commands, or update the host binary through the desktop protocol. Desktop control still lets an authorized controller perform whatever the selected user's GUI permissions allow; the protocol must not misrepresent that broad authority as application sandboxing.

Protect credentials/certificate keys and configuration with platform-appropriate ownership/ACLs, reject symlink/path substitution in privileged operations, and verify role-specific IPC capabilities. Same-user malicious code and the selected GPU stack are explicit trust limits, not silently claimed to be isolated by Rust or a UID check.

### 19.3 Denial-of-service controls

Rate-limit pre-admission connections, expensive codec probes, control requests, cursor-shape uploads, recovery requests, and diagnostic exports. Admission accounts for encoder sessions, GPU surfaces, CPU fallback, decoder limits, bandwidth, and per-viewer memory.

Bound both message counts and bytes. A queue of two access units can still be too large if access-unit size is unconstrained. Apply resource limits before a malicious stream can allocate its declared decoded dimensions.

Default telemetry is local and excludes screen/clipboard/input payloads; diagnostic metadata is separately sanitized before export. No screenshot retention, keystroke logging, clipboard logging, or automatic session recording.

Add global limits for handshake concurrency and duration, pending approvals, half-attached WSS channels, retained request receipts, decoder reconfigurations, and worker restarts. Idle session timeouts cannot be extended indefinitely by unauthenticated garbage or stale challenges. Cancel codec probes whose requesting session disappears. A peer cannot keep a hardware session reserved merely by starting negotiation.

Metadata allocations count too: thousands of zero-byte fragments or stream IDs can exhaust memory without crossing a payload-byte limit. Unknown generations, large cursor shapes, long Unicode names, and invalid parameter-set requests are rejected before expensive work. Continue to service revoke/expiry fairly during floods; no high-priority queue is unbounded.

## 20. Dependency policy, build system, and distribution

### 20.1 Dependency allowlist

The essential graph contains Asupersync at an audited revision, the smallest selected serialization/CLI utilities, narrow OS bindings, one selected FFmpeg binding family for the desktop platforms that need it, Opus, and browser bindings. Reuse already-approved cryptography through the transport rather than creating new primitives.

Do not add Tokio, libwebrtc, Electron, a Go Tailscale embed, a full application server framework, or a database by convenience. Do not import a huge Franken crate to save a few hundred lines of simple integration code.

Inspect transitive features on every target. Target-specific dependencies stay target-specific. A browser build must not inherit desktop FFI; a host daemon must not inherit a desktop GUI dependency.

The allowlist permits small established dependencies that avoid writing fragile TLS, platform bindings, parsers, or windowing code from scratch. “Few dependencies” means a reviewed minimal feature graph, not reimplementing every mature component or declaring generated unsafe bindings safe. Track direct, transitive, native, build-only, and test-only dependencies separately; test-only independent QUIC/HEVC peers do not become shipping libraries.

### 20.2 Toolchain and native artifacts

Use Rust nightly pinned to an exact date that passes the target matrix. “Nightly” means deliberately updated and tested, not silently moving on every build. Lock dependencies and retain the native compiler/SDK versions used for shipping artifacts.

Give each native artifact a source hash, configuration, ABI identity, checksum, and license manifest. Prevent an update from loading a different FFmpeg ABI from an arbitrary search path. Package GPU vendor integrations and system SDK dependencies according to the exact allowed redistribution model.

Retain a tested rollback toolchain and upgrade nightly deliberately across native, Android/iOS, and WASM targets. Avoid nightly-only features unless they provide a measured capability needed by this project. Pin third-party source revisions and reproduce native artifacts on declared builders; do not run a network downloader from `build.rs` or accept an unsigned prebuilt library solely because its filename matches the target. Cross-compilation and native execution are distinct lanes.

### 20.3 Native release lanes

Make repository-owned build/test commands authoritative. Run them on native Linux, macOS, and Windows builders and through the existing local/self-release infrastructure where appropriate. GitHub Actions may invoke those commands, but it is not the only way to produce or verify a release.

Ship a signed/notarized macOS process family, signed Windows packages, and a reproducible Linux package/install path. Mobile releases use their platform signing and entitlement mechanisms. Browser assets are shipped with `frd` and versioned with the protocol implementation.

Installation detects Tailscale, permissions, certificates, conflicts, and hardware support before advertising a working host. Uninstall removes FrankenRemote services and files but does not remove the user's Tailscale installation or rewrite tailnet policy.

The installer performs idempotent service/user-helper registration, checks the actual Tailscale installation variant, and modifies only narrowly required local firewall rules for the chosen tailnet service. It never overwrites a user's Tailscale grants or enables a public/LAN listener after failure. Setup records host-sharing/audio consent locally; repair preserves it without broadening permissions.

Document the supported OS/architecture/compositor/driver combinations and the exact install/start behavior. A desktop daemon, mobile app, and browser bundle can share Rust code without sharing identical startup or signing models. Include offline installation of packaged assets; certificate issuance and initial Tailscale enrollment still require their own connectivity.

### 20.4 Updates

Stage a complete signed generation, validate it, and activate atomically. Do not replace only one helper in an ABI-coupled process family. By default, defer activation until idle; an explicit immediate activation ends sessions cleanly first.

Preserve a rollback generation and write an installation receipt. Validate that permissions, helper identity, native libraries, and local IPC still work after activation. Version strings alone do not prove a successful installation.

Verify the publisher/signature trust anchored by the installed package or platform distribution mechanism, and reject unsigned or unexpected-role artifacts. Automatic update metadata cannot silently force a rollback to a vulnerable build; an explicit local rollback uses a previously verified package under local authority. Cap download/extraction sizes and validate archive paths before privileged writes. Resume after partial failure either activates a complete verified generation or retains the previous one; no mixed helper/FFmpeg ABI family is permitted.

## 21. Performance objectives and measurement

**Every number in this section is a proposed engineering objective or experimental starting point, not a measured FrankenRemote result.**

### 21.1 Operating envelopes

| Scenario | Proposed target and qualification |
|---|---|
| Idle host, no viewers | Less than 25 MiB resident memory for the idle broker on qualified targets; less than 0.1% of one CPU core averaged over a defined quiet interval; no active capture or encoder |
| Healthy direct path, 2–5-ms RTT | 1080p60 and 4K60 on qualified hardware; input-to-photon p50 at or below 45 ms and p95 at or below 70 ms in the instrumented test workload |
| Typical WAN, approximately 40-ms RTT | 1080p60 where capacity permits; instrumented input-to-photon p95 target at or below 130 ms |
| Constrained path, approximately 120-ms RTT, 5 Mbps, 1% independent datagram loss | Test readable document interaction with an admitted 720p/1080p viewport, exploring 15–30 fps but allowing lower cadence. No support claim until real fragmentation/repair shows bounded stalls and useful text; bounded application queues alone do not prove usability |
| Warm media path, permissions/certificates already valid | First useful frame within 500 ms on a healthy direct path; measure cold on-demand worker/encoder startup separately. The idle daemon does not secretly retain an encoder just to satisfy a warm target |
| Worker crash or transport loss | No reuse of the old input lease; control cleanup begins independently of media-worker recovery |
| Static screen | No continuous full-frame encoding solely to maintain a nominal frame rate; trustworthy capture freshness remains observable |

The idle target applies to the broker, with the session helper measured separately. Report the whole installed process family's memory and CPU as well; do not win the benchmark by hiding work in another process.

Active sessions require substantially more memory. A 3840×2160 BGRA surface alone occupies about 31.6 MiB, before stride padding; several capture/encoder/decoder surfaces make a tiny active-RSS claim implausible. Count GPU allocations separately from CPU RSS and report both.

Specify measurement scope before reporting numbers: process-family CPU/RSS and GPU allocations, target refresh, capture/encode/decode path, direct/relay mode, cold/warm state, and local client focus. Report the existing Tailscale process separately, including incremental streaming load when measurable, rather than either charging its whole idle footprint to FrankenRemote or hiding its transport cost.

Approval/OS prompts, Tailscale login, sleeping hosts, and certificate provisioning are not part of a pre-authorized warm-session latency measurement. They remain real user-facing setup time and need separate reporting. Frame-count bounds, lease deadlines, and wall-clock responsiveness during an OS/GPU failure are different guarantees.

### 21.2 Starting bitrate ranges

Initial experiments can sweep roughly 8–20 Mbps for 1080p60 and 25–60 Mbps for 4K60, with higher or lower values as content and hardware require. These are search ranges, not codec guarantees. Text precision, high refresh, chroma extensions, and noisy content can change the requirement materially.

Measure useful delivered quality at equal latency and equal bandwidth, not just a codec's PSNR at unconstrained buffering.

### 21.3 Measurement methods

Use an instrumented host application whose input handler changes a known visual target, paired with client timestamps and an optical/high-speed-camera lane where practical. This measures input-to-photon rather than merely packet-to-decoder submission.

Report distributions and worst useful tails, not just means. Separate capture cadence, encoder delay, network contribution, decoder delay, and display wait. Include frame drops, recovery stalls, stale intervals, and text readability in the result.

Compare against a well-configured Sunshine/Moonlight baseline and, for applicable platform pairs, native screen-sharing alternatives. Use the same physical displays, refresh rates, content, network impairment, bitrate accounting, and capture method. A low-latency measurement achieved by quietly lowering resolution is not equivalent quality.

Test sustained use and thermal equilibrium. A short burst on a cool phone is not its steady-state decoding envelope.

Add explicit tests for first frame, final frame before idle, refinement latency, loss-induced freezes, recovery bytes, and time spent with input suspended by stale view. Report packet-loss distribution and where impairment is injected: inner datagrams, outer tunnel packets, and TCP/DERP stalls are not interchangeable. Sweep burst loss and recovery-frame size as well as independent loss.

Quality comparisons include readable small colored text, scale correctness, cursor alignment, and color-range/HDR-to-SDR behavior, not only aggregate video metrics. Preserve recordings/screenshots only in an explicitly authorized benchmark lane. Calibration data must state which endpoints share a clock and which measurements are optical versus API/compositor timestamps.

## 22. Workspace and code-size budget

### 22.1 Proposed workspace

```text
frankenremote/
  Cargo.toml
  Cargo.lock
  rust-toolchain.toml
  README.md
  AGENTS.md
  PROTOCOL.md
  SECURITY.md
  COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENREMOTE.md
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
  mobile/
    ios/              native SwiftUI application over the shared Rust core
    android/          native Kotlin/Jetpack Compose application over the shared core
  web/                small self-hosted JS/CSS/HTML shell
  native/             reproducible media build recipes and manifests
  xtask/              repository-owned verification/release commands
```

These are proposed responsibility boundaries, not a requirement to create every crate before the first working slice. Split only where dependency isolation, unsafe isolation, or independent compilation earns the boundary. Platform modules do not each need an ecosystem of subcrates.

### 22.2 Budget

| Work area | Target handwritten Rust lines |
|---|---:|
| Protocol and session authority | 12,000 |
| Asupersync transport integration, including project-induced upstream work | 15,000 |
| Tailscale identity, discovery, and HTTPS | 10,000 |
| Media core and codec wrappers | 16,000 |
| Linux, macOS, and Windows host adapters | 30,000 |
| Input, clipboard, and audio (both directions, including virtual-mic endpoint integration) | 22,000 |
| File transfer and synchronization over ATP | 8,000 |
| Desktop client shells and presentation | 16,000 |
| Mobile client bridges and lifecycle | 12,000 |
| Browser WASM client and JS boundary support | 10,000 |
| CLI, diagnostics, packaging, and update logic | 10,000 |
| Tests, fuzzing, simulation, and benchmark harnesses | 33,000 |
| **Target total** | **194,000** |
| **Contingency, including optional precision experiments** | **46,000** |
| **Planned maximum** | **240,000** |

The budget includes handwritten test code and incremental work moved upstream specifically to enable this project. Moving code into Asupersync is good reuse, but it is not permission to hide the implementation effort.

Existing upstream code, generated SDK bindings, FFmpeg, system libraries, the Windows virtual-audio component, and assets are not counted as newly handwritten FrankenRemote Rust. Report their sizes, licenses, and trust boundaries separately. Establish a separate allowance, initially at most 20k handwritten lines, for JS, Swift, Kotlin, and build glue — raised from 15k in version 1.4 because the iOS and Android applications deliberately carry first-class native user interfaces. Presentation, navigation, and OS lifecycle code belongs in that allowance; do not move protocol, session, media, or input logic there to evade the Rust limit.

These are planning allocations, not an estimate derived from an implemented prototype. Review the actual slope at each milestone. If core delivery approaches 240k before qualification, remove optional scope or simplify abstractions. Do not redefine the counting method near the end.

The corrections in version 1.1 are requirements within these allocations, not an additional parallel architecture. Spend the existing transport allowance on one proven live path and bounded recovery, the input allowance on one explicit event/expiry model, and the platform allowance on qualified adapters. Defer window-scoped sharing, native multitouch, custom display drivers, chroma-carrier work, and broader architecture lanes before adding another subsystem. Do not implement both new infrastructure and a temporary shipping substitute indefinitely.

Use one fixed counting command in the repository and report handwritten Rust, generated bindings, non-Rust glue, vendored source, and project-induced upstream changes separately. The 240k planned maximum leaves margin below the user's strict 250k limit; optional features never consume the final margin before core security and qualification tests are complete.

## 23. Implementation sequence and release gates

### Phase 0 — Retire the architectural risks

Work in small real vertical experiments, not a large scaffold. Every experiment produces a reproducible command, exact hardware/software identity, result, and retained failure reason.

| Gate | Required experiment | What failure means |
|---|---|---|
| Native media | Actual capture -> hardware HEVC -> hardware decode -> presentation on Apple Silicon and a Windows/Linux GPU path | Revisit surface interop/backend choice before building the GUI around it |
| Native transport | Real Asupersync endpoint, real certificate/ALPN validation, packet protection, stream/DATAGRAM recovery, concurrent clients, independent QUIC peer tests, direct and forced-relay paths | Keep unsafe/unqualified paths disabled; count bounded upstream work and qualify WSS explicitly instead of inventing a second runtime/QUIC stack |
| Browser transport | Real HTTP/3/WebTransport negotiation, datagram-size limits, stream/session binding, origin/nonce checks, cancellation/reconnect; native/browser WSS bounded-credit fallback | A queued adapter or self-loopback is not interoperability; label WSS as degraded and publish the tested H3 draft/profile |
| Browser codec | Exact emitted hvcC/access units at real display geometry; first IDR, one P frame after long idle, last burst frame, recovery, low-cadence presentation | Narrow the qualified support matrix; do not silently add another codec or rely on per-frame flush |
| Tailnet identity | Real local interfaces/CLI variants, IPv4/IPv6 ingress, distinct owners/tags, sharing/routed sources, expired/stale identity, local policy changes | Prove membership and ingress before unattended hosting; missing evidence is not permission to infer it from reachability or DNS |
| OS lifecycle | Independent capture/input/clipboard/audio grants; Wayland restore-token rotation and PipeWire mapping, macOS signed-helper attribution, Windows user-session handoff/loopback scope, lock/suspend | Publish exact capability rows; capture success, root status, or service registration is not a permission proof |
| Native packaging | Reproducible stripped codecs plus native shell/surface interop on Linux/Windows; signed Apple system-media shell; permission/ABI/license checks | Choose one wrapper/shell strategy and correct packaging before multiplying adapters |
| Recovery and authority | Fragmented HEVC with loss/reordering, startup handshake, bounded cache, reference-versus-presentation expiry, input queued through a long stall, local revoke | No claimed low-latency or safe-control operating point until these real paths pass |
| Host virtual-mic endpoint | Create a selectable host input device per OS — PipeWire virtual source, signed user-space CoreAudio server plugin, Windows virtual audio endpoint driver — route the Opus uplink into a real host application, and measure latency; validate the signing/packaging path | Microphone forwarding ships as typed-unsupported on the failing host OS; no undisclosed third-party driver and no fake device |

The browser and OS risks are tested here even though complete clients are delivered later. They must not be discovered after most of the product has been written.

Record blocking dependencies as specific missing APIs/tests, not a broad “supported by Asupersync” assumption. Limit each spike to the smallest reproducible path and select a shipping composition from evidence. No outcome in this document is marked passed merely because source comments describe an intended implementation.

### Phase 1 — One complete controlled desktop

Build one native host/client pair end to end: identity, permission, capture, HEVC, transport, display, pointer, keyboard, revoke, and reconnect. Use the proposed generation model from the beginning.

Select the first pair from the successful Phase 0 experiments, preferably with an Apple Silicon path and a Windows/Linux hardware path kept close enough to prevent accidental single-platform assumptions. The first supported host does not need every optional quality mode.

**Exit gate:** an actual interactive session survives worker restart, network interruption, client focus loss, stale-view/input-ticket expiry, and display resize without stale authority or a growing queue. Bootstrap/recovery, modest real packet loss, basic rate control, optional approval of observation, and origin protections for any exposed browser ingress work already. An instrumented latency result exists. A mock codec does not satisfy this gate; correctness-critical recovery is not postponed to Phase 4.

### Phase 2 — All host platforms and native desktop clients

Complete the three host adapters, service/session lifecycle, native desktop presentation, audio in both directions where the host endpoint qualifies, the default-on shared clipboard, explicit file send/receive over ATP, display selection, and permission diagnostics. Add bounded viewer admission and controller handoff only after single-controller behavior is correct.

Native GUI event loops and OS callbacks integrate through the common Asupersync-owned session machinery; they are not an excuse to introduce another Rust async runtime. Platform-specific callback and thread-affinity boundaries remain explicit.

**Exit gate:** the common session/input fault suite passes on each qualified OS, with actual device evidence and packaging smoke tests.

### Phase 3 — Browser and mobile clients

Finish the thin browser shell and shared WASM state machine, hardware decode probes, secure-origin defenses, WSS fallback, iOS/Android shells, touch/keyboard interaction, and lifecycle behavior. Qualify Asupersync's selected browser/mobile features rather than relying on preview labels.

**Exit gate:** real browser and phone clients connect through their own tailnet connectivity; background/resume, rotation, decoder pressure, and network changes cannot preserve stale control. The capability matrix accurately separates supported, degraded, and unsupported combinations.

### Phase 4 — Quality, adaptation, and recovery

Tune the already functioning deterministic controller, idle behavior, settle-to-sharp refinement, cursor separation, text/motion policies, loss recovery, and telemetry, and complete the ATP folder-synchronization jobs on the already working transfer channel. Phase 4 improves measured quality and performance; it is not where basic deadline, congestion, or reference-chain correctness is first implemented. Optimize observed copies and queues before advanced codec tools.

**Exit gate:** the defined network scenarios show stable operating points, bounded application queueing, and measured text/latency tradeoffs. Controller decisions replay deterministically from sanitized traces.

### Phase 5 — Release qualification

Run adversarial parsing, origin attacks, tailnet-sharing tests, long-duration/thermal tests, installer/update/rollback tests, and actual hardware comparisons. Publish the supported matrix and unresolved restrictions.

**Exit gate:** signed installation artifacts reproduce the tested behavior; unsupported states fail specifically; no performance claim outruns the retained evidence.

### Optional extension lane

Only after the core is useful: native 4:4:4 precision, the HEVC chroma-carrier experiment, selective small-group FEC, and authorized FrankenTerm semantics. Each has a bounded budget and an independent off switch. None is allowed to hold the baseline architecture hostage.

## 24. Verification and acceptance matrix

### 24.1 Deterministic and protocol tests

Use Asupersync's lab runtime for loss, duplication, reordering, variable delay, bounded queues, timeout races, lease expiry, and cancellation. Test the actual state machines and limits, not just generated event traces that never exercise production logic.

Required properties include: a duplicate input identifier cannot admit another click within its live epoch; an old lease never controls a new session; geometry changes reject old coordinate input; an obsolete codec configuration never reaches the new decoder; and missing reference pictures trigger repair or recovery rather than indefinite corrupted output.

For channel cancellation, distinguish uncommitted work from committed sends. For OS injection, distinguish admitted work from irreversible external submission. Teardown must not silently discard the latter from its result.

Add model properties for host-clock challenge expiry, ticket expiry immediately before injection, no old action resurrection after a stalled reactor, read-only approval enforcement, controller handoff serialization, shared-pipeline lifetime, late viewer join, and old-pointer datagrams after a click. Distinguish local client observation freshness from old pixel timestamps on a genuinely unchanged screen.

The recovery model includes missing first/middle/final fragments, loss immediately before idle, reference repair after its own display deadline, lost recovery configuration/acknowledgement, exhausted recovery budget, a 120-ms repair horizon, large IDRs, and receiver-credit starvation. Assert bounded bytes and metadata, not only queue length. Exercise the production parsers/state machines with deterministic transport adapters; real TLS/codec interop remains a separate native/browser requirement.

### 24.2 Native fault tests

Kill or stall the media worker during capture, encode, configuration change, and shutdown. Disconnect Tailscale while modifiers are held. Crash the client, suspend a browser tab, background a phone, remove a display during a drag, change scale during a click, and switch local user sessions during controller handoff.

Test codec-device exhaustion while another application uses the GPU. Verify that the system degrades or refuses without consuming unbounded CPU through an unnoticed software fallback.

Exercise sleep/wake, certificate renewal, tailnet switching, permission revocation, audio-device changes, and expired/unknown peer metadata.

Stall the input worker/event loop independently of the encoder and then deliver queued presses after expiry. Kill the first viewer of a shared encoder, disconnect a slow secondary viewer, and join a third viewer during recovery. Test partial OS input submission, local/synthetic modifier collisions, relative-motion checkpoints, and IME cancellation on reconnect.

Rotate a Wayland restore token, reuse a numeric PipeWire node ID, revoke clipboard while video continues, and check endpoint-wide Windows audio with more than one user session. Remove/reinsert capture devices and GPUs, use mixed-DPI/rotated/negative-origin displays, and verify no stale or uninitialized surface pixels leak after resize. These are required qualified-adapter behaviors, not assumptions inferred from a happy-path demo.

### 24.3 Security tests

Attempt control from an unrelated LAN source, a forged `100.x` address on the wrong interface, an external sharee, a shared-in node, a subnet-routed source, and a malicious web origin running on an otherwise admitted client.

Attempt forged Serve headers, unauthorized local IPC approval, stale-generation worker messages, oversized media configuration, excessive fragment counts, duplicate-fragment memory inflation, recovery-request floods, clipboard echo loops, file-transfer path traversal and symlink escape, transfer-budget exhaustion floods, and microphone activation without the explicit client enable.

Fuzz the wire parser, HEVC configuration normalization, geometry arithmetic, input decoder, and local IPC framing. Use independent decoder interoperability tests for generated streams. Media-worker containment supplements parser bounds; it does not replace them.

Add invalid/expired host certificates, wrong hostname/ALPN, tampered QUIC packets, 0-RTT application operations, alternate-address migration, pre-admission media, replayed attachment tickets, and origin-less native-style WSS bypass attempts. Local approval must block thumbnails, audio, clipboard, and semantic reads as well as control.

Test forged host/origin routing, malicious iframes, back/forward-cache state, request smuggling/header ambiguity in the selected HTTP stack, mixed helper generations, DLL/library search-path injection, and unsigned/path-traversing update artifacts. Add hostile but valid-looking HEVC parameter sets, high DPB demands, changing codec configuration inside ordinary media, and audio packets declaring excessive decoded duration. Keep the selected OS/GPU trust limits explicit when a containment guarantee cannot be enforced.

### 24.4 Hardware and network matrix

Cover Apple Silicon, representative NVIDIA, AMD, and Intel paths, plus low-power mobile decoders. Record the specific GPU, driver, OS build, encoder path, decoder path, pixel format, and effective settings. Support belongs to qualified combinations, not a vendor logo.

At minimum test a healthy direct link, approximately 40-ms RTT, approximately 120-ms RTT, random loss, burst loss, variable jitter, abrupt capacity reduction, and a relayed path. Include text editing, terminal scrolling, thin colored lines, video motion, and mixed workloads.

Browser qualification records the exact browser, OS, codec probe, transport mode, decode path evidence, and presentation behavior. “WebCodecs exists” is not an acceptance test.

Also record the Tailscale build/installation variant, host architecture, compositor/portal versions, selected audio source and scope, client renderer, physical/logical coordinate mapping, and actual API copy path. Include bandwidth step changes immediately after idle and direct-to-relay transitions during a drag. WSS is measured independently, including receive-credit stalls and underlying-connection sharing, not scored using native-datagram assumptions.

### 24.5 Release evidence

One compact capability/result matrix and retained command outputs are sufficient. Do not create a bureaucracy of evidence services. A row is `passed`, `failed`, `blocked`, or `not tested`; an untested row is not “supported with caveats.”

Maintain a narrow list of known restrictions in the README and `fr doctor`. The tool should make the state legible to both users and agents.

Source reviewed, builds passed, simulated properties passed, independent wire interoperability passed, and hardware measurements passed are separate evidence categories. A source-level plan review, including this one, establishes none of the latter four. Tests added to the plan are requirements to execute during implementation, not tests already run.

## 25. Risks, bounded open decisions, and rejected scope

| Risk or open decision | Resolution policy |
|---|---|
| HEVC missing or slow in some browsers | Feature probe plus real decode/present test. Publish the limit. No secret second video codec. Chrome and Safari on supported hardware are the qualification bar; other browsers' gaps are accepted. |
| Asupersync native or WebTransport composition is not production-qualified | Test actual endpoints, TLS/ALPN, independent peers, congestion and bounds. Upstream only bounded missing work; WSS is a labeled fallback, not datagram parity. |
| Asupersync platform/runtime gaps exceed the integration allowance | Revisit schedule/scope before expanding application code. Count project-induced upstream work against the budget. |
| GPU capture-to-encode copies erase the hardware advantage | Measure copies and latency before GUI expansion; change backend or surface path. |
| Wayland unattended access differs across compositors | Explicit capability matrix and portal restoration tests; no permission bypass. |
| macOS/Windows secure desktop expectations exceed scope | State the existing-interactive-session boundary in setup and documentation. |
| Same-tailnet membership or ingress cannot be proven from a selected local API | No DNS/source-prefix/zero-sharer inference. Qualify installation variants and sharing/tagged devices; use an explicit tailnet grant profile only by local configuration. |
| Tailscale HTTPS not enabled | Setup reports the prerequisite and certificate visibility; do not weaken browser TLS silently. |
| FFmpeg build or license scope expands | Curated native bundle, exact configuration, single binding family, explicit distribution review. |
| Pure-Rust HEVC candidate is not yet fast/complete enough | Keep it out of the critical path; evaluate only as a bounded optional fallback. |
| 4:2:0 text color falls short of workstation expectations | Native pixels, color correctness, settle-to-sharp, and separately qualified precision extensions. |
| Multi-viewer workloads exhaust sessions or disturb other viewers | Shared-pipeline ownership, coalesced recovery, per-viewer byte limits, resource admission, independent degradation. |
| Loss/recovery becomes an IDR storm or stalls the dependency chain | Separate reference/display deadlines, bounded repair/reliable bootstrap, actual fragmented-media tests; lower operating point before expanding complexity. |
| Reliable input executes long after it is safe | Host-issued expiring tickets, independent lease watchdog, submission-time checks, no automatic replay. |
| Read-only viewing or audio bypasses local approval/scope | Gate all observation; disclose endpoint-wide audio and wider GUI control authority; do not claim window isolation. |
| Tiny idle/latency targets rely on hidden surfaces or stale timestamps | Account for the process family and GPU memory; separate cold/warm startup, source verification, decode, and actual presentation. |
| Complexity grows through “helpful” integrations | Apply the 194k target and 240k planned ceiling at each milestone; cut optional features first. |

The implementation should reject, unless a later explicit scope change justifies them: a new HEVC encoder, a full WebRTC stack, a custom VPN, public relay infrastructure, federated session management, a database-backed fleet service, mandatory RaptorQ on every frame, multiple video codec families, arbitrary FFmpeg filter graphs, and prediction that fabricates application state.

The native GUI/windowing binding and exact FFmpeg wrapper revision remain bounded implementation choices. Choose them through the early surface-interop/build experiments, write a short decision note, and proceed. Do not create open-ended adapter competitions.

## 26. Definition of done

FrankenRemote version 1 is done when a user can install a small process family on a supported Linux, macOS, or Windows host already on Tailscale; grant the required OS permissions; and use its existing desktop from qualified native desktop, mobile, and browser clients without a separate FrankenRemote account or pairing procedure.

The experience must remain honest under failure: unsupported HEVC fails a capability check; a stale screen is identified; an old input lease cannot survive reconnect; a failed media worker cannot retain authority; OS permission limits are stated; and relayed or TCP compatibility modes are not presented as equivalent to a healthy direct path.

It must have native hardware evidence for the advertised operating points, bounded resource behavior, explicit license/native-build provenance, an installer and rollback path, stable machine-readable diagnostics, and a codebase below the declared budget.

The final synthesis is intentionally small:

> **Tailscale decides which machines can reach and identify one another. Asupersync makes their session lifetimes tractable. Existing hardware APIs move and compress pixels. FrankenRemote makes the resulting interaction fresh, readable, safe to stop, and easy to understand.**

That is enough of a project to be valuable, and enough of a constraint to make it deliverable.

Version 1.1 adds no hidden completion exceptions: queued-but-expired input must not execute, approval cannot be bypassed through observation, shared encoders must outlive individual subscribers correctly, and recovery must work with fragmented real HEVC under the advertised loss envelope. Browser idle/flush behavior, identity proof, endpoint-wide audio scope, and signed helper lifecycles must have explicit qualification evidence before the corresponding support claims ship. A transport adapter, a positive codec probe, or a finite application queue alone does not satisfy those requirements.

## 27. Research provenance and references

This plan was informed by targeted source/document inspection. For version 1.1 the entire 27-section document was reread and revised in place, with deeper Asupersync endpoint inspection and re-verification of transport, codec, Tailscale, and platform contracts. The review did not compile the repositories, execute native hardware benchmarks, test a live FrankenRemote session, or validate release artifacts. Performance objectives, line budgets, adapter choices, and protocol limits are proposals. Repository README claims and candidate codec test reports are not independent benchmark results.

The Asupersync snapshot inspected was `bf6b361deb3154c56d1450ea679e6d4a3cbf09b9`. FrankenTerm search results were pinned to `9862064119a4c652d32cd944f40b6b4d4f99b25c`; the README and robot/integration material, rather than its entire implementation, were inspected. The FrankenGit plan was used as a structural example, not audited in full. These deliberately limited scopes identify reuse candidates, not certify them. The new review additionally inspected `src/net/mod.rs`, the opening implementation/API sections of `quic_native/connection.rs`, `endpoint_api.rs`, and `managed_endpoint.rs`, and the current branch metadata through GitHub; the returned branch still pointed to the same Asupersync commit. It did not audit all code in those large modules or all of FrankenTerm. The OxideAV README status/encoder portions were rechecked through GitHub (returned README blob `faba3f6d7bfcdbef9da12901bc52fd62374b9121`); its reported conformance/performance tests were not executed during this review.

### 27.1 Integrated review record

| Area corrected | Consequence for implementation |
|---|---|
| Admission and observation | Optional approval gates every sensitive view/audio/semantic path; broad same-tailnet control is explicitly enabled locally |
| Identity and platform LocalAPI | Same-tailnet membership and actual ingress need positive tested evidence; installation variants and source-preserving transport matter |
| Session ownership | Shared pipelines belong to the OS share session; individual viewer closure cannot cancel another viewer's encoder |
| Authority and input | Host-clock expiry at submission, challenge/ticket anti-resurrection, ordered coordinate barriers, explicit partial effects |
| Codec startup and recovery | Configuration acknowledgement is separate from first decode; display deadlines do not erase reference usefulness |
| Loss and buffering | Account for fragmented-picture loss, reliable recovery cost, repair windows, receiver credits, global bytes and surface pools |
| Runtime and WebTransport | State-machine aliases and adapter queues are not deployment proof; qualify actual endpoint/TLS/H3 compositions |
| Media FFI | Send/receive state, padded buffers, borrowed surfaces, fences, thread affinity, device loss, and sandbox limits are explicit |
| OS integration | Portal token/stream lifecycle, clipboard order, Windows audio scope, signed helpers, protected content, HDR-to-SDR qualification |
| Browser | Correct hvcC/access units, flush/keyframe behavior, input/origin bootstrap, output-frame lifetime, hidden-tab and cache lifecycle |
| Scope and budgets | Full-display baseline; defer isolated-window claims and optional precision; retain 194k/240k Rust allocations |
| Evidence | New tests are future implementation gates, not represented as performed; source review is not hardware or wire qualification |

### 27.2 References

References below support external facts. Architecture, algorithms, numerical targets, limits, and work allocations elsewhere in the document are proposed FrankenRemote decisions unless explicitly described as an inspected source finding. Mutable documentation was consulted during this review; an implementation must pin its chosen dependency/specification versions and repeat capability checks rather than treating this reference list as permanent certification.

| Reference | Source and relevance |
|---|---|
| [S1] | Asupersync README at the inspected commit: structured concurrency and cancellation boundaries |
| [S2] | Asupersync networking exports at the inspected commit: native QUIC alias and transport surfaces |
| [S3] | Asupersync manifest at the inspected commit: features, package version, license, browser preview, TLS dependencies |
| [S4] | Asupersync ATP WebTransport adapter: state/configuration and outbound queues |
| [S5] | Asupersync native ATP/H3 adapter: compatibility boundaries |
| [S6] | FrankenTerm README and robot interface at the inspected snapshot |
| [S7] | FrankenGit comprehensive plan: example document structure |
| [S8] | NVIDIA Video Codec SDK encoder programming guide: capability queries and low-latency configuration |
| [S9] | FFmpeg legal guidance: configuration-dependent licensing and distribution considerations |
| [S10] | `ffmpeg-next` binding project |
| [S11] | `ffmpeg-the-third` binding project |
| [S12] | x265 introduction: implementation and licensing boundary |
| [S13] | OxideAV HEVC project: current encoder/decoder claims and documented limitations |
| [S14] | W3C WebCodecs specification: codec support and decoding API |
| [S15] | W3C HEVC WebCodecs registration: codec strings, configuration, and access-unit forms |
| [S16] | W3C WebTransport specification: browser transport model |
| [S17] | Tailscale local status types: peer identity, sharing, current tailnet, and node addresses |
| [S18] | Tailscale WhoIs response types |
| [S19] | Tailscale HTTPS certificate setup and renewal considerations |
| [S20] | Tailscale Serve documentation: ingress and identity-header constraints |
| [S21] | Tailscale connection types: direct and relayed paths |
| [S22] | Tailscale application capabilities |
| [S23] | Apple ScreenCaptureKit introduction |
| [S24] | Apple low-latency VideoToolbox session: do not generalize its H.264-specific mode to HEVC |
| [S25] | Microsoft Desktop Duplication API |
| [S26] | Microsoft SendInput API and integrity restrictions |
| [S27] | XDG RemoteDesktop portal: session, permission, and persistence behavior |
| [S28] | Android MediaCodec API |
| [S29] | Opus license and implementation licensing |
| [S30] | Sunshine documentation: relevant existing hardware-streaming baseline |
| [S31] | Tailscale node metadata: addresses, sharer information, and node identity |
| [S32] | Asupersync native connection: runtime-agnostic state machine, not socket I/O |
| [S33] | Asupersync high-level endpoint: live versus lab transport and explicit qualification boundaries |
| [S34] | Asupersync managed endpoint: routing, timers, and authenticated accept machinery |
| [S35] | RFC 9000: QUIC transport, packet-size and protocol requirements |
| [S36] | RFC 9221: QUIC DATAGRAM semantics, limits, and congestion control |
| [S37] | RFC 9297: HTTP Datagrams and the Capsule Protocol |
| [S38] | WebTransport over HTTP/3 draft: negotiated sessions, streams and datagrams; qualify the actual revision |
| [S39] | Tailscale network troubleshooting: documented MTU and packet-size consequences |
| [S40] | Tailscale macOS variants: installation-specific integration boundaries |
| [S41] | Tailscale LocalAPI package: local transport and authorization mechanisms |
| [S42] | XDG Clipboard portal: parent-session grants, pre-Start request, and transfer lifecycle |
| [S43] | XDG ScreenCast portal: restore-token rotation, PipeWire identity and coordinate/cursor metadata |
| [S44] | Microsoft DuplicateOutput1: capture formats, adapter and lifecycle constraints |
| [S45] | FFmpeg send/receive API: ownership, EAGAIN, progress, and draining semantics |
| [S46] | Microsoft WASAPI loopback: render-endpoint capture and cross-session audio scope |
| [S47] | WHATWG WebSockets: buffering, close and browser API semantics |
| [S48] | FFmpeg decoding API: compressed-input padding and decoder ownership contracts |
| [S49] | Linux DMA-BUF: shared-buffer ownership, fences, and synchronization |
| [S50] | Tailscale policy syntax: membership, tagged and shared principal distinctions |

[S1]: https://github.com/Dicklesworthstone/asupersync/blob/bf6b361deb3154c56d1450ea679e6d4a3cbf09b9/README.md
[S2]: https://github.com/Dicklesworthstone/asupersync/blob/bf6b361deb3154c56d1450ea679e6d4a3cbf09b9/src/net/mod.rs
[S3]: https://github.com/Dicklesworthstone/asupersync/blob/bf6b361deb3154c56d1450ea679e6d4a3cbf09b9/Cargo.toml
[S4]: https://github.com/Dicklesworthstone/asupersync/blob/bf6b361deb3154c56d1450ea679e6d4a3cbf09b9/src/atp/adapter/webtransport.rs
[S5]: https://github.com/Dicklesworthstone/asupersync/blob/bf6b361deb3154c56d1450ea679e6d4a3cbf09b9/src/net/atp/h3/adapter.rs
[S6]: https://github.com/Dicklesworthstone/frankenterm/blob/9862064119a4c652d32cd944f40b6b4d4f99b25c/README.md
[S7]: https://github.com/Dicklesworthstone/frankengit/blob/main/COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGIT.md
[S8]: https://docs.nvidia.com/video-technologies/video-codec-sdk/13.0/nvenc-video-encoder-api-prog-guide/index.html
[S9]: https://ffmpeg.org/legal.html
[S10]: https://github.com/zmwangx/rust-ffmpeg
[S11]: https://github.com/shssoichiro/ffmpeg-the-third
[S12]: https://x265.readthedocs.io/en/master/introduction.html
[S13]: https://github.com/OxideAV/oxideav-h265
[S14]: https://www.w3.org/TR/webcodecs/
[S15]: https://w3c.github.io/webcodecs/hevc_codec_registration.html
[S16]: https://www.w3.org/TR/webtransport/
[S17]: https://pkg.go.dev/tailscale.com/ipn/ipnstate
[S18]: https://pkg.go.dev/tailscale.com/client/tailscale/apitype
[S19]: https://tailscale.com/docs/how-to/set-up-https-certificates
[S20]: https://tailscale.com/docs/features/tailscale-serve
[S21]: https://tailscale.com/docs/reference/connection-types
[S22]: https://tailscale.com/docs/features/access-control/grants/grants-app-capabilities
[S23]: https://developer.apple.com/videos/play/wwdc2022/10155/
[S24]: https://developer.apple.com/videos/play/wwdc2021/10158/
[S25]: https://learn.microsoft.com/en-us/windows/win32/direct3ddxgi/desktop-dup-api
[S26]: https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-sendinput
[S27]: https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.RemoteDesktop.html
[S28]: https://developer.android.com/reference/android/media/MediaCodec
[S29]: https://opus-codec.org/license/
[S30]: https://docs.lizardbyte.dev/projects/sunshine/latest/md_docs_2getting__started.html
[S31]: https://pkg.go.dev/tailscale.com/tailcfg

[S32]: https://github.com/Dicklesworthstone/asupersync/blob/bf6b361deb3154c56d1450ea679e6d4a3cbf09b9/src/net/quic_native/connection.rs
[S33]: https://github.com/Dicklesworthstone/asupersync/blob/bf6b361deb3154c56d1450ea679e6d4a3cbf09b9/src/net/quic_native/endpoint_api.rs
[S34]: https://github.com/Dicklesworthstone/asupersync/blob/bf6b361deb3154c56d1450ea679e6d4a3cbf09b9/src/net/quic_native/managed_endpoint.rs
[S35]: https://www.rfc-editor.org/rfc/rfc9000.html
[S36]: https://www.rfc-editor.org/rfc/rfc9221.html
[S37]: https://www.rfc-editor.org/rfc/rfc9297.html
[S38]: https://datatracker.ietf.org/doc/html/draft-ietf-webtrans-http3-16
[S39]: https://tailscale.com/docs/reference/troubleshooting/network-configuration/tcp-connection-two-devices
[S40]: https://tailscale.com/docs/concepts/macos-variants
[S41]: https://pkg.go.dev/tailscale.com/ipn/localapi
[S42]: https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.Clipboard.html
[S43]: https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.ScreenCast.html
[S44]: https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_5/nf-dxgi1_5-idxgioutput5-duplicateoutput1
[S45]: https://ffmpeg.org/doxygen/trunk/group__lavc__encdec.html
[S46]: https://learn.microsoft.com/en-us/windows/win32/coreaudio/loopback-recording
[S47]: https://websockets.spec.whatwg.org/
[S48]: https://ffmpeg.org/doxygen/trunk/group__lavc__decoding.html
[S49]: https://docs.kernel.org/driver-api/dma-buf.html
[S50]: https://tailscale.com/docs/reference/syntax/policy-file

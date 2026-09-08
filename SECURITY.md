# Security Policy

FrankenRemote is a spec-first project in early implementation, with initial core and media-contract crates but no working remote workstation. Security reports may concern defects in the existing code, design defects in [`COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENREMOTE.md`](COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENREMOTE.md), or repository verification/release automation.

## Reporting a vulnerability

Use GitHub's private security-advisory flow for this repository. Do not open a public issue containing exploit details, credentials, tailnet metadata from a real deployment, or a working proof of concept against a deployed system.

Include:

- affected document section, commit, component, protocol/format version, or capability row;
- threat actor and required access (unrelated network client, same-tailnet peer, shared-in/external sharee, malicious web origin on an admitted machine, local unprivileged process, admitted controller);
- exact invariant, capability, trust boundary, or authority primitive violated;
- reproduction steps, minimal model, packet/bitstream corpus, or deterministic trace;
- impact scope: observation, control, audio, clipboard, persistence, or resource exhaustion;
- suggested mitigation, if known;
- disclosure constraints.

## Highest-priority areas

Aligned with the plan's threat model (§19) and security test matrix (§24.3):

- tailnet identity and admission: membership/ingress inference weaknesses, shared-in nodes, external sharees, subnet-routed or forged `100.x` sources, stale/expired peer metadata, LocalAPI variant confusion;
- session authority: lease/challenge/ticket expiry, resurrection of stale authority after suspend/stall, generation-fencing gaps, controller-handoff races, approval bypass via read-only observation or auxiliary channels;
- input path: execution of expired or replayed actions, submission-time check omissions, held-key/modifier cleanup, uncertain-effect misreporting;
- wire parsing and limits: fragment reassembly arithmetic, duplicate/overlap handling, metadata-allocation exhaustion, oversized or contradictory configuration;
- HEVC/media boundary: hostile parameter sets, DPB/geometry demands, in-band configuration changes, decoder containment, FFmpeg wrapper ownership and padding contracts, uninitialized surface/padding pixel leaks;
- browser ingress: Origin/nonce/ticket bootstrap, DNS rebinding and Host tricks, cross-origin sockets, back/forward-cache and service-worker state, CSP gaps;
- HTTPS/QUIC: certificate/hostname/ALPN validation, 0-RTT admission, downgrade to WSS with weakened semantics, Tailscale Serve identity-header forgery;
- local IPC: role-capability forgery, worker substitution, approval-endpoint reachability from unauthorized process roles, named-pipe/socket session binding;
- privileged helpers, installer, and updates: privilege containment of the platform broker, signed process-family integrity, mixed-generation activation, library search-path injection, archive path traversal, forced rollback;
- file transfer and synchronization: path traversal and symlink escape on received paths, non-atomic publication of partial transfers, budget-exhaustion floods, sync-job conflict handling that silently overwrites;
- bidirectional audio: microphone activation without the explicit client enable, virtual-microphone endpoint integrity and signing (including the Windows driver-class component), endpoint-scope misrepresentation;
- denial of service: pre-admission floods, codec-probe reservation, recovery-request storms, half-attached channels, per-viewer and global byte budgets;
- privacy: screen/clipboard/input/audio content reaching logs, traces, discovery responses, or diagnostic exports; endpoint-wide audio scope misrepresentation.

## Non-claims

No production support window or security SLA exists before an implementation release. Nodes inside the locally selected sharing scope (the host user's own devices by default; the whole tailnet only by explicit local choice) are deliberately trusted with desktop control; a compromised in-scope device has that authority until revoked. The selected desktop user, host OS, installed Tailscale authority, and GPU/media stack are explicit trust limits. Process separation is crash isolation, not a sandbox, unless a specific enforced OS sandbox is documented for that target.

# Security Policy

FrankenRemote is an early implementation with tested Rust core policy and media contracts, not an installable remote desktop release. Security reports may concern current code, design defects in [`COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENREMOTE.md`](COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENREMOTE.md), or repository verification/release automation. [`IMPLEMENTATION_STATUS.md`](IMPLEMENTATION_STATUS.md) distinguishes implemented behavior from unqualified platform and transport boundaries.

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

## Privileged ingress helper and residual trust

Per plan §5.2 and §19.2 and [`LINUX_NATIVE_INGRESS.md`](LINUX_NATIVE_INGRESS.md):

- **Least-privilege split, not a sandbox.** An unprivileged `frd run` (for example
  a systemd user unit) does not administer nftables. The root `frd ingress-helper`
  does, and its entire API is `install`, `renew` and `remove` of one drop-only,
  exact-destination rule per connection. The rule covers the configured tailnet
  interface, an address the kernel reports on it, a nonzero port and a protocol
  set within {udp, tcp}. No caller-supplied nft text, table name or interface index is
  accepted. Frames are fixed binary and bounded. There is one request in flight
  per connection, at most 8 connections, and rate limits on accepts, requests and
  refusal logging.
- **Local IPC forgery.** Every connection is checked with `SO_PEERCRED` against
  `allowed_uids` in a root-owned configuration, never against anything the
  caller sends. The broker requires a root-only socket path and a uid 0 peer.
  Generations fence renew/remove per connection. A rule lives exactly as long as
  its connection, which the broker's leases hold, so broker exit (including
  SIGKILL) removes it. A dead helper's `frdh_` tables are reclaimed at its next
  start; no other table is touched.
- **Residual trust, stated.** The helper, the kernel, root-owned `nft`/`ip`, the
  configured TUN and the root configuration are trusted. The unprivileged broker
  cannot read the kernel ruleset. It validates the **helper's** read-back with
  the same `validate_rule` as the direct path, which is trust in the helper
  rather than independent kernel evidence. It independently re-reads the TUN
  index, the address assignment and LocalAPI identity. Any process running as an admitted uid
  can request drop-only rules (denying non-tailnet traffic to a port on a tailnet
  address); it cannot accept traffic or touch another connection's rule. A
  helper crash or restart removes protection from a live broker until that
  broker's next renewal (at most about 500 ms), the same periodic limit as the
  direct path. Qualified with real nftables only in a private network namespace;
  a live tailnet host and the emitted systemd unit are not yet qualified.

## Media-worker sandboxing and residual trust

Per plan §5.4, §19.1, §19.2, and [`WORKER_SANDBOX.md`](WORKER_SANDBOX.md):

- **Architectural capability isolation**: Media workers (capture/encode, decode/presentation) receive only borrowed frame buffers and encoded NAL streams across private pipes. Workers never receive Tailscale control sockets, TLS certificate private keys, the local approval IPC endpoint, or input lease capabilities. Input authority is verified independently by the broker and input agent immediately prior to OS submission.
- **Enforced sandbox boundaries**: Where supported by the OS and compatible with the media pipeline role, workers operate under kernel-enforced privilege reduction:
  - *Linux software decoder*: Strictly confined using kernel seccomp BPF (`SECCOMP_SET_MODE_FILTER` with `PR_SET_NO_NEW_PRIVS`). Network sockets (`connect`/`bind`), filesystem access (`open`/`creat`), process creation (`fork`/`clone`/`execve`), and approval IPC are blocked with `EPERM`, verified by automated escape attempt tests in `crates/fr-native/tests/decoder_sandbox_escape.rs`.
  - *Linux host capture/encode worker*: **no kernel sandbox**. It runs as a separate same-user process with a cleared environment (only `DISPLAY`/`XAUTHORITY`) and its two pipes; process separation is crash/hang containment only.
  - *Windows and macOS workers*: **not implemented**. (Earlier text claiming enforced restricted tokens/Job Objects and `sandbox_init` profiles was withdrawn on 2026-09-24: no such code exists.)
- **Explicit residual trust disclosure**: Where hardware acceleration (VA-API, NVENC, Direct3D11/DXGI, VideoToolbox) is used, GPU vendor user-mode drivers and kernel DRM/device nodes require ioctls that cannot be fully filtered without breaking hardware acceleration. A compromised same-user unsandboxed or partially sandboxed GPU worker is **never claimed to be harmless**. The host OS, X11/Wayland display server, and GPU driver stack remain explicit trust boundaries.


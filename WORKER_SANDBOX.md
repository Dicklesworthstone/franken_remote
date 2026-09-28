# Worker Sandboxing and Privilege Reduction

This document records the per-OS sandboxing, privilege reduction, and residual trust model for FrankenRemote media workers, fulfilling the requirements of plan §5.4, §19.1, and §19.2, and bead `fr-p2-worker-sandbox-y3x`.

---

## 1. Threat Model & Principles

Media workers run foreign, complex, and potentially memory-unsafe code (e.g., FFmpeg libraries, OS windowing adapters, GPU vendor runtime drivers). Host encoders consume local frame captures, and client decoders ingest potentially hostile network-delivered bitstreams (HEVC) from remote hosts.

Under plan §5.4 and §19.1:
1. **Process separation is crash/hang isolation by default**, not a security sandbox.
2. **Never claim isolation that is not enforced.** Where OS facilities cannot restrict a worker without breaking essential driver/compositor capabilities, the residual trust must be explicitly stated.
3. **Role-Capability Isolation by Broker:** Media workers are never granted Tailscale control sockets, TLS private certificate keys, the local approval IPC endpoint, or input authority leases. Input authority is held and verified exclusively on the authority broker thread and input agent immediately before OS submission.
4. **Enforced OS Privilege Reduction:** Only the Linux (x86_64) client decoder/presenter worker runs under a kernel-enforced sandbox (seccomp-BPF). The Linux host capture/encode worker has no kernel sandbox. Windows and macOS workers are not implemented.

> **Corrected 2026-09-24.** Earlier versions of this document marked Windows restricted tokens/Job Objects and a macOS `sandbox_init` profile as *Enforced* and cited Landlock for the Linux capture worker. None of that code exists (no `CreateRestrictedToken`, `SetInformationJobObject`, `sandbox_init` or Landlock anywhere in the tree). Those rows now say what exists.

---

## 2. Per-OS Sandbox Capability Table

| Operating System | Worker Role & Pipeline | Enforced Restrictions | Partial / Unenforced Restrictions | Kernel Facility & Implementation | Reasons & Residual Trust |
| :--- | :--- | :--- | :--- | :--- | :--- |
| **Linux (x86_64)** | Client Decoder / Presenter (Software HEVC + X11) | **Enforced**: Network sockets (`socket`, `connect`, `bind` blocked with `EPERM`); Filesystem open/read/write (`open`, `openat`, `creat` blocked with `EPERM`); Process spawning (`fork`, `clone`, `clone3`, `execve`, `execveat` blocked); Approval IPC (`/tmp/frd-approval.sock` blocked). Only pre-opened borrowed descriptors (worker pipe + X11 display socket) permitted. | None for software presentation. | Linux Seccomp BPF (`SECCOMP_SET_MODE_FILTER` with `PR_SET_NO_NEW_PRIVS`, configured in `crates/fr-native/src/decoder_sandbox.c`). | Tested by `tests/decoder_sandbox_escape.rs`. Residual trust: X11 server connection allows X protocol requests to the borrowed window/canvas; X server is an explicit trust boundary. |
| **Linux (x86_64)** | Host Capture / Encoder (X11 XShm over a sealed memfd, `XGetImage` fallback, + software x265) | **None** (no kernel sandbox). | **Unenforced**: network, filesystem and process creation are all available to the same-user worker; no seccomp or Landlock filter is applied. | Process boundary only. The launcher (`crates/frd/src/worker.rs`) clears the environment except `DISPLAY`/`XAUTHORITY` and passes only its stdin/stdout pipes; certificate keys, the Tailscale socket, approval IPC and input authority stay in `frd`. | **Residual Trust**: a compromised capture/encode worker has the host user's full privileges plus the X display. Process separation gives crash/hang containment only (AGENTS §3.2). Hardware encode (VA-API/NVENC) is not implemented. |
| **Linux (x86_64)** | Client Opus Decoder (`fr-opus-worker`, one per audio epoch) | **Enforced** before any packet: the inherited socket must be a stream from the parent (`SO_PEERCRED` uid and pid); `RLIMIT_AS` 256 MiB, `RLIMIT_CORE`/`RLIMIT_FSIZE` zero; every other descriptor closed (`close_range`); `PR_SET_NO_NEW_PRIVS`; an allow-list seccomp filter on all threads (`SECCOMP_FILTER_FLAG_TSYNC`): reads/writes only on the inherited descriptors, memory calls without executable mappings, clocks/signals/futex/exit, everything else `EPERM` (no sockets, opens or process creation), a non-x86_64 syscall ABI kills the process. | Tested: file open, socket creation and process spawn return `EPERM` (`opus_process.rs`). | `crates/fr-native/src/opus/process/sandbox.c`, launched by the client's audio playout supervisor (20c49ef, 7e15e31). | Decodes host-generated (potentially hostile) Opus off the controlling thread; libopus and the kernel remain trust boundaries. |
| **Linux (x86_64)** | Host Audio Capture / Opus Encoder (`fr-media-worker --audio`) | **None** (no kernel sandbox). | **Unenforced**: network, filesystem and process creation are available to the same-user worker. | Process boundary only; `frd` links neither libpulse nor libopus. | **Residual Trust**: a compromised audio worker has the host user's privileges and the PulseAudio session. |
| **Windows** | Client Decoder / Presenter | **Not implemented**: no Windows worker exists. | Not applicable. | None. | Not applicable until a Windows worker exists. |
| **Windows** | Host Capture / Encoder (Desktop Duplication / NVENC / AMF) | **Not implemented**: no Windows worker exists. | Not applicable. | None. | Not applicable until a Windows worker exists. |
| **macOS** | Client Decoder / Presenter | **Not implemented**: no sandboxed macOS worker exists. | Not applicable. | None. | Not applicable until a macOS worker exists. |
| **macOS** | Host Capture / Encoder (ScreenCaptureKit / VideoToolbox) | **Not implemented**: no sandboxed macOS worker exists. | Not applicable. | None. | Not applicable until a macOS worker exists. |

---

## 3. Enforced Escape-Attempt Verification

Verification is automated in `crates/fr-native/tests/decoder_sandbox_escape.rs`.
Before 2026-09-27 that test only checked that each probe failed, and its TCP and
approval-IPC probes targeted endpoints that did not exist, so they would have
failed without any sandbox. It now proves the filter: the parent opens a TCP
listener, a Unix listener, a readable file and a writable directory; the child
first runs every probe **unconfined** against those targets (each must succeed),
then enters `confine_decoder_process` and repeats them against the same targets,
where each must fail with `EPERM` (errno 1) specifically:

| Probe | Unconfined control run | Confined |
|---|---|---|
| TCP connect to the parent's listener | succeeds | `EPERM` |
| UDP bind `127.0.0.1:0` | succeeds | `EPERM` |
| open the parent's readable file | succeeds | `EPERM` |
| create a file in the parent's directory | succeeds | `EPERM` |
| connect to the parent's Unix listener (stand-in for an approval socket) | succeeds | `EPERM` |
| spawn `/bin/true` | succeeds | `EPERM` |

The Opus decoder child's test (`crates/fr-native/tests/opus_process.rs`,
`confinement_forbids_files_sockets_and_new_processes`) asserts `EPERM` for opening
`/etc/passwd`, creating a socket pair and spawning `/bin/true`, each of which
succeeds for the unconfined test process.

---

## 4. Broker Capability Isolation (Architectural Containment)

Irrespective of OS-level sandbox availability, the FrankenRemote architecture strictly segregates responsibilities across independent processes:

1. **No Tailscale Control Access:** Media workers never inherit or access the local Tailscale socket (`tailscaled.sock`), LocalAPI bearer keys, or network control credentials.
2. **No Certificate Private Keys:** TLS session keys and host certificates remain confined within the broker/transport engine (`frd`). Media workers process only raw YUV/BGRA pixel surfaces or encoded HEVC NAL units.
3. **No Approval Endpoint Access:** The local user approval IPC (prompts, consent tokens, whitelist storage) is isolated from worker pipes.
4. **No Input Lease Authority:** The input agent evaluates input validity tickets and generation fences independently at submission time. The media worker has no input injection capability and cannot forge input receipts.

---

## 5. Residual Trust Summary

As required by plan §5.4:
- Process separation provides crash and hang containment.
- When running software HEVC presentation under Linux, kernel seccomp BPF provides strict syscall confinement.
- Where GPU hardware acceleration (VA-API, NVENC, DXGI, VideoToolbox) is utilized, vendor user-mode drivers and kernel DRM/KMT interfaces require privileged access that cannot be fully filtered by generic sandboxes without breaking acceleration.
- A compromised worker operating under the same user UID is **not claimed to be harmless**. Only the sandboxed Linux decoder role is denied network, filesystem and process creation by the kernel; the host capture/encode worker is merely not *given* certificate keys, the Tailscale socket, approval IPC or input authority, and can still reach the network and the user's files.

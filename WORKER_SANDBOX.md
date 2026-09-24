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
| **Linux (x86_64)** | Host Capture / Encoder (X11 `XGetImage` + software x265) | **None** (no kernel sandbox). | **Unenforced**: network, filesystem and process creation are all available to the same-user worker; no seccomp or Landlock filter is applied. | Process boundary only. The launcher (`crates/frd/src/worker.rs`) clears the environment except `DISPLAY`/`XAUTHORITY` and passes only its stdin/stdout pipes; certificate keys, the Tailscale socket, approval IPC and input authority stay in `frd`. | **Residual Trust**: a compromised capture/encode worker has the host user's full privileges plus the X display. Process separation gives crash/hang containment only (AGENTS §3.2). Hardware encode (VA-API/NVENC) is not implemented. |
| **Windows** | Client Decoder / Presenter | **Not implemented**: no Windows worker exists. | Not applicable. | None. | Not applicable until a Windows worker exists. |
| **Windows** | Host Capture / Encoder (Desktop Duplication / NVENC / AMF) | **Not implemented**: no Windows worker exists. | Not applicable. | None. | Not applicable until a Windows worker exists. |
| **macOS** | Client Decoder / Presenter | **Not implemented**: no sandboxed macOS worker exists. | Not applicable. | None. | Not applicable until a macOS worker exists. |
| **macOS** | Host Capture / Encoder (ScreenCaptureKit / VideoToolbox) | **Not implemented**: no sandboxed macOS worker exists. | Not applicable. | None. | Not applicable until a macOS worker exists. |

---

## 3. Enforced Escape-Attempt Verification

Verification is automated in `crates/fr-native/tests/decoder_sandbox_escape.rs`. The test spawns a confined worker process under active seccomp BPF confinement and verifies that every attempted sandbox escape is refused with typed OS errors (`PermissionDenied` / `EPERM`):

1. **Network Escape (TCP Connect):**
   - Probe: `TcpStream::connect("127.0.0.1:80")`
   - Result: Refused (`PermissionDenied` / `EPERM`). Kernel BPF filter returns `FR_SC_DENY` on `sys_socket` and `sys_connect`.
2. **Network Escape (UDP Bind):**
   - Probe: `UdpSocket::bind("127.0.0.1:0")`
   - Result: Refused (`PermissionDenied` / `EPERM`).
3. **Filesystem Read Escape:**
   - Probe: `File::open("/etc/passwd")`
   - Result: Refused (`PermissionDenied` / `EPERM`). Kernel BPF filter blocks `sys_open` and `sys_openat` with flags creating or opening arbitrary paths.
4. **Filesystem Write Escape:**
   - Probe: `File::create("/tmp/fr_sandbox_escape_probe.txt")`
   - Result: Refused (`PermissionDenied` / `EPERM`).
5. **Approval IPC Escape:**
   - Probe: `UnixStream::connect("/tmp/frd-approval.sock")`
   - Result: Refused (`PermissionDenied` / `EPERM`). Prohibits reaching the host's approval channel from an untrusted media decoder.
6. **Process Spawning Escape:**
   - Probe: `Command::new("/bin/sh").spawn()`
   - Result: Refused (`PermissionDenied` / `EPERM`). Kernel BPF filter denies `sys_clone`, `sys_clone3`, `sys_fork`, `sys_execve`, `sys_execveat`.

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

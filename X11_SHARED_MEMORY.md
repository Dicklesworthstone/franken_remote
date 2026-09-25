# Bounded X11 shared-memory transfers

Implementation slice for `fr-rc-media-xshm-capture-e23`, under plan sections
10, 11 and 19. This is a CPU-staging optimization, not hardware encoding,
zero-copy, transport qualification, or an end-to-end latency claim.

## Capture

Both `X11Surface::snapshot` (root capture) and `X11SelectedCapture::snapshot`
(selected whole monitor) use one retained MIT-SHM image when the original local
X connection supports the fd-capable 1.2 extension. The capture worker reaches
these paths without a new command or alternate capture/permission mechanism.
Selected capture still validates the original topology before and after each
readback. Neighboring displays are not copied. A resize-away-and-back or monitor
replacement still retires the original source, even with identical final bounds.
Explicit readback of a presentation window remains an instrumentation operation,
not a capture-source witness, and retains the existing socket implementation.

The native owner uses checked XCB requests on the **same Xlib connection**, through
libX11-xcb. Xlib retains event ownership. No second X connection, global X error
handler swap, callback, background thread, or image queue is added. A completed
GetImage reply establishes that the server has finished writing before Rust's
owned BGRA output is filled. A failed shared-memory request is terminal for that
transfer, never a reason to retry an uncertain read on another path.

The image is at most 8,192 pixels per dimension, 16,777,216 pixels total, and
64 MiB of backing storage, with exact checked 32-bit packed stride. Only depth-24
TrueColor with the admitted RGB masks and little-endian BGRA storage is accepted.
The unused alpha byte is initialized to 255 before pixels leave this boundary.
Rust's negotiated geometry limits still apply before this native allocation.

The fd-backed implementation deliberately uses an anonymous, close-on-exec
`memfd`, sealed against shrink/grow, rather than a persistent SysV shmid. XCB takes
ownership of the descriptor during attachment; only the mapping and the server
resource remain afterward. This prevents an orphan SysV segment when a worker
is killed before it can execute cleanup. Detach completes before local unmapping;
`XDestroyImage` never frees mmap memory. The owning Display outlives the transfer.
All blocking X-server work remains inside the supervised media boundary; it is
not made wall-clock cancellable by this optimization. The selected user and X
server remain trusted, as before. The decoder seccomp allowlist is unchanged.

## Explicit path and copy accounting

`transfer_statistics()` on root/selected capture and `ChangeAwareCapture`
returns the original native owner's selected path and bounded counters. A path
is `NotInitialized`, `SharedMemory`, or `Socket` with a specific reason:
extension unavailable, descriptor transport unavailable, shared allocation
unavailable, or attachment refused. Selection is sticky, not periodically retried.

Counters report completed image transfers, explicit BGRA-copy bytes, pixel bytes
returned through the X socket, and retained image backing bytes. They exclude
codec/GPU/server-internal copies, protocol headers, allocation initialization,
and alpha initialization; they must not be presented as total memory bandwidth.
Counters saturate at `u64::MAX`, never wrap. Root retirement releases backing
storage while retaining its historical counters. Fixed native/XImage metadata
is additional, bounded per-owner storage. The SHM capture image is one additional
source-stage buffer alongside the existing bounded Rust BGRA snapshots.

## Dependency boundary

Linux media adds the narrow system `libX11-xcb.so.1` ABI and libxcb to the existing
X11 dependency. It uses the distribution's Xproto MIT-SHM structures and XCB's
checked request/fd ownership APIs, not a second X client implementation or new
Rust dependency. `linux-media` builds need libxcb development headers/libraries
and the libX11-xcb runtime; browser and daemon-only builds do not inherit them.
No build-time downloading or unprotected native-library search path is added.

## Verification

Run `bash scripts/test_x11_image.sh` for the production C transfer owner against
real isolated Xvfb servers, with MIT-SHM enabled and disabled. The test includes
independent XGetImage pixel comparison, selected rectangles, invalid sizes and
buffer guards, a real descriptor-allocation refusal, checked X error handling,
terminal failed transfers, repeated cleanup and client descriptor counts. Each
mode also measures 300 warm 1920x1080 readbacks and retains its output.

The Rust `image_transfer` integration target exercises the actual public root and
selected-monitor APIs, generation retirement and four concurrent original X
connections. Existing idle-presentation and decoder-sandbox tests remain intact.

Session evidence: the pinned nightly checked the full production native Rust
library with linux-displays and passed strict Clippy. Three new Rust integration
tests, twelve unchanged idle-presentation tests and the unchanged kernel sandbox
escape test passed. This container lacks FFmpeg SDK headers: those runs linked
the exact X11 portion of bridge.c through a focused external manifest, not an
alternate X11 implementation. They do **not** constitute a full Cargo workspace,
full FFmpeg translation-unit or media-worker build. The corresponding ordinary
workspace commands remain required on a fully provisioned builder.

In the retained container Xvfb run, capture median/p95 microseconds were
3,261/4,334 (shared) and 5,814/7,606 (socket). An earlier run measured 872/1,082
and 5,589/6,699 respectively; timing varied in this shared environment and is not
a guaranteed speedup. Both modes still copy 8,294,400 BGRA bytes into Rust per
frame. The shared path reports zero X-socket pixel payload bytes per frame versus
8,294,400 for the socket path. Real-X-server, hardware, optical and two-machine
measurements are not tested here; the broader bead remains open.

# Separate native cursor capture

The original Linux capture worker now answers `ReadCursor` (private IPC kind 16)
with `CursorSnapshot` (274), or its existing typed `NeedInput`/`Refused` response.
The request has an empty body and retains the original worker epoch, role,
sequence, parent pipes, and supervision boundary. It is legal only after capture
configuration. It does not consume a video frame ID, perform framebuffer
readback, encode HEVC, or mint `UnchangedCapture`/source-age evidence. A moving
pointer between native queries produces `NeedInput`, not invented coordinates.

Both root-screen and selected-monitor paths borrow their original X11 connection;
neither reopens DISPLAY nor admits another capture source. Selected capture checks
scope before requesting the native shape and again before copying it, normalizes
coordinates to the selected rectangle, and revalidates topology on both sides.
An outside pointer returns the distinct one-byte outside-scope result, with no
bitmap or position from a neighboring monitor. Removal/re-addition with identical
bounds still retires the original source. A new cursor request cannot revive it.

## Shape bounds and ownership

The only added production native dependency is the explicit system ABI
`libXfixes.so.3`, linked under the existing `linux-media` feature. There is no new
Rust dependency, runtime, downloader, or display connection. The test fixture
uses system `libXcursor.so.1` to create real ARGB cursors; it is not a production
dependency. XFIXES 1 or newer is checked on the server; missing support refuses.
The platform package must provide this system library; no filename-based download
or untrusted search-path provisioning is performed here.

XFixes returns one library-owned image. Its public ABI uses unsigned-long ARGB
pixels; channels are converted from premultiplied ARGB to straight-alpha RGBA8.
Transparent pixels do not leak hidden RGB. Native dimensions, hotspot, byte count,
and the complete private response size are validated before any Rust pixel copy
or allocation. The cursor is at most 256 by 256, further reduced by the admitted
control-message byte limit. The private response includes its 36-byte IPC header
in that limit. The borrowed response decoder validates exact lengths and geometry
without allocating another bitmap. Diagnostics omit pixels and cursor names.

The X server and XFixes library remain a native trust boundary: XFixes' internal
reply allocation occurs before these Rust checks. These calls therefore stay in
the supervised media process, never in an input/approval owner. This is process
containment, not an OS sandbox or cancellable foreign call. The caller must keep
its cursor requests and retained replies bounded like every other worker request.

## Visibility is not fabricated

**XFIXES image capture does not reveal actual cursor visibility or pointer lock.**
The real server returns the logical image even after a different client calls
`XFixesHideCursor`. The native API reports a logical observation only, and the
private response deliberately contains no visibility bit. Its native serial is
scoped to the original X server, not reusable as a network shape ID. `shape()`
returns neutral flags; it is not a ready-to-publish visible cursor assertion.

The XFIXES protocol separates image tracking from cursor hiding, and keeps
CursorNotify active even while hidden:
<https://www.x.org/releases/current/doc/fixesproto/fixesproto.txt>.
A qualified visibility/lock owner, connection-scoped shape identity, positive
cursor capability negotiation and ordinary source/subscriber observation checks
remain necessary before remote publication. This commit does not enable a
network cursor channel, alter input authority, or silently turn unknown visibility
into either known-visible or a claimed observed-hidden state.

## Executed verification

Ten focused tests pass: seven actual Xvfb/X11 cursor and selected-RandR cases,
and three private IPC/worker cases. They cover real shapes, hotspot/position,
alpha conversion, hidden-image negative evidence, missing XFIXES, selected-scope
edges, oversized outside/inside shapes, same-bounds topology retirement, bounded
borrowed parsing, pre-bootstrap refusal, and separate production child processes.
The worker test encodes actual software HEVC around cursor queries and requires
the next unchanged observation to keep the original reference and candidate IDs.
No synthetic codec output or externally substituted runtime is used.

Native library builds/strict Clippy pass for `linux-media` and `linux-displays`;
the production worker and both new targets pass strict pedantic Clippy. Source
formatting and whitespace checks pass. Testing used pinned nightly-2026-08-31,
Debian FFmpeg 7.1.5, checksum-verified `e0fe083` first-party sources plus the exact
native production changes through `6cbda5de` and this slice. Small external local
manifests select the same production files and feature graph without compiling
unrelated workspace async dev dependencies; repository manifests are unchanged.
Concurrent shared recovery/e2e changes through `97ab5b2` are preserved, not counted
as executed coverage. Full workspace, network integration, GPU/compositor,
physical visibility and live-tailnet qualification are not claimed.

Retained initial failures: this Xvfb build aborts during disabled-XFIXES client
teardown when the fixture holds an unnecessary second observer connection. The
missing-extension case now creates only the real production capture connection;
its refusal and framebuffer assertions are unchanged, with no server-fix claim.
The selected replacement test initially expected GeometryChanged twice; its
second result correctly requires the existing terminal Closed state instead.
No authority deadline, production refusal or prior regression assertion changed.

This advances the native cursor requirement in plan 11.4/17, not a complete
cross-platform cursor, input, media-pipeline or workstation qualification gate.

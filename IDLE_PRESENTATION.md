# Idle native presentation

The X11 renderer retains exactly one tightly packed BGRA image after submission,
reusing the same allocation for subsequent pictures. `maintain_presentation()`
coalesces exposures and restores that submitted image without another capture,
HEVC encode/decode, frame ID, source observation, or visibility acknowledgement.
Before the first picture it neither allocates pixel storage nor submits content.
Quiet maintenance does not redraw. The retained pixel allocation is exactly
`width * height * 4`, within the already validated geometry/byte limits, plus one
fixed-size XImage header. This replaces the previously transient XImage; it does
not retain a decoder-owned surface or introduce a history queue. Fitted output,
including its black bars, is retained as actually submitted.

Both owned and borrowed presenter windows select exposures and lifecycle events
on their original connection. Lifecycle events are processed before querying the
drawable or repainting it. Resize-away-and-back, unmap/remap, destruction, and
borrowed-window reparent retire the original owner and release its image. A turn
consumes at most 128 events; exceeding that bound refuses rather than continuing
an unbounded drain or drawing ahead of an unseen lifecycle fence. Idle repaint
never raises the window. Closing a borrowed renderer never destroys the UI's
window. Owned-window cleanup relies on XCloseDisplay's default DestroyAll mode,
not a second destroy request against a possibly dead XID.

This is still a thread-confined Xlib boundary in a supervised media process,
not a sandbox, cancellable foreign call, compositor or physical scanout proof.
A server failure can terminate the process. Source freshness and control remain
owned by their existing independent verifiers; repaint cannot refresh either.
The Xlib reference for event and connection ownership is
<https://www.x.org/releases/current/doc/libX11/libX11/libX11.html> (Closing the
Display, Event Queue Management). No dependency or alternate runtime was added.

## Executed scope

At source baseline `baab9c6283678b79c52cb8775b874d6b0b76d764` plus this slice,
24 native tests passed with the repository-pinned nightly-2026-08-31, actual
Xvfb/X11/RandR/DAMAGE, and Debian FFmpeg 7.1.5: nine new `idle_presentation`
tests, the unchanged fitted-presentation test, and 14 unchanged root/selected
DAMAGE capture tests. The capture targets include real software HEVC and worker
processes; the new presentation target uses exact pattern pixels and independent
X11 clears/readbacks, not hardware or optical evidence. Native library/worker
builds, production Clippy and the new target's strict pedantic Clippy passed.
The focused harnesses link newly source-built native/core/media/wire libraries;
the broader Asupersync-based `worker_process` target and full workspace are not
part of this executed scope. Initial testing caught and repaired the redundant
XDestroyWindow cleanup after destruction; a fitted fixture was corrected to use
an admitted downscale rather than unsupported upscaling. No assertion weakened.

This advances plan sections 11.2–11.4 and `fr-p1-frame-pipeline-am1`, not closure
of the frame-pipeline or workstation qualification gates. Automatic idle-worker
wakeups are the next integration slice; the renderer API itself is exercised
against the real server here.

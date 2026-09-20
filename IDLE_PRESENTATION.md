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

## Worker integration

The presentation worker now waits for either its original X11 connection or its
original parent command pipe, without a polling timer, extra thread, reopened
display, replacement runtime, or new protocol message. Exposures repair the last
submitted image during complete video silence. A decode-only reference picture
does not replace that image, but remains usable by the next dependent decode.
An idle lifecycle change terminates the old worker even without another frame;
it reports the existing bounded diagnostic and closes the pipe, never inventing
an unsolicited response identity. The parent still owns independent supervision.

Parent readiness (including EOF) takes priority over queued native events before
any new Xlib work. The command reader is unbuffered from the first bootstrap
record: `StdinLock` prefetch must not hide an already-received Stop from descriptor
readiness. The exclusively owned presentation connection drains unrelated local
events within the same 128-event bound, so a queued ClientMessage cannot turn an
idle wait into a busy loop. Capture/DAMAGE connection behavior is unchanged.

The final executed group contains **37 distinct passing tests**: twelve
`idle_presentation`, ten `idle_worker`, one unchanged fitted-presentation, seven
unchanged DAMAGE-capture, and seven unchanged selected-DAMAGE tests. Thus 22 tests
were added across these two commits; the other 15 remain unchanged. The new worker
tests launch a separate production media process, configure admitted hvcC, decode
real software HEVC, and independently clear/read back the UI window while sending
no additional frame requests. They also cover fitted bars, reference-only decode,
pre-first-frame exposure, three commands in one write, EOF, idle topology loss,
destruction, borrowed-window survival, and unrelated-event CPU spin. The CPU check
is only a coarse busy-loop regression, not the product's idle-performance gate.

The exact same idle-repair assertion fails against the retained pre-change
`baab9c628` worker (the cleared image is never restored) and passes against the
new worker. Both new test targets also pass three additional serial repetitions
and a default-parallel run. Final production library/worker Clippy and both
standalone test targets' strict pedantic Clippy pass, as do formatting and
whitespace checks. The worker fixture's initial configuration assertion was
corrected to require the protocol's existing PresentationReady variants rather
than DecoderReady; no production reply, timeout, or assertion was weakened.

Execution uses the same pinned toolchain and native SDK/runtime stated above,
with source-built first-party libraries from `baab9c628` plus `17f95e7d` and this
worker integration. No legacy Asupersync library substitutes for the current
workspace runtime. The broader Asupersync-based integration target, full workspace,
physical scanout, GPU, compositor and live-tailnet qualification remain outside
this executed scope. This advances plan sections 11.2–11.4 and
`fr-p1-frame-pipeline-am1`, not closure of the frame-pipeline or workstation gates.

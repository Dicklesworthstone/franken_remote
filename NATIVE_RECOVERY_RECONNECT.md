# Bounded fresh-session viewing recovery

The native observation profile can recover a lost reference chain without
restarting the current connection or decoder. The bounded QUIC namespace retains
retired stream identities, however: after one complete three-role replacement,
there is insufficient space for another. This change does not recycle identities,
increase a protocol limit, or tear down a healthy stream just because capacity
for a future replacement has run out.

When a real receiver failure needs recovery, its existing report owner checks
for three available role pairs **before sending** the failure request. It returns
`NamespaceExhausted` with the original failed-chain reason. The current receiver
is already fenced. An unsatisfiable request does not consume send credit or new
binding IDs, and its cause is not lost behind a later host-side connection close.
Connection, receiver-scope, permission, cancellation and original recovery-deadline
checks still run before this result can be produced.

The native reconnect supervisor permits a fresh attempt for this typed result
only when the original reason is `ReferenceExpired` or `RecoveryExpired`.
`DecodeFailed`, arbitrary recovery closure, unauthorized access and malformed
protocol remain terminal. The narrowly defined streamed worker deadline below
also permits a fresh attempt after cleanup. Known QUIC `Native`/`Expired`
errors inside recovery/replacement wrappers retain the same retry policy as
their existing direct transport equivalents; arbitrary backpressure is not
classified as a reason to reconnect.

This is **fresh-session reconnect**, not a second in-place recovery. The existing
supervisor must fence the old session, confirm native cleanup, honor cancellation,
and wait its bounded backoff before attempting a new Tailscale identity lookup,
TLS connection and session admission. Its finite attempt counter never resets
because a previous attempt managed to display frames. Host approval and display
selection are performed again as required; no old input authority, action,
window, worker, attachment or recovery deadline is restored. Failed or stalled
cleanup terminates rather than allowing overlapping native owners.

## Established native worker timeout

A decode/present request that misses its original native deadline still ends
that worker and session immediately. Only the established-streaming result
`Streaming(Media(Worker(Deadline)))` can now enter the existing fresh-session
supervisor. The supervisor must confirm cleanup of the original child, window,
input capture and other native owners before its fixed backoff and another
attempt. Cleanup error or expiry remains terminal. Attempts share the original
finite counter; successful frames do not reset it. This bounds repeated hangs
without reusing a poisoned decoder or admitting overlapping owners.

The media deadlines are unchanged. A positively negotiated reference-recovery
session already retains and drains an in-flight native borrow under the fixed
two-second ceiling while its expired picture is fenced and input is suspended.
A responsive late reply can still recover on the same running decoder. A request
that reaches the native ceiling poisons the worker; this new path instead creates
an independently authenticated session, receiver, decoder epoch, mapping and IDR
startup after confirmed native cleanup. It neither revives the failed reference
chain nor claims that killing the child reclaimed GPU driver resources.

Decoder configuration, initial first-frame, and replacement-handshake timeouts
remain terminal, as do malformed media, explicit codec refusal, broken pipes,
ambiguous process exit and cancellation. A genuine local stop or authenticated
host terminal report still vetoes reconnect even when the worker timeout wins
the service race. Previously requested control stays spent: recovery opens only
viewing until a new explicit local control decision. Historical external effects
and their receipts remain in the original request's report.

The native/network join still fences input before polling the original pending
network future, then drains that same future. If it returns an already validated
host closure or revocation, that exact typed report takes precedence over a
racing native timeout; cancellation and bare socket closure do not. The join
does not delay the fence or fetch later packets to seek terminal evidence.
The lower-level native adapters also retain their original observer's explicit
closing reports across reap. The supervisor's default `terminal_failure()` hook
reads that bounded evidence only for an already failed attempt after successful
cleanup and can only stop another attempt with its exact reason. A normal
successful user-requested close remains successful even when its closing
exchange has a report. A failed Cleaning notification cannot erase retained
terminal evidence; the supervisor's cancellation/clock checks keep their
precedence. The evidence resets at the next attempt; failed cleanup remains
dominant. Optional file owners join under the same original
cleanup deadline before either native adapter can permit replacement.

The focused runtime regression
`a_hung_stream_worker_is_reaped_before_a_fresh_epoch_can_present_again` uses
actual TLS/UDP, an X11 window and supervised fixture children. It positively
negotiates reference recovery, hangs only the first decoder epoch after initial
startup, rejects another attempt while its native owner remains retained,
requires explicit child/window reap, and requires a subsequent streamed picture
from the fresh epoch. Codec replies are synthetic and do not qualify HEVC,
hardware, physical visibility or installed-tailnet identity. Classifier and CLI
regressions cover terminal startup/codec errors, cleanup failure, host revocation,
local stop and control non-replay. These new Rust runtime regressions have not
been executed in the resource-constrained editing environment; the pinned
Asupersync dependency build was killed before their test artifacts could be
produced. The live loaded-client five-minute acceptance remains open under
`fr-live-client-present-cost-tqu7`.

## Control CLI: recover viewing without reacquiring input

The shipped Linux `fr connect --control` uses the same finite `--attempts`
budget as observation (default five; `--attempts 1` disables automatic retries). Its
control decision is spent before submitting the first request, including a
request whose admission or reply is uncertain. An established transient
transport/media failure can then recover a fresh **viewing-only** desktop after
confirmed native cleanup. The retry classifier includes the streamed native
worker deadline described above and the existing observation-session expiration
and renewal timeouts. All operation and cleanup deadlines remain unchanged. Explicit control-authority revocation/refusal, input expiry, user close,
local escape, focus or native capture failure, protocol refusal and TLS/identity
failure do not authorize another attempt.

Every retry creates fresh viewing state, mapping, decoder and window, performs
fresh admission/approval, and keeps the original installed node identity pinned.
The control-capable startup profile grants no input by itself. A recovered
window remains viewing only until the user deliberately presses and releases
**F8** while that window has focus. Its title advertises this action. This sends
one ordinary control request on the fresh session, with a new lease and the
unchanged request deadline, approval, mapping and freshness checks. Focusing the
window, receiving a new frame, or reconnecting never requests control. A refused
or uncertain request is not retried automatically.

This gesture restores input only. Original file selections and clipboard consent
are not replayed or reconfigured. Once the original request is spent, subsequent
client offers omit the five optional file capabilities (upload, download and
duplex included) before the fresh handshake. The supervisor permits only removal
of original optional capabilities: required control, profile, role, versions and
limits remain identical. This prevents a fresh grant from opening an unowned
file lane while retaining the original upload/download handles and receipts for
the completion report. Before the first request is spent, retries preserve the
original file offer. Optional playback audio retains the existing observation
policy and can restart on a fresh viewing attempt; F8 never repeats its setup.

The shortcut is local and consumed before native input capture is created. The
native boundary requires a real unmodified F8 press/release, allows the actual
Caps/Num/Scroll lock modifier mappings, and rejects synthetic events, keys already
held at arming, autorepeat while held, focus loss and keyboard-map changes.
A server timestamp barrier retains the age of queued events; both the gesture
and the resulting one-slot intent have a fixed 250 ms bound. An old/not-ready
gesture is discarded and a new physical gesture is required. No shortcut event
or held modifier is carried into the new input owner.

Every attempt retains its own native closing exchange and first input-stop
cause, including observation-only sessions and recovered viewing desktops that
never request control. A genuine local stop or authenticated host terminal
report that races with a link failure still vetoes reconnect after cleanup and
names the terminal reason. This evidence is scoped to the current attempt;
repeated callbacks from an older desktop cannot become a later dial's report.
The original and fresh control-request histories remain separate and unchanged.
A partial acknowledgement without a host report does not invent revocation,
and the cleanup callback cannot turn failed cleanup into success.

JSON distinguishes `last_attempt_mode` (`control_capable`, `view_only`, or
`explicit_control`),
`viewing_reconnects` (new viewing desktops actually opened),
`control_request_attempt` and `control_attempt_end`. `control_granted`, input
results and clipboard/file outcomes describe historical effects; they do not
claim that the final viewing attempt owns control. `control_close_exchange` and
`control_lease_report` retain the original controller's evidence separately from
the final connection's `close_exchange` and `lease_report`. The same distinction
survives terminal errors and human-readable output. New `explicit_control_requests`
entries retain each fresh local request's own attempt, granted capabilities,
input effects, end and cleanup reports. The fixed history covers the existing
32-attempt maximum; it cannot replace or extend the original report.

## Change the selected display from the running viewer

In an ordinary native `fr connect` viewer, press and release
**Ctrl+Alt+Shift+F7** while the viewer has focus to select another host display.
The action is available while viewing or controlling and is advertised in the
window title. It is disabled while a control request is pending, for one-shot
observation, and for foreground automation.

This is a deliberate fresh-session operation. The original viewer stops
accepting input, performs its bounded closing exchange, and retains its decoder,
window, input, clipboard and file owners until native cleanup completes. Only a
successful original service result and positive native cleanup permit the
supervisor's distinct `Reopening` notification. Cancellation, callback failure,
WM close, local escape, capture failure, adverse authenticated host reports and
known failed cleanup prevent a new connection. A successful close while still
viewing uses an ordinary session close and never fabricates an input lease;
staged or pending control requests retain immediate terminal cancellation.

The new attempt repeats installed-tailnet identity checks, TLS, admission and
approval, then opens a native picker for its newly received catalog. It cannot
reuse the old display handle, viewport, frame witness, decoder generation, lease,
ticket or held keys. This is not an in-place geometry change or simultaneous
multi-display streaming. Automatic retries after the change also use a fresh
picker. Cancelling that picker ends the invocation after cleanup.

Every new selected session starts viewing. A display change also retires an
**unused** initial `--control` decision; it does not mark that decision as a
request that actually happened. A fresh F8 gesture on a later mapped window may
request a new lease through the existing mapping, freshness and approval gates.
Neither the initial file choices nor clipboard consent is configured again.
Optional playback audio keeps its existing fresh-attempt observation policy.

The shortcut is consumed locally. It requires the complete physical key chord,
rejects synthetic or initially held gestures, and prevents repeats from renewing
the original press time. It uses the original server/native capture
time with a 250 ms age bound, and invalidates stale choices after focus, mapping
or mode changes. During control, the original capture owner withholds the
triggering batch and parks further exports until canonical session close; a
500 ms fallback ends an unresponsive owner. No event or modifier from the
gesture enters a new input queue.

An explicit change is independent of the fixed automatic retry allowance, so
it works with `--attempts 1` and never resets failure backoff or the remaining
retry budget. All initial, automatic and explicitly reopened sessions share a
monotonic maximum of 32 attempts with checked fresh worker epochs. Another
explicit change at that ceiling reports `session_attempt_limit` after cleanup.

Completion includes `display_changes` and `display_change_closes`: each accepted
change's original attempt number, session close exchange and optional lease
report. These remain visible when a later picker or connection fails. Unknown
remote cleanup and outstanding effects stay unknown; `Fenced` is never upgraded
to physical release. A bounded close timeout or closed transport can retain
uncertainty while opening fresh viewing, but known protocol, authorization,
clock or cancellation errors and adverse host reports veto reopening.
`initial_control_decision_retired_at` distinguishes unused consent retired by a
display change from an actual `control_request_attempt`.

Regression sources cover the real production C/X11 boundary, Rust native intent
and chord policy, session close custody, cleanup/report vetoes, consent retirement
and the connector's identity, cancellation and attempt budgets. The C harness
uses a private Xvfb and actual XTest events. The connector/streaming fixtures use
the actual UDP/TLS session implementation; authoring them is not evidence that
they ran. This change does not qualify a full physical multi-monitor desktop or
the experimental native transport.

Regression targets cover the spent request, later cleanup, late focus/escape
stops, authenticated revocation, cleanup failure precedence and distinct original
versus final closure metadata without starting native I/O. The ignored native
case `real_control::reconnect::transient_control_transport_loss_reconnects_fresh_viewing_without_reacquiring_input`
uses the shipped client, a real kernel route failure, fresh Xvfb pixels and independent
host input observations. The new native case
`real_control::reconnect::a_new_native_gesture_requests_fresh_control_after_viewing_reconnect`
first publishes a selected file, then reconnects viewing and presses F8. It checks
input on the same recovered window, no shortcut delivery to the host, and retention
of the original durable file receipt and exact published bytes despite a changed
original source. The existing workflow invokes both reconnect cases. Pure
intent/history tests cover expiry, one-use decisions, separate effects and late
terminal cleanup. CLI offer tests cover upload/download/duplex with each
clipboard/audio combination, including pre-request retry and historical report
preservation.

The native C gesture passed a strict warning-as-error build and actual Xvfb/XTest
checks. Fourteen checks covered fresh one-use choice, stale queued events, held
and synthetic input, modifiers, focus loss, mapping changes and unmap; seven
additional checks on the final source covered the arming race, recovery after
release and actual lock modifier masks. The exact unchanged Choice module also
compiled with the pinned Rust toolchain and passed all four unit tests in an
isolated bounded process. The complete native reconnect case and remaining Rust
integration tests were not executed in the resource-constrained editing
environment; they remain distinct from those completed unit and C/X11 checks.
These namespace fixtures do not establish live-tailnet, physical-device or
hardware qualification.

## Evidence scope

Actual TLS/UDP tests exhaust the namespace through completed role replacements,
then check healthy-stream survival, unsent reference-loss requests, original decode
failure and authorization refusal. Another test uses the canonical host/viewer
loops, real attachments and supervised synthetic codec children to verify the
namespace error survives session teardown while original worker reap custody is
retained. Policy and existing supervisor tests cover cause classification,
cancellation, finite backoff and cleanup ordering. These are separate tested
boundaries, not a claim of a complete live desktop reconnect across two losses.

Codec bytes/completions are fixtures, not HEVC, hardware or live-tailnet
qualification. The first recovery still uses the original workers and connection;
a later recovery-demanding loss requires the bounded fresh-session path described
above. Related scope: `fr-p1-loss-recovery-20s` and `fr-p1-fr-client-bis`, plan
sections 12, 14, 17 and 19.

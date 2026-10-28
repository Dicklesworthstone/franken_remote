# Bounded reconnect after recovery-channel exhaustion

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
`DecodeFailed`, worker failure/hang, arbitrary recovery closure, unauthorized
access and malformed protocol remain terminal. Known QUIC `Native`/`Expired`
errors inside recovery/replacement wrappers now retain the same retry policy as
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

## Control CLI: recover viewing without reacquiring input

The shipped Linux `fr connect --control` uses the same finite `--attempts`
budget as observation (default five; `--attempts 1` makes one attempt). Its
control decision is spent before submitting the first request, including a
request whose admission or reply is uncertain. An established transient
transport/media failure can then recover a fresh **viewing-only** desktop after
confirmed native cleanup. The existing retry classifier and deadlines remain
unchanged, including retries for observation-session expiration and renewal
timeouts. Explicit control-authority revocation/refusal, input expiry, user close,
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

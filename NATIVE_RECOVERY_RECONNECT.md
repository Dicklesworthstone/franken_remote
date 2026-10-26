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
The control-capable startup profile grants no input by itself. After its one
request, the CLI never submits another request, accepts a control state, creates
an input capture, or configures another clipboard/file owner. Its optional
playback audio can restart as observation media. A new command is required to
request control again.

The original upload/download handles and completion receipts remain available.
Native cleanup also reports its exact original closing exchange, lease report
and first input-stop cause. A genuine local stop or authenticated host terminal
report that races with a link failure still vetoes reconnect after cleanup.
The cleanup callback is informational: it cannot turn failed cleanup into success.

JSON distinguishes `last_attempt_mode` (`control_capable` or `view_only`),
`viewing_reconnects` (new viewing desktops actually opened),
`control_request_attempt` and `control_attempt_end`. `control_granted`, input
results and clipboard/file outcomes describe historical effects; they do not
claim that the final viewing attempt owns control. `control_close_exchange` and
`control_lease_report` retain the original controller's evidence separately from
the final connection's `close_exchange` and `lease_report`. The same distinction
survives terminal errors and human-readable output.

Regression targets cover the spent request, later cleanup, late focus/escape
stops, authenticated revocation, cleanup failure precedence and distinct original
versus final closure metadata without starting native I/O. The ignored native
case `real_control::reconnect::transient_control_transport_loss_reconnects_fresh_viewing_without_reacquiring_input`
uses the shipped client, a real kernel route failure, fresh Xvfb pixels and independent
host input observations. Its namespace fixtures do not establish live-tailnet,
physical-device or hardware qualification.

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

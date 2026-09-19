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

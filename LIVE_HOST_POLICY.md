# Live local policy and native connections

`frd::host_policy::live::Watch` is an opt-in read-only monitor of the existing
protected `Store`. `Server::with_live_policy(watch.handle())` attaches that
monitor to the original native host owner, including its protected Linux and
shared-observer paths. Keep the Watch on the independent broker lifetime,
not in any one peer's scope, and retain it until `try_finish` observes thread exit.

## Admission and revocation

Every native acceptance obtains one immutable lease at **call time**, before its
future can wait unpolled or begin TLS/LocalAPI admission. The observed policy's
sharing scope and approval mode replace those two Request fields. Other limits,
local IDs, negotiated capabilities and deadlines stay unchanged. This explicit
opt-in gives the live file authority over those flags; a stale caller cannot
bypass required consent or silently substitute broader tailnet scope.

The same lease stays in the original connection guard through Host and
HostSession handoffs. It is checked before application dispatch, on transport
I/O and on the existing bounded maintenance pulse even when an application is
parked. Observing ANY later revision retires old leases, even when settings have
changed away and back between reads. Values are never changed in place on a
running session. Only a new acceptance can obtain a new policy epoch.

Malformed/unreadable files, a symlink replacement, deletion of a previously
saved revision, rollback, changed values at the same revision, evidence expiry,
clock reversal, cancellation and monitor failure/drop are terminal. Repairing
an invalid file does not resurrect a retired monitor or lease. The original
terminal cause remains on each lease. No fallback to Request flags or permissive
defaults is allowed after an attached monitor fails. An initially absent file
still uses the Store's existing documented defaults without creating anything.

Policy is not peer identity, an individual approval, capture permission or input
authority. Enabling local approval ends original unapproved connections; it does
not fabricate a new approval for them. Broader policy does not upgrade an existing
observer to control. Completed application effects are not relabeled rollback.
The broker credentials and independent local desktop source are not canceled by
a peer's policy failure. The caller still drives actual OS permission/lock events
and the existing authority cleanup paths.

## Bounds and lifecycle

One process-wide disk worker calls the ORIGINAL Store::load; there is no file
I/O on authority/transport polls, filesystem notifier queue, extra async runtime,
policy database or writer. Reads are attempted at 100ms intervals. Initial
readiness has a two-second limit; subsequent positive evidence expires 500ms
from read START, not reply completion. Each lease independently enforces expiry
if the reader stalls. These are configured bounds, not measured real-time or
suspend guarantees; the enclosing OS lifetime must still fence suspension.

Stop retires policy immediately and wakes the disk worker but never joins an
unfinished filesystem call. A stuck retired reader keeps the one-worker permit
until actual thread exit, preventing unbounded replacements. Explicit
`try_finish` reports thread cleanup separately from the handle's policy status.

## Integration limits

Unmodified callers without with_live_policy retain their startup-selected policy.
The existing CLI save commands still save only: their success does NOT prove that
any running host observed or applied a revision. No CLI output is changed to claim
live application. The installed `frd run` command uses `run_with_policy` and
retains its initial resolved path, forwarding only explicitly provided flags as
process overrides. It never turns a saved snapshot into permanent overrides.

The concurrent capacity-one serial listener calls the same guarded acceptance
for every peer. Its existing fatal-error classification treats policy change or
monitor failure as terminal for that serving operation, not as a reason to
silently resume/replay a session. A local owner may start a new operation only
with valid evidence and the existing transport/ingress cleanup requirements.

Focused tests use real disk operations and worker threads. Native integrations
use actual UDP/TLS, Host/Viewer negotiation and credential-checked Unix HTTP in an
isolated namespace, with explicitly synthetic LocalAPI metadata, test PKI and an
ingress-lifetime fixture. This is not installed-Tailscale, actual input injection,
kernel ingress, hardware-media or full-workspace qualification.

## Shipped-daemon coverage

The serial namespace target also launches the actual `frd` binary, writes policy
with the actual `frd approval` command, and verifies active-share revocation
before the silent viewer's lease deadline. Explicit overrides retain their values
but still retire the old worker before a new listener; malformed policy stops the
daemon. The command output reports publication without claiming acknowledged
live application. Executables at `/usr/sbin/ip` and `/usr/sbin/nft` are fixture
bind mounts visible only to the test child's private mount namespace. No shipping
trust or ingress bypass option is added; kernel-filtering qualification is not
claimed. Build `frd` as well as `fr` before running this integration target.

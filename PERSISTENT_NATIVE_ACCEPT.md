# Persistent native acceptance (capacity one)

`Server::serve_serial_on_protected_listener` serves successive independent peers
on one already protected UDP destination. The pinned native socket owner is
single-connection: this is explicitly **capacity one**, not simultaneous
multi-client routing. It does not queue arrivals while another peer owns the
socket, migrate packets, resume a failed session, or replay application effects.

Each attempt uses the original native accept/TLS/LocalAPI/Host implementation,
the listener's original transport configuration, a fresh request-scoped context,
and locally supplied new connection/session identifiers. The local Application
allocates globally fresh identifiers; an adjacent-ID sanity check is not a
historical collision registry. Host-boot and OS-session identities stay fixed.
Use the same runtime for supervisor and peer contexts. The supervisor must be
independent of broker, credential and shared-source contexts.

The first acceptance starts when the service is polled. Each individual
acceptance retains its fixed acquisition and handshake budgets. Later attempts
wait for the original transport to disappear, then for the configured cooldown
(default 250 ms, minimum 100 ms). A retained Host/HostSession cannot overlap a
replacement socket: an Arc marker follows its actual transport I/O guard. A
stalled retirement ends the service rather than claiming cleanup succeeded.

Only explicit peer-local refusal classes can continue: idle/noise acquisition
limits, failed TLS/candidate binding, and positive peer-scope/membership refusal.
Host identity changes, unavailable LocalAPI, invalid configuration, stopped
credentials and lost ingress end hosting. The completion callback cannot override
a fatal result. Original application outcomes are delivered once BEFORE cleanup
waiting, so publication/effect receipts remain observable even when cleanup
fails. The callback can release its retained transport; it never authorizes
replay. Statistics count authentication/application handoff, not successful
media presentation or control.

Local revocation and cancellation are checked during active service, retirement
and cooldown using bounded timer registration, with no spawned task or alternate
runtime. Unpolled abandonment and caught callback panic fence the dedicated
supervisor and current peer before releasing their pending work. Independent
credentials and shared desktop source are not cancelled.

Ten focused tests execute actual TLS/UDP and Host/Viewer negotiation, including
local consent, sequential sessions on the same destination, unauthorized-then-
authorized peers, host identity changes, cooldown, retained transport cleanup,
idle timeout, ingress revocation, duplicate IDs, and panic/unpolled abandonment.
They use the existing root-credential Unix HTTP fixture and test CA in a fresh
user/network namespace; metadata and ingress-lifetime checks are synthetic.
They are not installed-Tailscale, nftables/TUN, hardware or simultaneous-client
qualification. Build evidence uses the pinned compiler, matched retained CI
dependencies and rebuilt first-party snapshot sources plus current host/accept
overlays. Unrelated host helper modules are omitted only in the disposable build;
there are no committed test exclusions beyond explicit namespace requirements.

This API does not itself install ingress restrictions, renew host certificates,
or enable the unfinished `frd run` CLI desktop dispatch path. Linux callers must
keep the real Boundary alive and supervised across ALL attempts, not manufacture
an always-true ingress assertion.

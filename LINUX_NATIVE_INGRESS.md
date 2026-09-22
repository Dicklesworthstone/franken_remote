# Linux native ingress and host ownership

`frd::native_connection::host::Server::bind_linux` joins the existing native
host-accept path to `fr_tailnet::ingress::Boundary`. It consumes the original
Server and returns a single-use `LinuxServer`, not a second transport stack.
The local administrator explicitly chooses the exact destination and kernel
TUN interface with `fr_tailnet::ingress::Configuration`.

## The enforced sequence

Credentials and installed LocalAPI node identity are checked first. The adapter
verifies that the selected address belongs to that node and the selected live
kernel TUN. It installs one atomic, exact-address/UDP-port **drop-only** nftables
rule, reads it back, and only then binds Asupersync's canonical native listener.
An IP address that merely resembles a tailnet address is not sufficient.

`LinuxServer::run` consumes that original socket at call time. It drives the
existing native TLS, fresh post-TLS membership checks and `Host` startup while
renewing the ingress evidence. No raw socket or caller-supplied boolean ingress
assertion is exposed by this path. The local callback still has to drive actual
session negotiation, any required consent, and the full admitted session. For
shared hosting, retain the existing `Admission::serve_host` future rather than
returning immediately after enqueuing a hub ticket.

The original transport retains the ingress lease through Host/HostSession/hub
handoffs. Failed renewal, changed node/interface, missing or changed rule,
expiry, cancellation, abandonment and a caught callback panic fence that
session. The application is not restarted and external effects are not retried.
Use a dedicated session Cx, distinct from the broker and credential-service Cx.

`LinuxServer::stop` fences at call time and closes an unused listener. It removes
only its owned rule, and refuses `InUse` while any handed-off transport retains
the lease. Cancellation is not proof of socket destruction. Keep the owner to
retry incomplete cleanup; Drop never silently removes firewall protection.
Setup failures can leave a restrictive `frd_*` table, never an unprotected
usable socket. At most eight existing prefixed tables are admitted by the local
setup preflight; this is not a cross-process administrator serialization lock.

## Scope and qualification

This is an opt-in privileged Linux kernel-TUN/nftables profile. The host OS/root,
installed daemon and locally selected TUN are trust boundaries. Existing
firewall policy is preserved: the rule neither flushes other tables nor accepts
traffic rejected by another chain. Rule/interface revalidation is periodic,
not a claim that untrusted root policy changes can be prevented atomically.
An unsupervised snapshot expires; call run promptly after bind.

The implementation and focused lifetime/refusal tests build against the pinned
compiler and retained matching dependencies. The privileged namespace example
`cargo run -p fr-tailnet --example qualify_linux_ingress` must only run in the
fresh network/mount namespace established by `verify-tailnet-ingress.yml`.
It checks real IPv4/IPv6 TUN/nftables behavior with synthetic LocalAPI metadata;
that is separate from installed-Tailscale qualification. This development
container cannot execute nftables/TUN qualification, so no kernel or live-tailnet
pass is asserted here.

This does **not** enable the unfinished `frd run` CLI dispatcher, implement
multi-client UDP demultiplexing, provide userspace-networking Tailscale ingress,
or establish media/hardware readiness. It replaces the missing protected-socket
ownership boundary; application consent, media readiness and input authority
remain independent requirements.

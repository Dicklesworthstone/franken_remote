# Linux native ingress and host ownership

`frd::native_connection::host::Server::bind_linux` joins the existing native
host-accept path to `fr_tailnet::ingress::Boundary`. It consumes the original
Server and returns a single-use `LinuxServer`, not a second transport stack.
The local administrator explicitly chooses the exact destination and kernel
TUN interface with `fr_tailnet::ingress::Configuration`.

## The enforced sequence

Credentials and installed LocalAPI node identity are checked first. The adapter
verifies that the selected address belongs to that node and the selected live
kernel TUN. It installs one atomic, exact-address/port **drop-only** nftables
transaction, reads it back, and only then binds Asupersync's canonical native listener.
An IP address that merely resembles a tailnet address is not sufficient.

`Configuration::protocols` selects the protocol set, a nonempty subset of
{udp, tcp} (default UDP). The transaction holds one
`ip daddr A <proto> dport P meta iif != IDX drop` rule per protocol, and the
read-back must show exactly one rule per requested protocol and nothing else.
`frd run` requests UDP, the only protocol it listens on (QUIC). HTTPS/WSS ingress
will add TCP once it exists; TCP enforcement is qualified below, but no TCP
listener is claimed.

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

## Unprivileged broker: the root `frd ingress-helper`

nftables administration needs `CAP_NET_ADMIN`, which a systemd **user** unit
does not have. `frd run` therefore calls `Enforcement::detect`. When the process
holds effective `CAP_NET_ADMIN` (read from `CapEff` in `/proc/self/status`, never
inferred from the uid), it keeps the direct path above. Otherwise it asks the root helper at
`/run/frankenremote-ingress/helper.sock` and keeps that connection as the lease.
Plan section 5.2 requires the privileged part to be a minimal broker with no
arbitrary privileged RPC. The helper's **entire** API is three requests:

| Request | Fields | The helper checks | Reply |
|---|---|---|---|
| install | interface, address, port, protocol set within {udp, tcp} | the uid is admitted; interface equals the configured one; port is not 0; address is not unspecified, loopback, multicast or v4-mapped; the configured interface is an UP kernel TUN; the address is assigned to it (kernel `ip -j address show`, not tentative or DAD-failed); this connection holds no rule | `installed(generation, ifindex, table, read-back)` |
| renew | generation | it is this connection's current generation; same interface index; address still assigned; its root `nft -j -n list table` read-back passes `validate_rule` | `renewed(generation, read-back)` |
| remove | generation | it is this connection's current generation; the table carries the helper prefix | `removed(generation)` |

No request carries nft text, a table name or an interface index. The helper
chooses the script, a random `frdh_<32 hex>` table (never the direct path's
`frd_` prefix) and the kernel's index for its configured interface. Any other
answer is a typed refusal, one of: `malformed_request`, `unsupported_version`,
`unknown_operation`, `oversized_request`, `peer_not_allowed`,
`capacity_exhausted`, `rate_limited`, `interface_not_configured`,
`interface_unqualified`, `address_not_assigned`, `invalid_address`,
`invalid_port`, `invalid_protocols`, `already_installed`, `not_installed`,
`stale_generation`, `foreign_table`, `firewall_failed`, `firewall_mismatch` or
`interface_changed`. Each generation is valid only on the connection that
installed it, so a previous generation can neither renew nor remove its
successor.

**Admission and bounds.** Before reading a byte, the helper checks every
connection with `SO_PEERCRED` against `allowed_uids` in the root-owned
`/etc/frankenremote/ingress-helper.json`, for example
`{"interface":"tailscale0","allowed_uids":[1000]}`. The optional fields are
`socket`, `nft` and `ip`. Unknown or duplicate fields, an empty or oversized uid
list, a non-root-owned file and non-root tools all refuse. Frames are fixed
binary: a request body is at most 64 bytes, a reply body at most 16 KiB + 128
bytes. There is one request in flight per connection; once a frame starts, all
of it must arrive within 1 s. The other limits:

- at most 8 connections;
- admitted-uid connections: a burst of 8, then 4 per second;
- requests per connection: a burst of 4, then 4 per second (renewal uses 2 per
  second);
- refusal log lines: a burst of 16, then 1 per second, with a suppressed count.

A codec violation is answered with its refusal and closes the connection. Logs
carry uids, generations and table names, never addresses, ports or command
output.

**Lifetime.** A rule lives exactly as long as its connection. The broker's
`Boundary` and every `Lease` clone hold a duplicate of the connection descriptor.
The helper therefore removes the rule only after the last lease is gone, or when
the broker process dies (the kernel closes the descriptors, including on
SIGKILL). `Boundary::stop` sends `remove(generation)` and requires the
acknowledgement. A lost connection is reported as `HelperUnavailable`, not as
verified cleanup. The client persists partial writes and reads, so a cancelled
renewal is completed and its reply discarded before the next request.

At start, the helper reclaims every `frdh_` table (the residue of a dead helper)
and never touches other tables. On SIGTERM it closes every connection, waits
2 s, which is longer than the brokers' 500 ms renewal period, so they fence
first, and then reclaims. A helper crash leaves its rules in place, which is
restrictive. Brokers see end-of-stream at their next renewal and fence. The
emitted unit's `RestartSec=2` orders a restarted helper's reclaim after that.

**Refusals in `frd run`.** `HelperUnavailable` (no socket, an unprotected path, a
non-root peer, or a lost connection) maps to `ingress_helper_unavailable`. Every
other ingress failure, including a typed helper refusal such as
`HelperRefused(AddressNotAssigned)`, maps to `ingress_unenforced`. These codes
apply whether the failure came at configuration, bind or renewal. The detail
text carries the typed reason. `frd run` never serves without a confirmed rule.

**What is trusted.** This is crash isolation plus a least-privilege split, **not
a sandbox**:

- The helper is root. It trusts the kernel, the root-owned `nft`/`ip` tools
  (verified protected), the configured TUN and its root-owned configuration.
- The broker authenticates the helper by a root-only socket path (every ancestor
  root-owned and not group- or other-writable) plus `SO_PEERCRED` uid 0.
- An unprivileged broker **cannot read the kernel ruleset**. `nft list` needs
  `CAP_NET_ADMIN`; the qualification observes `can_read_ruleset=false`. The
  broker runs the direct path's `validate_rule` on the helper's own read-back.
  That is trust in the helper's report, not independent kernel evidence.
  Independently of the helper, the broker re-reads the TUN index and address
  assignment (sysfs and unprivileged `ip`) and its LocalAPI node identity.
- The helper authenticates callers by uid alone. Any process running as an
  admitted uid can request rules. Every rule is drop-only, exact-destination,
  and only for an address currently on the configured interface. There is at
  most one rule (plus one uncertain residue) per connection and at most 8
  connections. Such a caller can deny non-tailnet traffic to a port. It cannot
  accept traffic, pass nft text, name tables or remove another connection's rule.
- When the helper crashes or restarts, or closes a connection after a protocol
  violation, a live broker's rule disappears before the broker fences. The
  broker notices at its next renewal, within 500 ms plus one exchange. This is
  the same periodic-revalidation limit as the direct path, not an atomic
  guarantee.

**Installation.** `frd install` for a user unit renders the helper's system unit
(`/etc/systemd/system/frd-ingress-helper.service`, with `RuntimeDirectory`
`frankenremote-ingress`, `CapabilityBoundingSet=CAP_NET_ADMIN`,
`NoNewPrivileges`, `ProtectSystem=strict` and `RestartSec=2`) and the config file.
It prints the steps that need root; an unprivileged install writes neither file.
A root service must not run a user-writable binary. When the installing `frd` is
not root-only, the unit runs `/usr/local/bin/frd`, and the steps copy the binary
there first. The helper itself refuses to start from a non-root-only executable
(`ingress_helper_untrusted_executable`). These unit directives have not been
exercised under a live systemd.

**Qualification.** Run the helper qualification with real nftables:

```sh
cargo build -p fr-tailnet --example qualify_ingress_helper
cargo build -p frd --bin frd
scripts/test_ingress_helper_namespace.sh target/debug/examples/qualify_ingress_helper target/debug/frd
```

The script runs as real root (`sudo unshare --mount --net`) in a private
namespace, with private `/run`, `/sys` and `/tmp`. There, a TUN named
`tailscale0` holds `100.64.0.1`, and a veth pair leads to a second namespace.
The real `frd ingress-helper` runs as root. The production `Boundary` with
`Enforcement::Helper` runs as uid 65534, which has no capabilities and cannot
read the ruleset. The script shows:

- UDP and TCP to the service port arriving over the veth are dropped;
- the same UDP and TCP arriving over the TUN pass (TCP answers with a SYN-ACK);
- an unrelated port is unaffected;
- uid 65533 is refused by `SO_PEERCRED`;
- a SIGKILL of the broker makes the helper remove the rule;
- `stop` removes the rule by acknowledged generation;
- over the real socket: codec, interface, port, address, stale-generation,
  connection-cap and accept-rate refusals;
- deleting the interface fences the broker;
- after a helper SIGKILL, the rule stays restrictive and is reclaimed at restart,
  while foreign tables are untouched;
- SIGTERM closes connections and reclaims tables.

The LocalAPI node metadata is synthetic. **This is not a live-tailnet test.** A
real `tailscale0` under `tailscaled`, a real second NIC on a real host, and the
emitted systemd unit remain unqualified.

## Scope and qualification

This is a Linux kernel-TUN/nftables profile, administered either by a root or
`CAP_NET_ADMIN` process directly, or by the root helper above. The host OS/root,
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

This does **not** by itself run a host (`frd run` composes it in `crates/frd/src/host_run.rs`), implement
multi-client UDP demultiplexing, provide userspace-networking Tailscale ingress,
or establish media/hardware readiness. It replaces the missing protected-socket
ownership boundary; application consent, media readiness and input authority
remain independent requirements.

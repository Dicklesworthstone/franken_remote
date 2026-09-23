# Persistent protected Linux hosting (capacity one)

`LinuxServer::serve_serial` connects `Server::bind_linux` to the existing serial
native acceptor. The original enforced ingress boundary stays owned and serviced
across every attempt: idle acquisition, TLS, post-TLS membership authorization,
application service, original-transport retirement, cooldown, and rebinding.
It does not expose a socket or accept a caller's boolean assertion of protection.

Provision and service the original host credentials, optionally select a live
policy monitor with `Server::with_live_policy`, then bind the explicit local
kernel-TUN configuration through `bind_linux`. Pass `serve_serial` a dedicated
supervisor context, the same runtime's handle, a bounded serial policy, and the
existing `serial::Application`. The broker/ingress, credentials, and OS desktop
source have independent contexts. No new runtime or task is created here.

This profile is **capacity one**. The application allocates fresh local session
and connection identifiers on each attempt and keeps its peer service alive
for the whole admitted session. The existing serial implementation decides which
peer refusals can continue. Local identity, credential, unhealthy live-policy and
ingress evidence end hosting. A healthy policy revision retires the old peer and
allows only a NEW attempt after its transport is destroyed and cooldown completes;
new approval requirements apply. No completed operation is replayed or granted a
new authority deadline. Concurrent QUIC connections are not supported by this API.

The first listener is consumed when the service is constructed. Dropping even an
unpolled service fences its supervisor and ingress lease. Any active peer is
fenced before pending work is dropped, including after a caught callback panic.
A normal peer's departure does not retire the boundary needed by the next peer.
The original transport must actually be destroyed before another socket binds;
its cancellation alone is insufficient. Authentic application outcomes remain
available through the existing completion callback even when retirement fails.

Keep the `LinuxServer` after completion and call `stop` with an independent
cleanup context. The owned firewall rule remains in place until explicit cleanup
succeeds. Cleanup refuses `InUse` while any handed-off original transport retains
an ingress lease. An uncertain cleanup leaves a restrictive rule, not an asserted
successful teardown. Neither source cleanup nor credential cleanup is implied.

Both single-peer and persistent services eagerly destroy pending native work
once they return a result, after fencing the dedicated hosting context. Retaining
that completed future does not retain its operation or pending ingress leases.
The same ordering holds for an unpolled drop and a caught application panic.
Polling an already spent service refuses rather than polling original work again.
An escaped transport is still independently owned and continues to prevent rule
removal; eager local retirement does not assert that every external owner is gone.

## Focused lifecycle verification

Build the integration test with the repository's pinned compiler:

```sh
cargo test -p frd --test native_host_linux_serial --no-run --locked
scripts/test_linux_serial_lifecycle.sh /absolute/path/to/native_host_linux_serial-TEST_HASH
```

The runner uses a fresh user/mount/network namespace and executes all eight
explicitly ignored tests. It mounts synthetic `/sys` and `/run` only inside that
namespace. IPv4 UDP/TLS, Host/Viewer negotiation, Unix peer-credential HTTP, the
protected policy store, command subprocesses, ingress renewal, and serial owners
are real. **The selected interface and nftables replies are explicit fixtures:**
these tests do not filter real packets or establish kernel-ingress qualification.
They check successor sessions across evidence renewal, fixed idle deadlines,
rule loss during cooldown, retained transport, policy revision changes,
cancellation, panic and unpolled abandonment without weakening production checks.

Actual TUN/nftables behavior still requires the separate
`qualify_linux_ingress` executable and installed-tailnet testing. These tests do
not claim actual desktop media, full newer-main workspace qualification, or a
completed `frd run` integration. Simultaneous multi-client UDP dispatch and CLI
wiring remain separate unfinished work.

# Persistent protected Linux hosting (capacity one)

`LinuxServer::serve_serial` connects `Server::bind_linux` to the existing serial
native acceptor. The original enforced ingress boundary stays owned and serviced
across every attempt: idle acquisition, TLS, post-TLS membership authorization,
application service, original-transport retirement, cooldown, and rebinding.
It does not expose a socket or accept a caller's boolean assertion of protection.

`Server::bind_linux_addresses` extends the same owner to a fixed set of at most
eight exact current node addresses. `frd run` supplies the installed node's IPv4
and IPv6 addresses and installs a separate enforced ingress boundary for each.
The first valid Initial selects one original socket through fair polling, under
one aggregate datagram limit and one fixed acquisition deadline. Unselected
sockets close before TLS; the selected local address is rechecked against the
original bind set, ingress protection and fresh identity evidence. After the
peer's original transport retires, the full original set binds again.

Setup anchors every address's non-cloneable node snapshot to the first snapshot
with `LocalApi::revalidate_node` before installing any rule. Matching IPs cannot
silently combine different node keys, daemon processes, certificate names or
tailnets; the first snapshot's original validity must still hold. Each boundary
then performs its ordinary installation and continuing identity checks.

All original boundaries keep renewing while that one session runs. Losing any
boundary fences the service; cancellation and completion fence every boundary
before application resources are released. Cleanup attempts every installed
rule and retains failed removals for explicit retry. This remains the same
capacity-one service and desktop source owner.
If setup fails, a failure to remove any known installed rule is returned as an
ingress error instead of being hidden by the earlier bind/setup error. An
uncertain installation retains the existing restrictive-residue behavior.

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

Build the integration test and the shipped client with the repository's pinned
compiler, then run every ignored test in a fresh namespace:

```sh
cargo build -p fr-native --features linux-desktop,linux-displays --bin fr --bin fr-media-worker --locked
cargo test -p frd --test native_host_linux_serial --no-run --locked
scripts/test_linux_serial_lifecycle.sh /absolute/path/to/native_host_linux_serial-TEST_HASH
```

`shipped_client` and `real_media` find `fr` and `fr-media-worker` in the same
cargo profile directory as the test executable; `real_media` also needs Xvfb and
`xsetroot` and runs the real capture/encode/decode/present path. Hosts that restrict unprivileged user namespaces (Ubuntu's AppArmor
default) can run the same fresh mount/network namespaces as real root with
`FR_NS_SUDO=1`.

The namespace loopback carries the live tailnet path: MTU 1280 (tailscale0) and
an nftables rule in the fresh namespace that drops IP fragments, as Tailscale
does, so a QUIC datagram above 1252 bytes is lost rather than reassembled. A
host or client whose datagrams outgrow the tailnet MTU therefore fails here
instead of only on a real tailnet. The runner refuses (exit 2) without `nft`.

The runner uses a fresh user/mount/network namespace and executes every
explicitly ignored test. It mounts synthetic `/sys` and `/run` only inside that
namespace. IPv4/IPv6 UDP/TLS, Host/Viewer negotiation, Unix peer-credential HTTP, the
protected policy store, command subprocesses, ingress renewal, and serial owners
are real. **The selected interface and nftables replies are explicit fixtures:**
these tests do not filter real packets or establish kernel-ingress qualification.
They check successor sessions across evidence renewal, fixed idle deadlines,
rule loss during cooldown, retained transport, policy revision changes,
cancellation, panic and unpolled abandonment without weakening production checks.

The dual-stack regression cases additionally exercise cross-family serial
sessions past the first evidence deadline, either family's protection failing,
partial bind cleanup, and the shipped controlling client over IPv6 while the
host also listens on IPv4. The control case checks real HEVC presentation and
independently observed X11 pointer/key effects. These are namespace acceptance
cases; their presence does not claim live-tailnet IPv6 qualification.

Additional dual-stack lifetime regressions retain an IPv6 transport after its
session ends and require both rules to refuse removal until the transport is
destroyed. Unpolled abandonment and a caught request callback panic must release
both sockets while preserving the independent credential context. These newly
added regressions also change the installed identity without changing its IPs,
and fail one partial-setup cleanup while requiring the other rule to be removed.
They have not yet been executed in this session; their definitions do not
establish a passing runtime result. The initial build stopped with no space
left on the shared filesystem; the retry's pinned Asupersync compilation was
killed by SIGKILL before application type checking.

`host_run` drives the production `frd run` composition (`crates/frd/src/host_run.rs`)
with a scripted capture worker: a real viewer receives frames for four seconds past
the initial authority lease, then stop runs the fixed cleanup order.
`shipped_client` runs the real `fr displays` binary against it, serves sequential
viewers and departing inspections without counting them as host failures, and
expires a connected viewer that stops answering renewal.

This suite runs as (namespace) root, so `bind_linux` uses direct enforcement
with the fixture tools. `frd run` without `CAP_NET_ADMIN` instead selects the root
`frd ingress-helper` (`Enforcement::detect`). That unprivileged path is qualified
with real nftables, a TUN and a veth pair by
`scripts/test_ingress_helper_namespace.sh`. It runs the production `Boundary` as
an unprivileged uid against the real helper role, and covers `SO_PEERCRED`
refusal, removal on broker SIGKILL and helper-crash reclaim (see
[LINUX_NATIVE_INGRESS.md](LINUX_NATIVE_INGRESS.md)).

Actual TUN/nftables behavior still requires the separate
`qualify_linux_ingress` and `qualify_ingress_helper` executables and
installed-tailnet testing. These tests do
not claim actual desktop media, hardware encode or a live-tailnet run.
Simultaneous multi-client UDP dispatch remains separate unfinished work.

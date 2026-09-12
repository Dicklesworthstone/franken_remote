# Native outbound connection

`fr_tailnet::NativeClient` connects one installed-daemon-selected `PeerTarget`
through the existing Asupersync QUIC implementation. It binds an exact current
local node address and dials an exact remote node-owned address; it performs no
DNS lookup, family race, public fallback, application pairing, or transport
substitution. The local application supplies its trusted certificate roots and
chooses one address pair and port from the checked metadata.

The native TLS configuration requires TLS 1.3, `fr-remote/0`, strict WebPKI chain
and hostname validation, and no early data. It deliberately does not use the
upstream convenience helper's exact-leaf fallback. A peer certificate is not a
trust-store bootstrap. Root count and total bytes, handshake duration, stream
windows, datagram size, and concurrent attempts are bounded.

## Ownership and handoff

`dial` takes ownership of the target and claims the single shared attempt slot
before its future is polled. The timeout begins at that call. Clones share that
slot; dropping an unpolled or pending attempt releases it and cannot resume or
replay the connection. Connection IDs come from kernel randomness.

The connector revalidates the original stable node identity before binding and
concurrently with TLS negotiation. A lost handshake flight does not suspend
metadata renewal. Changes to node keys, name, local daemon, tailnet or address
ownership terminate the attempt. A fresh final revalidation is mandatory after
TLS and before returning a `ConnectedPeer`; completed TLS alone cannot freeze an
old metadata snapshot. Every lookup and native handshake remains inside the
original overall deadline.

`ConnectedPeer::into_connection` consumes that owner, rechecks its current
metadata deadline and actual socket tuple, and returns the original established
`NativeQuicUdpConnection` for viewer startup. A parked result that expires refuses
and drops the socket. Dialing sends no desktop application records and grants no
observation or control permission. Ongoing session and path authority after
handoff remain the enclosing session's responsibility.

This is **not a protected host listener**. Binding an address does not prove that
packets arrived over Tailscale's tunnel. The host still needs independently
qualified ingress enforcement, client admission, optional local approval and its
existing session/grant lifecycle before exposing desktop content. The current
pinned native endpoint's public bind API does not supply that ingress proof.

## Verification

The new tests exercise real credential-checked Unix sockets and HTTP metadata,
strict TLS trust, actual Asupersync QUIC over UDP, loss of the first handshake
flight, delayed handshake completion with concurrent revalidation, IPv4/IPv6,
wrong-host certificates, identity reassignment, timeout/cancellation, exclusive
attempt ownership and expired handoff. Certificates and metadata are synthetic.
The five network tests require an isolated Linux network namespace containing
fixture node addresses; these are not a real Tailscale installation, protected
TUN ingress, WAN performance or hardware-codec qualification.

```sh
cargo test -p fr-tailnet --locked
./scripts/verify-native-dial.sh
```

The second command builds the unit-test executable and runs only the five
explicit network cases inside a new user/network namespace. Namespace creation
failure is a visible qualification failure, never permission to modify the
caller's network. Native tests do not silently skip when explicitly selected.

Local verification of this increment: 59 ordinary tests and all five explicit
network tests pass; strict library/test Clippy, workspace formatting and
documentation checks are separate gates. First-party sources were rebuilt using
the exact pinned compiler against matching retained dependency libraries. A cold
Cargo dependency build was attempted but Asupersync compilation was killed at the
container's memory limit; it is not a passing full-workspace or CI result.

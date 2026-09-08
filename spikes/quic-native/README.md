# Phase 0 spike: live Asupersync QUIC endpoint qualification

Bead **fr-p0-quic-native-aqy** · plan §4.1, §12.1, §12.4, §23 Phase 0.

This standalone harness qualifies the documented live composition
(`QuicUdpEndpoint` + `QuicHandshakeDriver` + `NativeQuicUdpConnection`) over
real UDP sockets: handshake and TLS enforcement, stream integrity, RFC 9221
datagrams, deterministic loss/reorder injection, cancellation, idle CPU, and
interoperability against an independent peer (quinn 0.11) in both directions,
plus a cross-machine pair over a real tailnet direct path.

**The verdict and full evidence table live in [`RESULTS.md`](RESULTS.md).**
Raw scenario output is retained under `results/`.

## Layout

- `src/main.rs` — scenario dispatch; one scenario per process invocation.
- `src/asup.rs` — the composition under test (nothing manually advanced).
- `src/quinn_peer.rs` — the independent QUIC peer (test-only, never shipped).
- `src/proxy.rs` — deterministic seeded loss/reorder middlebox + Initial-DCID
  sniffer (the single-connection accept API needs the DCID out of band).
- `src/certs.rs` — runtime rcgen PKI; real WebPKI verification, no skip-verify.
- `run_all.sh` — reproducible local driver.

## Running

```bash
./run_all.sh                       # all local scenarios
# Cross-machine pair (host A serves, host B connects):
quic-native-spike emit-pki /tmp/spike-pki       # then copy dir to both hosts
quic-native-spike serve 0.0.0.0:47777 /tmp/spike-pki          # host A
quic-native-spike connect-remote <A>:47777 /tmp/spike-pki     # host B
```

Builds assume the FrankenSuite checkouts are siblings (`../../../asupersync`);
the qualified asupersync revision is pinned in RESULTS.md. On this fleet, use
rch offload (see AGENTS.md §10); the `fetch_bin.sh` helper builds remotely and
returns the binary via a job result dir.

## No-claims boundary

This spike measures the caller-driven composition (explicit capability context,
as upstream's own live-UDP proof does). It does not claim: full-runtime
scheduler behavior, managed multi-connection listener qualification, DERP-relay
behavior, or anything about WebTransport/H3 — those are separate rows/beads.

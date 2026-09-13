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

The current manifest selects released Asupersync 0.5.0 at
`78b64636e99fea4ea2d868096576021dd3b8e519`. This candidate has not yet been
executed by this harness. The historical results retain their original source
identities and NO-GO verdict; they do not qualify the new pin.

## Layout

- `src/main.rs` — scenario dispatch; one scenario per process invocation.
- `src/asup.rs` — the composition under test (nothing manually advanced).
- `src/quinn_peer.rs` — the independent QUIC peer (test-only, never shipped).
- `src/proxy.rs` — deterministic seeded loss/reorder middlebox + Initial-DCID
  sniffer (the single-connection accept API needs the DCID out of band).
- `src/certs.rs` — runtime rcgen PKI; real WebPKI verification, no skip-verify.
- `run_all.sh` — runs a prebuilt binary, retains every scenario exit, and
  returns nonzero if any scenario fails. It refuses an existing results path.

## Running

From the repository root, compile and test through RCH. The spike is a separate
workspace: root workspace tests do not execute its receive-window regression.

```bash
RCH_REQUIRE_REMOTE=1 rch exec -- cargo test --locked -j 2 --manifest-path spikes/quic-native/Cargo.toml
bash spikes/quic-native/fetch_bin.sh
bash spikes/quic-native/run_all.sh spikes/quic-native/out/quic-native-spike spikes/quic-native/results/new-run
```

Choose a new results directory for each run. The runner executes the RCH-built
release binary locally; it performs no compilation. Keep its input binary fixed
through the entire run: it checks the digest between scenarios and at completion
and refuses mixed-build evidence. Retain the RCH build log with its source
revision and dependency identity alongside the scenario logs. Debug-build
timings do not replace the original release-build measurements.

```bash
# Cross-machine pair (host A serves, host B connects):
quic-native-spike emit-pki /tmp/spike-pki       # then copy dir to both hosts
quic-native-spike serve <A-tailnet-IP>:47777 /tmp/spike-pki          # host A
quic-native-spike connect-remote <A-tailnet-IP>:47777 /tmp/spike-pki # host B
```

The manifest and lockfile pin an immutable Asupersync candidate. A pin is not a
qualification result: read `RESULTS.md` for measured rows and remaining failures.
The root workspace retains its separate published runtime pin. `fetch_bin.sh`
uses strict RCH to build the release binary and copies the returned artifact
into this spike's `out/` directory.

The receive-window test sends four 1024-byte payloads over one real TLS/UDP
stream with a fixed 1024-byte receive window. Each application read must
replenish both advertised and enforced credit; reconfiguring a larger window
cannot hide a stalled transfer.

The TLS-negative scenario retains the client certificate-verification result,
requests server cancellation, and awaits server completion. Cancellation is
cleanup; only the client's fatal TLS alert satisfies the refusal assertion.

## No-claims boundary

This spike measures the caller-driven composition (explicit capability context,
as upstream's own live-UDP proof does). It does not claim: full-runtime
scheduler behavior, managed multi-connection listener qualification, DERP-relay
behavior, or anything about WebTransport/H3 — those are separate rows/beads.

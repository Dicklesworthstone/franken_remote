# fr-p0-quic-native-aqy — Live Asupersync QUIC Endpoint Qualification: Results

**Verdict: NO-GO for external QUIC interoperability. CONDITIONAL for the
asupersync↔asupersync native profile, with named upstream defects that must be
fixed (and re-qualified) before the plan's native transport profile can be
enabled.** Details, evidence, and the exact upstream work items are below.

Evidence category for everything in this file: **live-wire measurements on the
hardware identified below**, produced by the harness in this directory. Nothing
here is simulated, mocked, or manually advanced; every byte crossed a real UDP
socket. Raw per-scenario output is retained under `results/`.

## Qualified composition

The one documented live composition chosen per the bead:

`QuicUdpEndpoint` (real UDP through asupersync's reactor surfaces)
→ `QuicHandshakeDriver` (`client_config`/`server_config`, rustls TLS 1.3, real
WebPKI verification, ALPN required)
→ `NativeQuicUdpConnection::connect`/`accept` (authenticated 1-RTT data plane,
caller-driven `drive_io_once`/`flush`).

Driven with an explicit capability context (`Cx::for_testing`, via asupersync's
documented `test-internals` dev opt-in) — the same way upstream's own live-UDP
integration proof drives this composition. Qualification under the full runtime
scheduler is follow-up work; rows likely affected are marked.

## Environment identity

| Item | Value |
|---|---|
| Date | 2026-09-08 |
| Host (client side) | Linux 7.0.0-30-generic x86_64 (`sensedemobox`, tailnet 100.100.118.85) |
| Remote host (tailnet pair) | `omarchy`, tailnet 100.80.46.93, x86_64, direct WireGuard path (~29 ms RTT, verified via `tailscale ping`: `via 173.56.62.32:3544`, not DERP) |
| Toolchain | rustc 1.100.0-nightly (908501772 2026-08-30), pin `nightly-2026-08-31` |
| asupersync | path dep `/data/projects/asupersync`, git `02df0613e3f770984ac0ce4feed9eedb40805e6e` (2026-09-08), version 0.4.11, features `tls`, `test-internals` |
| Independent peer | quinn 0.11.11 / quinn-proto 0.11.16 (tokio runtime, test-only) |
| TLS | rustls 0.23.44 (ring), rcgen 0.13.2 runtime-generated CA/leaf PKI |

Reproduce: `./run_all.sh` in this directory (builds via rch offload or local
cargo), plus the cross-machine pair documented in the main.rs `serve` /
`connect-remote` comments.

## Result matrix

| # | Row | Status | Evidence |
|---|---|---|---|
| 1 | Real-UDP handshake, self pair (loopback) | **passed** | Established in 1 ms; ALPN `fr-spike/0` both sides; TLS 1.3 via rustls; CIDs bound. `results/self-pair.log` |
| 2 | TLS refuses untrusted CA | **passed** | Real fatal alert (`read_hs_fatal_alert`); no skip-verify path. `results/tls-negative.log` |
| 3 | TLS refuses wrong hostname | **passed** | Same mechanism; WebPKI verification is live. |
| 4 | 4 MiB bidirectional stream integrity | **passed** | Checksummed echo, 153 ms, 0 lost, RTT 703 µs, cwnd grew to ~4.3 MB. |
| 5 | Handshake + stream recovery through 3% loss / 2% reorder | **passed** | Deterministic middlebox (seeded); 1 MiB echo intact; 108 packets declared lost, recovered; `results/loss.log` |
| 6 | Cancellation is prompt and fenced | **passed** | Post-cancel `drive_io_once` refused in 1.3 µs; post-cancel sends refused with typed errors. |
| 7 | RFC 9221 datagrams (payload ≤ 1150 B) | **passed** | Delivered intact on loopback and across the tailnet (4/4 echo). |
| 8 | Datagram admission vs wire bound | **FAILED (upstream defect D1)** | `send_datagram` admits 1180 B, packet assembly then produces a 1212 B protected packet > 1200 cap and the **connection dies with a fatal error** instead of a typed send-time refusal. Measured boundary: admitted ≤1180, deliverable ≤1150. `results/self-pair.log` |
| 9 | Idle receive suspends (no busy-poll) | **FAILED (upstream defect D2)** | 6 s silent hold burned 6 s CPU — 99% of one core inside `drive_io_once`'s bounded receive wait. Fatal to the plan's <0.1%-core idle daemon target in this composition. Full-runtime behavior untested. `results/idle-cpu.log` |
| 10 | Interop: asupersync client → quinn server | **FAILED (upstream defect D3)** | Client Initial datagram is **288 bytes** (RFC 9000 §14.1 requires ≥1200); quinn silently drops it; 64 identical 288 B retransmits captured on the wire, zero server responses, then `client_handshake_incomplete`. `results/interop-quinn-server.log` |
| 11 | Interop: quinn client → asupersync server | **FAILED (upstream defect D4)** | Server rejects quinn's RFC-compliant Initial at header parse (`packet_header_decode`): `decode_long_header` validates reserved bits and reads pn-length from the first byte **before header-protection removal** (RFC 9001 encrypts those bits), so every compliant peer's packet is "malformed". `results/interop-quinn-client.log` |
| 12 | Coalesced-datagram handling | **FAILED by inspection (upstream defect D5, latent behind D3/D4)** | `recv_handshake_packet` treats one UDP datagram as exactly one packet with the AEAD tag at datagram end; RFC 9000 §12.2 coalesced flights (quinn default) would fail AEAD and are then misclassified as "stale" and retried (`is_stale_handshake_packet_error` matches `packet_unprotect`), burning the flight budget. |
| 13 | Cross-machine pair over real tailnet (direct path) | **passed with defect D6** | Handshake 30 ms over ~29 ms RTT; 1 MiB integrity ok; 4/4 datagrams. **But**: 14.3 s for 1 MiB (~0.6 Mbps), 736/1032 packets declared lost (~71%) on a healthy direct link, cwnd collapsed to 2400 B — the loss detector / ACK timing misfires at real-network RTT; loopback RTT had masked it. `results/tailnet-pair-*.log` |
| 14 | Flow-control credit model | **passed, documented behavior** | Stream/connection receive credit is fully caller-driven (`configure_stream_receive_window`, `advertise_connection_receive_limit`); without explicit replenishment transfer stalls at the initial 1 MiB stream window, and `write_stream` returns a typed "flow control exhausted" backpressure error. Consumers must build a credit protocol; nothing is automatic. |
| 15 | Multi-connection listener | **not tested** | `ManagedQuicEndpoint::begin_authenticated_accept` requires the client's source address **and** Initial DCID configured before admission — there is no cold accept. The single-connection `accept` was qualified with the DCID supplied out of band (sniffed by a middlebox in the quinn test). Managed multi-client routing under concurrent load remains untested. |
| 16 | Forced-DERP relay path | **blocked** | Requires forcing the tailnet pair off its direct path (firewall/config changes to shared infrastructure this session is not authorized to make). Direct path tested (row 13). |
| 17 | 0-RTT | **not applicable** | Not exercised; the plan forbids admitting application operations in 0-RTT. Upstream module claims no 0-RTT support. |

## Upstream work items (bounded, countable against the 15k transport allowance)

- **D1** — bound `send_datagram` admission by the *protected packet* budget
  (payload + frame header + AEAD overhead vs max packet size), and make an
  over-budget datagram a typed refusal, never a connection-fatal assembly error.
- **D2** — make the bounded receive wait suspend through the reactor (or
  document + qualify a full-runtime composition that does); a caller-driven
  drive that burns a core cannot host the idle daemon.
- **D3** — pad client Initial datagrams to ≥1200 bytes (RFC 9000 §14.1).
- **D4** — remove header protection before interpreting reserved/pn-length bits
  (RFC 9001 §5.4); parse by the long-header Length field, not datagram extent.
- **D5** — split coalesced datagrams into individual packets (RFC 9000 §12.2)
  and stop classifying AEAD failures as retryable "stale" traffic.
- **D6** — investigate loss detection/ACK timing at ≥20 ms RTT: ~71% spurious
  loss and cwnd collapse on a clean direct tailnet path.

## Consequences for the plan (§4.1, §12.1, §23)

1. **The native asupersync↔asupersync profile is viable in shape** — real
   handshake, verified TLS, integrity under injected loss, prompt cancellation,
   typed backpressure — but is **not enableable** until D1/D2/D6 are fixed and
   re-measured: D2 violates the idle objective, D6 destroys WAN throughput, D1
   is a remote-triggerable connection kill.
2. **External interoperability does not exist today** (D3/D4/D5 are three
   independent protocol-level breaks). Per the bead's failure meaning: keep the
   unqualified path disabled; the browser/WebTransport gate
   (`fr-p0-webtransport-b9u`) **cannot pass against this stack** until they are
   fixed — a real browser is a compliant peer and will behave exactly like
   quinn did. No second QUIC stack; the work items above are the path.
3. The **WSS compatibility profile** must be treated as the qualification
   priority for anything that needs to ship before the upstream items land.
4. Protocol drafting note (PROTOCOL.md): measured safe QUIC DATAGRAM payload on
   this composition is **≤1150 bytes** under a 1200 B packet budget; the plan's
   1280-MTU arithmetic must subtract real framing+AEAD overhead, not assume
   1200 B of payload.

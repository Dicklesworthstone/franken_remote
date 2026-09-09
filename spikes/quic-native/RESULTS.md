# fr-p0-quic-native-aqy — Live Asupersync QUIC Endpoint Qualification: Results

## 2026-09-09 candidate requalification (`fr-5nb`)

**Verdict: NO-GO for independent interoperability. `fr-5nb` remains open.**
The spike pins Asupersync `352b3695d3cb84a6296bb307a7880a6c9fafc2f8`;
the shipping workspace remains on published `0.4.10`. Source fixes and the
passing rows below do not qualify or enable a shipping native profile.

The final release binary is SHA-256
`85be3bd92b2b17a2201d160f3f7d56b040318ca65d256a1da303d0e99178f9f5`,
compiled through RCH on `vmi1149989` with `nightly-2026-08-31` and `-j 2`.
[RCH build receipt](results/candidate-352b3695d-cid-20260909/rch-build.log)
records base `421184b11f5e54f7a179bb31f91786ce5d012d98` plus the explicit
spike source overlay. The [environment record](results/candidate-352b3695d-cid-20260909/environment.txt)
contains source/manifest/lock digests. The independent peer is Quinn **0.11.11**
with **quinn-proto 0.11.17**, rustls 0.23.44, and rcgen 0.13.2; Quinn's Tokio
runtime is confined to this standalone test harness.

These are real UDP/TLS experiments using the caller-driven `Cx::for_testing`
composition. Builds/checks/tests ran through strict RCH; the prebuilt binary
ran on `sensedemobox`. RCH job mode refused the two-slot worker because it
estimated the serial runner at 16 cores. No build fell back locally. The
second endpoint was `omarchy` (Linux 6.19.8, glibc 2.44); it ran the identical
binary. The direct tailnet path was observed before and after that transfer.
This is not Tailscale identity/admission or full-runtime qualification.

| Original row / scope | Status | Final evidence |
|---|---|---|
| 1, 4: native loopback handshake and 4 MiB echo | **passed** | 194 ms, checksum matched, 0 declared losses. [self-pair](results/candidate-352b3695d-cid-20260909/self-pair.log) |
| 2–3: untrusted CA and wrong hostname | **passed** | Both retain the client `read_hs_fatal_alert` result; peer cancellation is awaited as cleanup. [TLS](results/candidate-352b3695d-cid-20260909/tls-negative.log) |
| 5: 3% loss / 2% reorder experiment | **passed** | 1 MiB intact; 123 ms; 40/25 proxy drops and 31 reorder events. Seeded loss decisions depend on the actual packet schedule. [loss](results/candidate-352b3695d-cid-20260909/loss.log) |
| 6: cancellation | **passed** | Drive refused in 2.265 microseconds; post-cancel send refused. [cancel](results/candidate-352b3695d-cid-20260909/cancel.log) |
| 8 / D1: datagram admission bound | **passed** | All 6 admitted probes delivered intact; largest accepted/delivered 1150 B, first refusal 1180 B, no connection-fatal admitted size. [self-pair](results/candidate-352b3695d-cid-20260909/self-pair.log) |
| 9 / D2: gross idle busy-poll regression | **passed** | 6001 ms wall, 12 wakeups, no measured CPU ticks. The 10 ms process-counter resolution does **not** establish the daemon's <0.1%-core objective or literal zero CPU. [idle](results/candidate-352b3695d-cid-20260909/idle-cpu.log) |
| 10 / D3: native client → Quinn server exchange | **failed** | Initial is now 1200 B and Quinn reports a completed TLS handshake, but the native client fails with `expected_long_header`; no stream exchange completes. [independent server](results/candidate-352b3695d-cid-20260909/interop-quinn-server.log) |
| 11 / D4: Quinn client → native server exchange | **failed** | Native server completes the handshake, then fails on frame 24 (`NEW_CONNECTION_ID`, 0x18). [independent client](results/candidate-352b3695d-cid-20260909/interop-quinn-client.log) |
| 12 / D5: complete coalesced-flight interoperability | **blocked** | No successful independent exchange satisfies the observer row. A visible-header parser test is not packet-authentication evidence. |
| 13 / D6: native pair over a direct tailnet path | **passed** | 59 ms handshake; 1 MiB intact in 422 ms; 0 lost / 1020 acked, 0 PTO, smoothed RTT 61.447 ms, cwnd 1,099,405 B. [client](results/candidate-352b3695d-cid-20260909/tailnet-client.log), [server](results/candidate-352b3695d-cid-20260909/tailnet-server.log) |

The final seven-scenario [runner exits](results/candidate-352b3695d-cid-20260909/exits.txt)
are 0 for the first five scenarios and 1 for both independent exchanges;
the runner exits **1**. The two-machine [process exits](results/candidate-352b3695d-cid-20260909/tailnet-exits.txt)
are both 0. Its 4/4 DATAGRAM result is **count-only**; the cross-machine byte
integrity assertion applies to the stream. Self-pair and independent-peer
DATAGRAM assertions require exact payloads, but the independent DATAGRAM rows
were not reached. The 422 ms result is one transfer, not a throughput benchmark
or latency distribution. Path probes were 174 ms before and 35 ms after;
Tailscale overhead was not separately measured.

### Receive-credit regression and harness corrections

The regression configures a 1024-byte receive window once and sends four
1024-byte payloads over one real TLS/UDP stream. Registry `0.4.10` failed on
the next window with
`Transport(Stream(Flow(Exhausted { attempted: 1024, remaining: 0 })))`:
[RCH j-30012848524493037](results/candidate-352b3695d-cid-20260909/registry-window-failed.log),
0 passed / 1 failed, with the registry manifest/lock retained beside that log.
The candidate passes the unchanged regression. The final suite runs three
tests: that transfer, visible-header parsing, and role-specific CID parameter
encoding. The latter two use explicit parser/wire fixtures, not live-peer proof.

The old TLS-negative fixture waited for both handshake halves after the client
had already refused, exhausting the runner's 180-second budget. The fixed
fixture retains the client result, requests server cancellation, and awaits
server completion; no timeout or TLS assertion was weakened. The first
[exploratory run](results/candidate-352b3695d-20260909/NOTES.txt) is retained as
negative history, not single-build qualification: RCH also replaced its input
binary between scenarios. Later runs use fixed copies; the runner now verifies
the binary digest between scenarios and at completion.

The first fixed-binary run exposed **CID authentication failure** from both
Quinn roles: [server report](results/candidate-352b3695d-final-20260909/interop-quinn-server.log),
[client report](results/candidate-352b3695d-final-20260909/interop-quinn-client.log).
The spike caller had omitted `initial_source_connection_id` and the server's
`original_destination_connection_id`. It now supplies the actual bounded CIDs
through the existing raw-parameter API, as required by
[RFC 9000 section 7.3](https://www.rfc-editor.org/rfc/rfc9000.html#section-7.3).
This corrects outbound configuration; it does not implement missing upstream
receiver-side CID authentication.

### Remaining upstream work

At the pinned candidate, source review identified three concrete gaps:

- The client handshake checks completion before a receive batch, then routes
  all datagrams through a long-header-only parser. A standalone early 1-RTT
  packet can therefore fail with `expected_long_header`. The observed `s54`
  datagram being that packet is an inference; the wire log records direction
  and length only. Repair needs bounded retention and authenticated data-plane
  handoff, including coalesced short-header tails, rather than discarding data
  or suppressing the parse error.
- The native frame parser does not handle `NEW_CONNECTION_ID` / `RETIRE_CONNECTION_ID`
  (0x18 / 0x19). The live Quinn-client run reaches and fails on 0x18. Proper
  frame validation and bounded CID state are upstream work; disabling normal
  peer behavior would not satisfy the independent gate.
- The single-connection path decodes peer parameters but does not require and
  compare their CID values. Managed admission checks mismatches when present
  but permits absence. Supplying correct local parameters does not close this
  mandatory peer-authentication requirement.

The original receive-credit correction is `ccb5af622`; D1/D3 source work is
`e2d2d4300`, D2 is `616af91b5` and follow-ups, and D4/D5/D6 work includes
`c72aae490`, `22429c667`, and `50a6f7c84`. These changes did not complete the
independent gate. `git show --numstat <commit> -- '*.rs'` across these 18
candidate ancestors totals **5,875 additions / 535 deletions**:
`ccb5af622 e2d2d4300 616af91b5 c72aae490 22429c667 50a6f7c84 8739ef0cd
ec396e774 8e995b17d c0c873793 4bd9bfe1b 391ab06c3 23c7f2d02 2bc6e1adf
34d7af44d a3652f1ad df00cc149 f6ed27fd3`.
This is conservative gross Rust churn, including tests/comments/replacement
lines and compound-commit hunks, not maintained LOC or certification of the
entire 15k transport allowance. No upstream source was edited in this slice.

### Verification boundary

| Check | Result | Retained evidence |
|---|---|---|
| Spike check / Clippy with `-D warnings` / tests | **passed**; 3 tests, 0 failures | [check](results/candidate-352b3695d-cid-20260909/rch-spike-check.log), [Clippy](results/candidate-352b3695d-cid-20260909/rch-spike-clippy.log), [tests](results/candidate-352b3695d-cid-20260909/rch-spike-test.log) |
| Unchanged root workspace, all targets/features | **passed**; 372 tests plus 2 example tests | [check](results/candidate-352b3695d-cid-20260909/rch-root-check.log), [Clippy](results/candidate-352b3695d-cid-20260909/rch-root-clippy.log), [tests](results/candidate-352b3695d-cid-20260909/rch-root-test.log), [examples](results/candidate-352b3695d-cid-20260909/rch-root-examples.log) |
| UBS, final four touched Rust files | **failed**, exit 1; 5 critical / 109 warning / 90 informational reports | [summary](results/candidate-352b3695d-cid-20260909/ubs-touched.json) |
| UBS, earlier whole-tree snapshot | **failed**, exit 1; 198 critical / 6805 warning / 838 informational reports across 105 files; not fully triaged | [full log](results/candidate-352b3695d-cid-20260909/ubs-full-earlier.log) |

The five touched-file critical reports were reviewed: three classify public
transfer counters/generated test payloads as secrets; two do not follow the
returned Quinn thread handles to their joins in `main.rs`. The
[full touched-file output](results/candidate-352b3695d-cid-20260909/ubs-touched-details.log)
is retained without suppressions; the repository UBS log copies trim only
trailing whitespace. This review does not turn UBS's nonzero exit
into a clean scanner gate; the earlier whole-tree findings remain untriaged.

Formatting, shell syntax, documentation links, and diff whitespace passed
locally. Root Cargo gates exclude this standalone spike, so their pass does not
replace its own tests or the failed live interoperability lane. Full-runtime,
managed multi-client, forced-DERP, WebTransport, and handshake-security
qualification remain outstanding under the existing follow-up beads.

## Original 2026-09-08 results (`02df0613`)

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

Historical reproduction used the then-current `./run_all.sh`. The current
[RCH build and runner instructions](README.md#running) exercise the newly pinned
candidate, not this historical revision.

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

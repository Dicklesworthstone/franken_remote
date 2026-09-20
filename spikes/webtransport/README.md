# Phase 0 spike: Real HTTP/3 WebTransport and WSS Fallback Qualification

Bead **fr-p0-webtransport-b9u** · plan §12.1, §12.4, §12.5, §16.4, §23 Phase 0.

This spike qualifies the browser transport boundary for FrankenRemote:
1. **Live HTTP/3 WebTransport Interoperability**: Real Google Chrome (153.0.8010.52) connecting over UDP to Asupersync Native QUIC (`NativeQuicUdpConnection`) via ECDSA P-256 `serverCertificateHashes`.
2. **HTTP/3 Extended CONNECT & Framing**: Real negotiated HTTP/3 `SETTINGS` (`ENABLE_CONNECT_PROTOCOL=0x08`, `H3_DATAGRAM=0x33`, `H3_DATAGRAM_DRAFT04=0xffd277`, `SETTINGS_WEBTRANS_DRAFT00=0x2b603742`, `SETTINGS_WEBTRANS_MAX_SESSIONS_DRAFT07=0xc671706a`), extended `CONNECT` request handling (`:protocol webtransport`), and 200 OK / 403 Forbidden responses.
3. **HTTP Datagram Association**: RFC 9297 Quarter-Stream-ID datagram encapsulation and decapsulation (`quarter_stream_id = stream_id / 4 = 0`), bidirectional exchange of multiple datagram payloads up to 1150 bytes under the 1280 MTU ceiling.
4. **Origin Check Enforcement**: Strict Origin validation on extended CONNECT requests with typed 403 Forbidden rejection and browser connection abort.
5. **Stream Multiplexing**: Unidirectional stream preamble parsing (`[0x40, 0x54, 0x00]`) and bidirectional stream preamble stripping (`[0x40, 0x41, 0x00]`) with payload echo.
6. **WSS Fallback Profile**: Bounded receiver-granted credit backpressure (preventing browser WebSocket buffer bloat where `bufferedAmount` only reports outbound bytes) and stale-generation fencing on channel replacement.

**The full qualification verdict and evidence table live in [`RESULTS.md`](RESULTS.md).**

## Layout

- `src/main.rs` — CLI runner (`pki`, `serve-wt`, `serve-wss`).
- `src/pki.rs` — Generates ECDSA P-256 leaf certificate with 10-day validity (< 14-day WebTransport constraint) and computes SHA-256 fingerprint.
- `src/proxy.rs` — UDP middlebox proxy that sniffs Chrome's Initial DCID and forwards packets to `NativeQuicUdpConnection`.
- `src/h3.rs` — HTTP/3 control stream SETTINGS, extended CONNECT parsing with `:protocol` pseudo-headers, 200/403 HEADERS frames, and RFC 9297 Quarter-Stream-ID datagrams.
- `src/server.rs` — Live WebTransport server driving `NativeQuicUdpConnection` through handshake, control settings, CONNECT session establishment, datagram echo, and stream handling.
- `src/wss.rs` — Bounded-credit WebSocket server implementing separately-bound role channels (`/channel?role=video&gen=1`), receiver-granted credit flow control, and stale-generation rejection.
- `probe.html` & `probe.js` — Browser probe script executed by headless Chrome to exercise the WebTransport and WSS APIs.
- `run.py` — Orchestration harness running headless Google Chrome against the spike server across all test scenarios, recording NetLogs and JSON test reports.

## Running

Build the spike binary:

```bash
RCH_CARGO_WRAPPER_BYPASS=1 cargo build --release --manifest-path spikes/webtransport/Cargo.toml --target-dir /data/tmp/cargo-target-webtransport-spike
```

Run the complete qualification suite:

```bash
python3 spikes/webtransport/run.py
```

The script runs Google Chrome headlessly against both the WebTransport (HTTP/3 over UDP) and WSS endpoints, and writes the structured result to `results/latest/result.json`.

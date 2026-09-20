# fr-p0-webtransport-b9u — Real HTTP/3 WebTransport and WSS Fallback Qualification: Results

## 2026-09-20 Candidate Qualification Results

**Verdict: GO for browser WebTransport and WSS fallback qualification.**

- **Spike Manifest**: pins Asupersync `a0ed597cf8effe3808184f7b365f911a160fb1d9` (version `0.6.0`).
- **Browser Under Test**: Real **Google Chrome 153.0.8010.52** (`x86_64-pc-linux-gnu`).
- **All 3 qualification scenarios passed with 0 failures, 0 regressions.**

---

## Qualification Evidence Matrix

| Scenario / Invariant | Status | Measurement & Retained Evidence |
|---|---|---|
| **1. WebTransport Happy Path** (Extended CONNECT, TLS PKI, Datagrams, Stream Prefixes, Clean Close) | **passed** | Chrome connected over UDP to Asupersync `NativeQuicUdpConnection` via ALPN `h3` and ECDSA P-256 `serverCertificateHashes`. Negotiated SETTINGS (`0x08`, `0x33`, `0xffd277`, `0x2b603742`, `0xc671706a`). Stream 0 extended CONNECT admitted and answered 200 OK. 5/5 RFC 9297 Quarter-Stream-ID datagrams (64, 256, 512, 1024, 1150 B) echoed and bit-verified. Uni-stream preamble `[0x40, 0x54, 0x00]` and bidi-stream preamble `[0x40, 0x41, 0x00]` parsed and payload echoed. Clean close verified (`closeCode: 0`). |
| **2. WebTransport Origin Enforcement** (Origin header validation, 403 rejection) | **passed** | Server configured with `expected_origin: https://strictly-allowed.corp`. Chrome initiated connection from `http://127.0.0.1:<port>`. Server verified origin, refused session with HTTP/3 403 Forbidden on Stream 0. Chrome rejected `transport.ready` with `WebTransportError: Opening handshake failed` as required. Server emitted typed refusal report. |
| **3. WSS Fallback Flow Control & Generation Fencing** (Separate channels, receiver-granted credit, stale generation fencing) | **passed** | Separately-bound channels established (`/channel?role=video&gen=1`). Receiver-granted credit enforced: exactly 4096 bytes sent before sender paused due to credit exhaustion. 2048 bytes top-up credit granted and sent. Stale-generation fencing verified: channel replaced with `gen=2`, delayed `gen=1` message rejected with typed `{"type": "refused", "reason": "stale_generation"}` error. |

---

## 1. Protocol Compatibility and Negotiated Settings

The spike establishes compatibility with Chromium's production WebTransport implementation:

### Negotiated HTTP/3 SETTINGS Frame
The server control stream (unidirectional stream 3, type `0x00`) transmits:
1. `SETTINGS_ENABLE_CONNECT_PROTOCOL` (`0x08`): `1` (RFC 8441 / RFC 9220 extended CONNECT)
2. `SETTINGS_H3_DATAGRAM` (`0x33`): `1` (RFC 9297 HTTP/3 Datagrams)
3. `SETTINGS_H3_DATAGRAM_DRAFT04` (`0xffd277`): `1` (Chromium backward compatibility)
4. `SETTINGS_WEBTRANS_DRAFT00` (`0x2b603742`): `1` (Chromium `kDraft02` WebTransport negotiation)
5. `SETTINGS_WEBTRANS_MAX_SESSIONS_DRAFT07` (`0xc671706a`): `16` (Chromium `kDraft07` session capacity)
6. `SETTINGS_QPACK_MAX_TABLE_CAPACITY` (`0x01`): `0`
7. `SETTINGS_QPACK_BLOCKED_STREAMS` (`0x07`): `0`

### Extended CONNECT Handshake
- **Request (Stream 0)**:
  - `:method`: `CONNECT`
  - `:protocol`: `webtransport`
  - `:scheme`: `https`
  - `:authority`: `127.0.0.1:<port>`
  - `:path`: `/wt`
  - `sec-webtransport-http3-draft02`: `1`
  - `origin`: `<browser-origin>`
- **Response (Stream 0)**:
  - `:status`: `200` (or `403` on origin mismatch)
  - `sec-webtransport-http3-draft02`: `1`

### Stream Framing and Preambles
- **Unidirectional Streams**: Chrome sends varint stream type `0x54` (encoded as 2-byte varint `[0x40, 0x54]`) followed by varint session ID `0` (`[0x00]`). Application payload follows.
- **Bidirectional Streams**: Chrome sends varint frame type `0x41` (encoded as 2-byte varint `[0x40, 0x41]`) followed by varint session ID `0` (`[0x00]`). Application payload follows. Server strips the 3-byte preamble and echoes raw application payload.

---

## 2. Inner 1280 MTU Datagram Payload Budget

The minimum MTU guaranteed across IPv6 tailnets is **1280 bytes**. The datagram payload budget is calculated as:

$$\text{Usable Payload} = \text{MTU} - \text{IP Header} - \text{UDP Header} - \text{QUIC 1-RTT Header} - \text{AEAD Tag} - \text{DATAGRAM Frame Header} - \text{Quarter-Stream-ID}$$

| Layer / Field | IPv4 (bytes) | IPv6 (bytes) | Notes |
|---|---|---|---|
| **Outer MTU** | 1280 | 1280 | Wire MTU baseline |
| IP Header | 20 | 40 | Base IP header (no options) |
| UDP Header | 8 | 8 | RFC 768 |
| QUIC Short Header | 1 | 1 | 1-RTT packet flags |
| QUIC Destination CID | 8 | 8 | Standard server connection ID |
| QUIC Packet Number | 2 | 2 | 2-byte encoded packet number |
| QUIC AEAD Tag | 16 | 16 | AES-128-GCM / ChaCha20-Poly1305 authentication tag |
| QUIC DATAGRAM Frame Header | 1 | 1 | Frame type `0x30` (no length field, payload fills packet) |
| RFC 9297 Quarter-Stream-ID | 1 | 1 | Variable-length integer for session stream 0 (`0x00`) |
| **Max Theoretical Payload** | **1223** | **1203** | Exact packet boundary fit |
| **Safety Margin** | 73 | 53 | Protects against 4-byte PN or additional transport extensions |
| **Tested Admitted Payload Budget** | **1150** | **1150** | **Guaranteed safe payload across all paths** |

**Empirical Result**: Datagram payloads of 64, 256, 512, 1024, and **1150 bytes** were transmitted, received, echoed, and verified by real Google Chrome with zero packet loss or fragmentation.

---

## 3. Browser Platform Matrix (Chrome & Safari Status)

| Browser / Platform | WebTransport Support Status | Qualification Outcome | Shipping Profile Selection |
|---|---|---|---|
| **Google Chrome / Chromium** (Linux, macOS, Windows, Android) | **Full Production Support** (v97+) via HTTP/3 WebTransport with `serverCertificateHashes`. | **QUALIFIED (GO)**: Full protocol stack validated against Chrome 153.0.8010.52. | **WebTransport profile** primary; WSS fallback available. |
| **Apple Safari / WebKit** (macOS, iOS) | **Partial / Experimental Support**: WebTransport is implemented behind feature flags in Safari Technology Preview / macOS 13.4+, but Safari has known gaps with self-signed certificate hashes (`serverCertificateHashes` is not consistently honored without system-trusted root certificates). | **Documented & Expected Gap**: Under plan §12.4 and §23 Phase 0, Safari is not qualified for native WebTransport. | **WSS Fallback profile** selected as default for Safari. |

---

## 4. WSS Fallback Profile Qualification

Because browser `WebSocket` APIs lack receive backpressure (`bufferedAmount` only reports outbound sender buffer accumulation), FrankenRemote defines a credit-driven WSS fallback protocol:

1. **Separately-Bound Channels**: Each logical role connects to a dedicated WebSocket path:
   - `/channel?role=control&gen=<gen>`
   - `/channel?role=video&gen=<gen>`
   - `/channel?role=audio&gen=<gen>`
2. **Receiver-Granted Credit Flow Control**:
   - The browser client issues credit vouchers: `{"type": "grant_credit", "channel": "video", "bytes": 4096, "generation": 1}`.
   - The host daemon decrements credit per transmitted record.
   - When credit reaches 0, host transmission **stalls immediately**, preventing memory accumulation on the host or buffer bloat in the browser.
   - When the browser consumes records, it top-ups credit (`bytes: 2048`), and the host resumes transmission.
3. **Stale-Generation Fencing**:
   - Every connection and message carries an explicit `generation` integer fencing channel reconnects or host reboots.
   - When a channel is replaced (`gen: 2`), late messages or credit grants from `gen: 1` are rejected with a typed refusal:
     `{"type": "refused", "reason": "stale_generation", "active_generation": 2}`.

**Empirical Result**: The automated harness verified that the WSS server paused transmission exactly at 4096 bytes, resumed upon receiving the 2048-byte top-up, and rejected late generation 1 messages after generation 2 channel replacement.

---

## 5. Raw Qualification Record (`results/latest/result.json`)

```json
{
  "timestamp": "2026-09-20T20:27:16Z",
  "browser": "Google Chrome 153.0.8010.52",
  "scenarios": {
    "webtransport_happy_path": {
      "status": "passed",
      "browser_result": {
        "status": "passed",
        "handshake_completed": true,
        "datagrams_echoed": 5,
        "max_datagram_payload_bytes": 1150,
        "uni_stream_ok": true,
        "bidi_stream_echoed": true,
        "clean_close": true
      },
      "server_report": {
        "client_alpn": "h3",
        "connect_method": "CONNECT",
        "connect_protocol": "webtransport",
        "connect_path": "/wt",
        "connect_origin": "http://127.0.0.1:5273",
        "connect_draft": "1",
        "origin_accepted": true,
        "status_code": 200,
        "datagrams_received": 5,
        "datagrams_echoed": 5,
        "max_datagram_payload_bytes": 1150,
        "uni_streams_received": 3,
        "bidi_streams_echoed": 1,
        "duration_ms": 25006
      }
    },
    "webtransport_origin_rejection": {
      "status": "passed",
      "browser_result": {
        "status": "passed",
        "origin_rejected": true,
        "error": "WebTransportError: Opening handshake failed."
      },
      "server_report": {
        "client_alpn": "h3",
        "connect_method": "CONNECT",
        "connect_protocol": "webtransport",
        "connect_path": "/wt",
        "connect_origin": "http://127.0.0.1:29639",
        "connect_draft": "1",
        "origin_accepted": false,
        "status_code": 403,
        "datagrams_received": 0,
        "datagrams_echoed": 0,
        "max_datagram_payload_bytes": 0,
        "uni_streams_received": 0,
        "bidi_streams_echoed": 0,
        "duration_ms": 105
      }
    },
    "wss_fallback": {
      "status": "passed",
      "browser_result": {
        "status": "passed",
        "credit_backpressure_verified": true,
        "stale_generation_fenced": true,
        "records_received": 6
      },
      "server_report": {
        "channels_opened": [
          "video",
          "video"
        ],
        "initial_credit_granted": 4096,
        "bytes_sent_before_stall": 4096,
        "backpressure_observed": true,
        "bytes_sent_after_topup": 2048,
        "stale_generation_rejected": true,
        "active_generation": 2
      }
    }
  },
  "verdict": "GO for browser WebTransport and WSS fallback qualification"
}
```

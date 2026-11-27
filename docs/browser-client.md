# Browser Rust/WASM startup and media ownership

The served browser shell loads the real `fr-web` Rust/WASM adapter. Rust owns
bootstrap validation, canonical `FRD0` ClientHello encoding, refusal decoding,
connection deadlines, lifecycle state, and the distinction between transport,
observation, and input authority. The JavaScript boundary performs bounded
same-origin fetch/WebSocket I/O and projects the Rust snapshot into the page.

The adapter now composes authenticated view-only desktop startup, canonical
media reception and the same Rust-owned HEVC pipeline. The installed Linux
`frd run --browser-port` path registers the real capture owner; see
[browser desktop setup and scope](browser-desktop.md). Generic ingress callers
without that owner continue to report `desktop_available: false`. Receiving a
WebSocket `open` event, a heartbeat, or arbitrary media bytes never enables
observation or control. Live browser/OS/GPU and tailnet qualification remain
open. The separate **Test video
decoding** action runs a bounded public HEVC sample through the production
Rust media owner and native WebCodecs/canvas boundary. Its result describes
only this local sample; see [scope and reproduction](browser-probe.md).

## Build the self-hosted package

Use the exact compiler selected by `rust-toolchain.toml`, its
`wasm32-unknown-unknown` standard library, Node.js, and the `wasm-bindgen` CLI
version pinned in `Cargo.lock` (currently `0.2.128`). The packaging command
checks both executable versions before building.

From the repository root, choose a new output directory:

```sh
node scripts/build-web.cjs target/browser-wasm
FR_WEB_ASSET_DIR="$PWD/target/browser-wasm" cargo build -p frd --release --locked
```

The first command builds the release WASM module, generates its matching web
JavaScript, and runs the actual-WASM adapter, transport and media tests together
with the loader and executable local playback-probe tests.
It preserves existing output directories and refuses to overwrite one. Use the
appropriate existing host feature flags when building a macOS or Windows host.
The generated package and build artifacts stay outside version control.

When `FR_WEB_ASSET_DIR` is set, the daemon build requires both `fr_web.js` and
`fr_web_bg.wasm`. It bounds the JavaScript to 256 KiB and WASM to 4 MiB, validates
UTF-8 and the WebAssembly v1 header, and embeds the exact pair. The build script
does no network downloads. Both assets are selected by one build configuration;
an incomplete or invalid pair fails the build.

The catalog serves the pair at `/pkg/fr_web.js` and `/pkg/fr_web_bg.wasm`, with
JavaScript and `application/wasm` MIME types respectively. Other generated files
are not exposed. A build without a package returns HTTP 503 with
`browser_client_unavailable` for either package path. The HTML, asset headers,
and ingress CSP allow only self-hosted scripts and the narrow
`wasm-unsafe-eval` capability needed to compile the module.

## Connection ownership and bounds

Each connection attempt owns one Rust adapter, its fetch reader, timers, and
control socket. Disconnect, replacement, pagehide, and restoration from the
back-forward cache retire that owner before cleanup. Late callbacks can only
refer to their original attempt; a reused Rust-local serial cannot select a
different adapter. The shared module loader retains at most 16 pending
subscribers, removes each cancelled subscriber immediately, and constructs
adapters only for active subscribers. A stalled import cannot retain an
unbounded chain of retired connection attempts.

Package loading has a separate 15-second I/O watchdog. The original connection
start time is still passed to Rust, so loading and network progress cannot
extend Rust's ten-second startup deadline or the bootstrap expiry bound. A
silent response body or socket is retired when that deadline expires.

Bootstrap bodies and control records are bounded to 4 KiB before copying them
into WASM. The reader enforces the cumulative body limit even without a
Content-Length header. Malformed UTF-8, oversized records, wrong protocol
messages, unavailable desktop capability, and typed host refusals terminate
startup. Diagnostics are static and do not include peer text or credentials.

## Native HTTPS/WSS startup boundary

The native ingress emits the same bootstrap schema that the Rust adapter
validates: WSS profile and version, the requested role, and explicit desktop
availability. Only a verified admitted socket with a registered, available
desktop owner can obtain the available path. An unavailable bootstrap allocates no
nonce, session, control binding, media queue, or attachment ticket. The server
retains exact Origin, Host/SNI, Fetch Metadata, and credential-in-URL checks.

HTTP policy fields are single-valued. Repeated Host, Origin, Fetch Metadata,
or WebSocket upgrade headers are refused case-insensitively before admission,
even when the repeated values agree. Field names must be HTTP tokens, policy-field values must be ASCII, body
lengths must contain only decimal digits, and control bytes are rejected
before optional space/tab trimming. The parser retains the 8 KiB header,
48-field and 4 KiB body ceilings; ordinary header casing and optional whitespace
remain accepted. These checks alone do not enable browser desktop sessions.

The control socket accepts a bounded canonical `FRD0` browser ClientHello. A
policy-issued nonce is single-use and bound to its original peer and role; even
a valid hello receives a canonical `HostUnavailable` refusal when no real host
owner is registered. The installed path negotiates `browser-video-wss` v1 and
opens its original observation before issuing a one-use video ticket. Legacy
`FRBA` authentication and `FRAT`/`FRTK` attachment messages cannot construct a
session. See the [exact video attachment](../PROTOCOL_BROWSER_WSS.md).

The installed listener gives membership admission and HTTPS setup one original
three-second opening budget. HTTP parsing has its own bounded startup budget.
Control startup then retains one original deadline through the WebSocket
upgrade, first-message authentication, negotiation and native source setup.
The first-message authentication budget is also enforced while no bytes arrive.
Progress cannot restart the deadline within any phase, and a peer's terminal
refusal is never answered by another refusal.

`cargo test -p frd --test browser_ingress --locked` covers these paths with
actual TCP sockets and the real Rust browser adapter. Its TLS cases use a
fresh test CA and normal certificate/hostname verification, including an
otherwise valid certificate whose SNI violates the configured host policy.
These are local ingress tests. The installed Linux listener and authenticated
host-session/media integration are now wired by `frd run --browser-port`; the
new native integration and real tailnet/browser qualification remain unrun.

## Rust-owned WebCodecs media boundary

The generated package also exports a media-only `BrowserMediaAdapter`. The
production player receives that owner through the same bounded package loader:

```javascript
const media = await FrankenRust.createMediaAdapter(signal, {
    frames: 4,
    bytes: 33554432n,
});
const player = new FrankenHevc.WebCodecsHevcPlayer(canvas, callbacks, media);
```

`configure` takes the original negotiated generation, coded and crop dimensions,
frame rate, color fields, decoded-picture bound, canonical `hev1` codec, and
exact `hvcc` bytes. Rust validates their consistency and derives the matching
`hvc1` configuration for WebCodecs. `submitAccessUnit` takes the original
`generation`, `frameId`, `sourceTimestampUs`, `isKey`, and length-prefixed `data`
as a `Uint8Array`. Rust validates admission and retains the original frame and
source identities; WebCodecs callback timestamps are separate, non-reused local
identities that remain exactly representable in JavaScript.

The configured count and byte allowance covers admitted access units and
escaped output reservations across current and retired decoders. Capability
probes that cannot be cancelled retain their original decoder slot until they
settle. Late output, replacement, background presentation, drawing failures,
and callback reentrancy cannot move an old charge to a new decoder. Native
decoder closure and Rust acknowledgment are separate cleanup steps; an escaped
`VideoFrame` retains its reservation until its own positive closure.

`onFrameClosed` delivers one original decoder's resource receipt. It creates no
session authority or `ReceiverPressure` record. The authenticated outer channel
owner must account for that receipt on its original channel and credit epoch.
A false result from `reset()` means original custody remains, so keep the owner
alive. `browser_media_cleanup_incomplete` is terminal quarantine: admission and
release receipts stop, and reset cannot claim clean teardown. Diagnostic custody
stays bounded even if a broken native decoder emits unowned handles; this does
not claim to bound arbitrary allocations inside that native implementation.

## Validation and remaining integration

The portable Rust suite tests policy and ownership. The actual-WASM suite loads
the generated JavaScript and binary, checks the exported ABI and linear memory,
and compares canonical hello/refusal bytes against independent fixtures. The
combined transport suite uses the real Rust module with simulated browser I/O;
its bootstrap responses that set `desktop_available: true` are test fixtures.
They do not establish live browser or tailnet qualification.

```sh
node --test web/tests/*.test.cjs
node --experimental-vm-modules --test web/tests/wasm_loader.cjs
FR_WEB_WASM_PACKAGE="$PWD/target/browser-wasm" node --test \
  crates/fr-web/tests/wasm_adapter.test.cjs web/tests/wasm_transport.cjs \
  web/tests/wasm_media_configuration.cjs web/tests/wasm_media.cjs \
  web/tests/wasm_playback_probe.cjs
```

The installed path now joins authenticated HTTPS/WSS ingress to SessionOpened,
the real host capture owner, original-channel receiver credit and presentation
freshness through the Rust boundary. Remote control is not part of this
view-only profile. The media tests execute the actual Rust/WASM
owner with simulated native WebCodecs handles; they do not qualify browser/OS/GPU
HEVC support or physical presentation. H3/WebTransport and real-browser/tailnet qualification are
separate outstanding gates. The page keeps input controls disabled and reports
desktop availability and freshness from the original Rust owner.

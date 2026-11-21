# Browser Rust/WASM startup

The served browser shell loads the real `fr-web` Rust/WASM adapter. Rust owns
bootstrap validation, canonical `FRD0` ClientHello encoding, refusal decoding,
connection deadlines, lifecycle state, and the distinction between transport,
observation, and input authority. The JavaScript boundary performs bounded
same-origin fetch/WebSocket I/O and projects the Rust snapshot into the page.

The current adapter implements connection startup and typed refusal. The host
continues to report `desktop_available: false` until the authenticated browser
connection is joined to a real desktop session. Receiving a WebSocket `open`
event, a heartbeat, or arbitrary media bytes never enables observation or
control. Browser desktop use, WebCodecs presentation through this adapter, and
live tailnet interoperability remain unqualified.

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
JavaScript, and runs the real-WASM adapter and transport integration tests.
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

## Validation and remaining integration

The portable Rust suite tests policy and ownership. The actual-WASM suite loads
the generated JavaScript and binary, checks the exported ABI and linear memory,
and compares canonical hello/refusal bytes against independent fixtures. The
combined transport suite uses the real Rust module with simulated browser I/O;
its bootstrap responses that set `desktop_available: true` are test fixtures.
They do not change the shipping host capability.

```sh
node --test web/tests/*.test.cjs
node --experimental-vm-modules --test web/tests/wasm_loader.cjs
FR_WEB_WASM_PACKAGE="$PWD/target/browser-wasm" node --test \
  crates/fr-web/tests/wasm_adapter.test.cjs web/tests/wasm_transport.cjs
```

The remaining browser path must connect authenticated native HTTPS/WSS ingress
to `SessionOpened` and the real host session, then connect decoder/output
custody, receiver credit, presentation freshness, and control leases through the
Rust boundary. H3/WebTransport and real-browser/tailnet qualification are
separate outstanding gates. The current page keeps desktop controls disabled
while those capabilities are unavailable.

# Browser desktop over HTTPS/WSS

The installed Linux host has an opt-in, view-only browser desktop. It serves the local web application and its compiled Rust/WASM connection owner over the host's Tailscale HTTPS origin, then captures the selected X11 display through the existing supervised HEVC worker.

This is the explicitly degraded WSS profile. Session control and video use separate TCP connections. Configuration, recovery and encoded pictures share the video connection, so video remains subject to TCP head-of-line blocking. The profile does not advertise WebTransport or datagrams.

## Build and start

Build the browser package with the repository's pinned nightly (`nightly-2026-08-31`) and the matching `wasm-bindgen` generator (`0.2.128`). The existing `scripts/build-web.cjs` recipe generates the self-hosted bundle. Its `fr_web.js` and `fr_web_bg.wasm` must come from the same build. The connection ABI is **2**; the local media-probe ABI remains **1**.

Use a new output directory; the build recipe preserves existing bundles. It builds the actual WASM module and runs the generated-WASM integration suites:

```sh
node scripts/build-web.cjs /absolute/new/browser-bundle
FR_WEB_ASSET_DIR=/absolute/new/browser-bundle cargo build --locked -p frd
```

The second command embeds the generated pair. A build without the pair refuses `--browser-port` before listening or capturing. A previously generated connection-ABI-1 pair cannot run the new desktop.

Install the existing capture worker, logind monitor and observation indicator beside `frd`, or name their absolute paths. The Linux native build needs the existing `linux-displays`, `linux-session-monitor` and `linux-input` platform features. The installed Tailscale daemon must have HTTPS certificate issuance available, and the TCP port must be permitted by the configured ingress helper or protected directly with the required host capability.

An example foreground run, adjusted to the actual login session, display and installed paths:

```sh
frd run \
  --browser-port 8444 \
  --approval none \
  --sharing own-user \
  --logind-session 3 \
  --display :0 \
  --worker /absolute/path/fr-media-worker \
  --session-monitor /absolute/path/fr-session-monitor \
  --observation-indicator /absolute/path/fr-observation-indicator \
  --encoder auto
```

The host prints the certified HTTPS URL. Open that URL from an admitted tailnet machine and use the page's explicit connection action. The browser must pass the existing HEVC/WebCodecs qualification checks. The example's hardware-auto selection probes real backends; it does not silently switch to software. An operator can explicitly select the existing CPU developer profile with `--encoder software`.

`--browser-port` selects a browser-only listener. Do not combine it with native `--port`. The existing native listener remains the default when the browser flag is absent. `--once` ends the run after its first accepted desktop, including failed desktop startup. Ordinary asset and bootstrap requests do not consume that desktop lifetime.

## Authority and lifecycle

Every actual TCP socket is checked against installed Tailscale membership using its real local and remote endpoints. It retains its original admission and refreshes before that proof expires. An expired or failed admission is terminal; the server cannot acquire a new proof to rescue the old desktop.

HTTPS uses the existing verified, atomically rotated certificate owner, with a separate TLS 1.3 / HTTP/1.1 configuration. The handshake requires the installed node's canonical SNI. Certificate rotation does not extend a handshake's previously selected certificate. No reusable unchecked TLS acceptor or private key is exported.

The existing exact-Origin, Host/SNI, Fetch Metadata and bounded HTTP/WebSocket checks remain in front of application authentication. A navigation GET serves static content. A verified same-origin POST can issue a one-use bootstrap nonce, and the control socket must consume it in its bounded first application message. The video socket additionally needs its one-use role ticket and a live admission matching the control socket's complete authenticated peer identity. Matching only an IP address or a session label is insufficient.

The active desktop keeps the original observation owner, selected catalog/source, negotiated decoder binding and worker retirement receipt. Browser visibility/lifecycle invalidation closes that session. Locking, leaving or suspending the selected login, losing the observation indicator, a saved-policy revision, credential failure, or local Stop sharing also ends the run/session through the same original authority. A closed original session is not silently reconnected.

Capture cleanup is retained outside both socket futures. Shutdown fences authority and closes the original sockets, then explicitly retires the original worker and removes the protected listener's rules. A failed cleanup remains an error; it is not reported as a successful stop or used to authorize a replacement source.

## Video bounds

The negotiated `browser-video-wss` version 1 capability defines this layout. The video lane admits canonical records no larger than 64 KiB. Its outstanding budget is at most 2 MiB and four pictures. Before the first fragment, the host charges the entire access unit plus the shared bounded bookkeeping reserve; the charge is `3 * encoded_bytes + 4096`, limiting a single encoded access unit to 697,685 bytes. The charge covers the overlapping compressed copies during receive, WASM conversion and WebCodecs submission.

The Rust receiver uses the existing shared `ReceivePipeline` and the same `BrowserSessionCore` HEVC pipeline. It retains each picture's full reservation while reassembly, queued data, decoder callbacks or native frames still own resources. Cumulative credit returns follow actual resource release. Socket writability and a low outbound `bufferedAmount` do not grant the host receive credit.

## Current scope and evidence

The implemented profile is **one active, view-only Linux-hosted desktop**. Local approval dialogs, remote input, clipboard, audio, microphone, file transfer, browser channel replacement, shared multi-viewer hosting, and HTTP/3 WebTransport are not provided by this profile. Requested incompatible host flags receive explicit refusals before startup. Other host operating systems are refused by the browser entry point.

The exact pinned release WASM package and matching generated glue pass all 117 package tests, including the new desktop handshake, real HEVC admission, delayed decode/presentation and original resource-credit cases. Portable JavaScript tests pass 55 cases. Native WebCodecs handles and browser I/O in those tests are explicitly simulated; these results do not establish real browser/OS/GPU or live tailnet qualification.

The new real TLS/admission tests and host session regressions remain unexecuted: native compilation was killed inside the pinned Asupersync dependency before project diagnostics, including a single-threaded metadata-only attempt. Chrome/Safari rows and a real second-machine tailnet session remain unqualified until their actual runs and evidence are recorded.

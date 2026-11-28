# Browser hosting as a Linux user service

The installed browser desktop can run under the existing systemd user service.
Choose its HTTPS/WSS listener explicitly with the installer's `--browser-port`.
The generated service retains the selected graphical session, local policy and
Stop Sharing image. It starts with `graphical-session.target`; installation
does not approve a connection or start sharing.

## Build the actual browser host

Use the pinned toolchain and matching wasm-bindgen generator described in
[browser desktop setup](browser-desktop.md). Build the browser package into a
new directory and embed that exact pair in the `frd` used for installation:

```sh
node scripts/build-web.cjs /absolute/new/browser-bundle
FR_WEB_ASSET_DIR=/absolute/new/browser-bundle cargo build --locked -p frd --release
```

Install the matching capture worker, logind monitor and observation indicator
with the existing Linux native prerequisites. An installer build without its
embedded browser package refuses this profile before writing a unit.

## Select and preview the service

Run as the selected desktop's user. Replace the example display, session ID
and paths with their actual installed values:

```sh
frd install --user --dry-run --browser-port 8444 \
  --approval local --sharing own-user -- \
  --encoder auto --display :0 --logind-session c2 \
  --worker /absolute/path/fr-media-worker \
  --session-monitor /absolute/path/fr-session-monitor \
  --observation-indicator /absolute/path/fr-observation-indicator
```

`--browser-port` belongs before `--`, with the installer's other policy and
listener choices. It is mutually exclusive with native `--port`. A second
listener flag after `--` is refused. The generated `ExecStart` contains only
the browser port, so it cannot accidentally select both host profiles.

The explicit `--approval local` example requires device-attributed consent for
each new browser session. `--approval none` explicitly chooses unattended
observation instead. Omitting `--approval` preserves saved-policy inheritance;
the installer never changes that policy. Browser hosting still requires the
live Stop Sharing indicator in either mode.

After reviewing the preview, omit `--dry-run` to publish the unit and follow
the reported service-manager and protected-ingress-helper steps. The helper
remains a separate privileged installation. Supply the selected user's X11
environment to the user service. Startup obtains its own live Tailscale,
certificate, ingress, session and UI evidence before sharing.

The service journal reports the certified HTTPS URL. If a new login changes
the logind session ID, update the explicit service selection. A previous
login's selection or approval never authorizes a new desktop automatically.

## Profile boundaries

Only the Linux graphical **user** service is enabled for browser hosting.
System/headless services and other host platforms are refused. The existing
view-only browser restrictions still apply: unsupported input, audio,
clipboard, files, directory and adaptive-capture flags fail during validation.
Local-approval mode can use the browser's explicitly selected indicator image;
the native QUIC service retains its existing packaged-UI requirements.

Port preflight checks TCP for the browser listener and UDP for native QUIC.
That localhost check is advisory; the live host still protects and binds its
exact current tailnet addresses. Installation and rendering tests do not prove
a running systemd/tailnet/physical-desktop session. Native build and live
browser qualification remain separately required.

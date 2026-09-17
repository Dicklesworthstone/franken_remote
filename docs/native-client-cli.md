# Native client executable (`fr`)

The `fr-native` package now includes a Linux `fr` binary behind `linux-desktop`.
It provides machine discovery and an explicitly experimental, view-only native
connection through the existing installed-tailnet client and composed desktop
owner. Installing or running it never enables hosting. This is not an installable
remote-workstation release or a qualified transport/hardware support claim.

## Build and discover

```sh
cargo build -p fr-native --features linux-desktop --bin fr --locked
./target/debug/fr --help
./target/debug/fr hosts
./target/debug/fr hosts --json
```

The build needs the repository-pinned compiler and the existing XCB/native-input
prerequisites. The executable does not link the FFmpeg codec implementation: the
separate, locally installed `fr-media-worker` remains its supervised decoder.
Building that worker additionally requires `linux-media` and the selected FFmpeg
SDK. Native package verification is still an installation responsibility.

`hosts` reads the protected installed `tailscaled` Unix socket. A locally selected
absolute `--socket` path still requires a root-owned peer; it is not permission to
use a user-owned proxy or supply identity JSON. No scan, connection, codec probe,
certificate issuance or hosting operation is performed. The list contains stable
node IDs, canonical certificate names and addresses. Desktop availability and
access permissions remain unknown, not false positives derived from presence in
the network map. Shared, expired and unusable candidates are counted separately.
An unreadable or invalid local identity is an error, not an empty healthy list.

JSON output has `schema_version: 1`, a wall-clock output timestamp, an outcome and
candidate/exclusion data. Candidate rows explicitly carry null
`desktop_available` and `access_authorized`. This is a snapshot, not a reusable
authority handle. Opaque identifiers are escaped in human terminal output.

## Connect through the original native session

```sh
./target/debug/fr connect NODE_ID \
  --view-only --experimental-native --display DISPLAY_HANDLE \
  --worker /opt/fr/bin/fr-media-worker \
  --trust-roots /opt/fr/share/ca-roots.pem
```

Select an existing host running the compatible native host startup/listener; this
command does not install, start or configure one. Supply its current explicit
display handle from the host's display inventory. A graphical display picker and
a display-inventory CLI command remain separate unfinished UI work. A missing
handle refuses the attempt rather than selecting some other display.

The node argument is a stable ID by default. `--by-name` instead selects an exact
canonical tailnet FQDN through authenticated LocalAPI metadata. It does not enable
arbitrary DNS, host URLs, redirects or address-prefix identity inference. Every
attempt repeats the existing target validation, strict TLS and shared startup.
The host's admission scope and local approval still apply; none can be changed
by this executable.

`--view-only` is mandatory. Without it the command refuses with
`control_ui_unavailable`; it never silently downgrades a request for control.
The offered capability set contains only display selection, decoder startup,
media attachment and media delivery. No input, clipboard, microphone, playback,
file-transfer or semantic-access capability is requested. No UI mapping,
visibility witness or input grant is fabricated from a decode or map event.

`--experimental-native` is also mandatory because the native transport remains
unqualified. It is a development opt-in, not a change to any protocol, admission
or release gate. There is no alternate QUIC stack, runtime or codec fallback.

The CA file is explicit local configuration, never supplied by the peer. It must
be a regular PEM file with at most 64 certificates and 1 MiB. Certificate checking
cannot be disabled. An absolute worker path does not itself establish a trusted
package: the installer/user must select the protected verified worker image.

Optional settings are `--port` (default 8443), `--ipv6` (no silent family fallback),
`--attempts` (default 5, maximum 32), `--x-display`, `--socket` and `--json`.
`DISPLAY` supplies the local X11 display when `--x-display` is absent. Set the
local process's `XAUTHORITY` before launch when required; the same local
credentials serve window/input ownership and are passed to the worker. The CLI
does not mutate process-global credentials or use peer-supplied paths.

## Stop, reconnect and cleanup

The existing reconnect supervisor owns bounded backoff, fresh identity checks,
new native windows/decoder epochs, and independent cleanup deadlines. No prior
session state is replayed. Each renderer is created only after approved display
selection, at the selected native pixel dimensions. Unsupported sizes, hide,
resize or window loss retain the existing terminal behavior.

Close the window, or send SIGINT/Ctrl-C, SIGTERM or SIGHUP. A signal cancels the
original supervisor but does not abandon its future: the same independent
cleanup path still runs. Native cleanup failure takes precedence over a friendly
"cancelled" result. A genuine user-close decision is sampled before cleanup;
programmatic window cleanup cannot turn a connection failure into a successful
user exit. That decision stops further retries only after mandatory cleanup, and
is cleared before a new attempt. Supervisor cleanup errors still take precedence
even when the application has already collected its window. Closing a window
does not assert host-key release, physical pixel erasure or observed display
scanout.

Callbacks only retain bounded counters and the latest content-free window
handle. They never print, block on terminal input, retain pixels, or build an
unbounded event log. Output is emitted after the operation and its cleanup return,
so a slow pipe cannot hold session authority or delay a cleanup callback. Live
connection/approval status UI is not implemented by this command. A completed
JSON record distinguishes stopped observation and confirmed cleanup, and retains
`transport_qualified: false` and `physical_visibility_proven: false`.

Exit codes: 0 for a completed command or normally stopped observation, 2 for
argument/unsupported-mode refusal, 1 for operational failure, 130 for signal
cancellation after the attempted cleanup, and 74 for output-write failure.
Structured errors have a stable code and specific next action; arbitrary input
arguments, certificate contents and native library strings are not echoed.

## Verification boundaries

```sh
cargo test -p fr-tailnet --locked
cargo test -p fr-native --features linux-desktop --bin fr --locked
cargo test -p fr-native --features linux-desktop --test fr_cli --locked -- --test-threads=1
cargo test -p fr-native --features linux-desktop --test viewer_window_x11 --locked -- --test-threads=1
```

The CLI tests execute real child processes, parse output independently with
Python's JSON parser, and check refusals, output errors, trust-store bounds and
SIGINT during an actual stalled Unix/HTTP lookup. The metadata is an explicit
fixture, not a real installed Tailscale daemon. Running as root exercises positive
CLI discovery; running as an ordinary user verifies that the same user-owned
socket is refused. Production has no UID override. The native-window regressions
separately exercise the existing composed owner. None of these tests establishes
an end-to-end installed-tailnet + FFmpeg-worker desktop, optical presentation,
hardware acceleration, independent QUIC interoperability or a completed desktop
shell. Existing required native-input and release qualification gates remain.

# Native client executable (`fr`)

The `fr-native` package now includes a Linux `fr` binary behind `linux-desktop`.
It provides machine and approved display discovery plus an explicitly experimental
native connection, view-only or with one explicit control request, through the
existing installed-tailnet client and composed desktop owner. Installing or
running it never enables hosting. This is not an installable
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

## Inspect approved remote displays without a local renderer

```sh
./target/debug/fr displays NODE_ID --experimental-native --json
```

This is one freshly authenticated connection, using the same installed-tailnet
identity, strict TLS, negotiation and optional host-local approval as viewing.
It obtains the bounded display catalog and closes that session before output.
No display is selected, no decoder/worker is launched, and no input is requested.
It requires neither `DISPLAY`/`XAUTHORITY` nor a `--worker` or `--display` argument.
No reconnection is attempted for inspection; the command preserves refusal,
cancellation and timeout instead of publishing an old cached catalog.

Rows report opaque handles, geometry generations, signed origins, pixel/logical
sizes, rational scale and rotation. JSON encodes handles, generations and catalog
revision as decimal strings to preserve all 128/64 bits. `snapshot_only`,
`session_closed`, `display_selected`, `decoder_started`, `input_requested` and
`transport_qualified` distinguish the actual result from an active desktop or
permission. An empty catalog is a valid empty snapshot, not a ready display.

**Handles are session-local.** This command is an inspection, not a reservation
or an authority grant for a later connection. A new connection must obtain and
validate its own catalog; a numeric match alone does not establish physical
monitor continuity across sessions. Use `--display choose` to pick within the
current connection, or the explicit `--display only` policy for a single-monitor
host, instead of having to know an opaque handle ahead of time.

## Connect through the original native session

```sh
./target/debug/fr connect NODE_ID \
  --view-only --experimental-native --display only
```

Select an existing host running the compatible native host startup/listener; this
command does not install, start or configure one. `--display only` selects the
sole entry in the current approved catalog and refuses zero or multiple entries;
it never guesses the first or primary monitor. This explicit policy is evaluated
again against every fresh reconnect catalog, not a cached handle. A change from
one to multiple displays refuses the next attempt instead of making a new choice.

`--display HANDLE` retains the existing explicit numeric selector for callers
that know the current alias. A missing handle refuses the attempt rather than
selecting another display. Neither numeric aliases nor `only` promise physical
monitor continuity across different sessions. Use `--display choose` when a
human must select from the current approved catalog.

The node argument is a stable ID by default. `--by-name` instead selects an exact
canonical tailnet FQDN through authenticated LocalAPI metadata. It does not enable
arbitrary DNS, host URLs, redirects or address-prefix identity inference. Every
attempt repeats the existing target validation, strict TLS and shared startup.
The host's admission scope and local approval still apply; none can be changed
by this executable.

Exactly one role is mandatory: `--view-only` or `--control`. Neither, or both,
refuses with `connection_role_required` before any I/O; the command never
silently requests control or downgrades a control request to viewing. With
`--view-only` the offered capability set contains display selection, decoder
startup, media attachment and media delivery (required) plus the optional
decoder-metrics, reference-recovery and remote-cursor capabilities. No input, clipboard,
microphone, playback, file-transfer or semantic-access capability is requested.
No UI mapping, visibility witness or input grant is fabricated from a decode or
map event.

When the host also selects `remote-cursor` (`frd run` offers it), the host's
pointer, which its X11 capture excludes, arrives as a reliable shape on the
media-configuration lane plus replaceable position datagrams. The sandboxed
presenter composites ONE cursor into the presented image at the mapped position
(straight alpha, clipped to the picture, hidden when the pointer leaves the
shared display). It is confirmed host state for drawing only: never an input
command, decode receipt or freshness evidence. Evidence is the namespace e2e on
two Xvfb displays (`crates/frd/tests/native_host_linux_serial/real_cursor.rs`),
not a real GPU compositor, HiDPI scaling or Wayland. XFIXES cannot observe
`XFixesHideCursor`, and an image too large for the 8 KiB reliable record is shown
as a small built-in crosshair.

### Host playback audio (`--audio`)

```sh
cargo build -p fr-native --features linux-desktop,linux-audio --bin fr --locked
./target/debug/fr connect NODE_ID \
  --view-only --audio --experimental-native --display only \
  [--audio-server /run/user/1000/pulse/native] [--audio-sink NAME]
```

`--audio` is an explicit local request: without it the client offers no audio
capability at all. With it, the view-only offer adds the OPTIONAL
`native-audio-down` capability. The host must run `frd run --audio` (its own
local enable); a host without it simply omits the capability, the session
continues with video only, and the completion record reports
`audio_absence: "host_did_not_offer"`. `--audio` with `--control` is refused
before any connection (`audio_control_unsupported`): the client decodes audio in
its own session thread, and this slice never runs that decode in a controlling
session. Builds without the `linux-audio` feature refuse `--audio`
(`audio_unavailable_in_build`).

The attached `audio-down` channel carries `AudioConfiguration`/`AudioStop` on a
reliable lane and `AudioPacket` datagrams (Opus, 48 kHz stereo, 20 ms). Every
record is validated, including the negotiated packet/sample bounds, before the
real libopus decoder sees it. The client configures its local PulseAudio output
(the server from `--audio-server`, else `PULSE_SERVER`, else
`$XDG_RUNTIME_DIR/pulse/native`; the server's default sink, pinned at stream
start, unless `--audio-sink` names one), answers `AudioConfigured`, and only then
receives packets. A host `AudioStop` fences queued audio at once. A local playout
failure (for example a missed device slot) drops that stream without flushing old
samples into the next one and asks the host for a fresh audio epoch, at most
eight times per session. Audio never feeds video presentation or freshness.

The completion record adds `audio_requested`, `audio_active` (at least one decoded
frame was accepted by the local audio server), `audio_frames_submitted`,
`audio_output_resets`, `audio_absence` (null or a typed reason such as
`host_did_not_offer`, `host_audio_unavailable` or `local_output_failed`) and
`audibility_proven: false`: a submission receipt is not proof that anyone heard it.

### Request control (`--control`)

```sh
./target/debug/fr connect NODE_ID \
  --control --experimental-native --display only
```

The host must run `frd run --input-agent PATH`; a host without it refuses with
`host_control_unavailable`, never a silent view-only session. The connection
offers the control role plus the input attachment, grant, clock and
presented-state capabilities, and first *views* exactly as `--view-only` does.
Passing `--control` is the local user's decision: once the window is mapped and
shows an X11-submitted frame, the client confirms that window's coordinate
mapping, reports the frame, and sends ONE control request when the session
itself reports current view evidence. After the host's grant, it supplies the
single layout (native pixels 1:1, or the `--fit` rectangle) that attaches X11
input capture to this window. Keys (with repeat), absolute pointer and buttons
are captured; text, scrolling, relative pointer, audio and files are not. The
clipboard is a separate opt-in, described next.

The visibility witness is stated, not overclaimed: a frame counts as visible
when the presenter completed `SubmittedToCompositor` into the still-mapped
window (unmapping stops the session). Occlusion by another local window is not
detected, and `physical_visibility_proven` stays `false`.

Control is requested at most once per invocation. A refused, expired or lost
grant is never retried or reacquired: after a request, cleanup runs and the
command does not reconnect, reporting that attempt's original outcome. A new
grant needs a new `fr connect --control`. The completion record has
`role: "control"`, `control_requested`, `control_granted`, and content-free
counts of host input results (`input_results`, `input_submitted_to_os`); these
are host-reported stages, not local proof of an effect.

The controlled share does not forward the remote cursor yet: while
controlling, the only pointer shown over the viewer window is the local one.

### Clipboard (`--clipboard`, with `--control`)

```sh
./target/debug/fr connect NODE_ID \
  --control --clipboard --experimental-native --display only
```

Build `fr` with `--features linux-clipboard` (otherwise `--clipboard` refuses
with `clipboard_unavailable` before any I/O); `--clipboard` without `--control`
refuses with `clipboard_requires_control`. The connection additionally offers
the three clipboard capabilities, all OPTIONAL: a host that does not offer them
still grants control. The host opts in separately with `frd run --input-agent
PATH --clipboard` (`--clipboard` alone refuses with
`clipboard_requires_input_agent`).

The clipboard follows the controller's input lease. Nothing opens before the
grant: the host offers the clipboard lane afterwards, the transport attaches it
only under the active input attachment, and both sides exchange readiness
before either opens its X11 CLIPBOARD owner. Revocation, expiry or the end of
the session fences it with the lease; a later copy crosses neither way. Only
complete UTF-8 items of at most 1 MiB travel, on the bounded
begin/chunk/commit records, and are published only after whole-item validation
and a fresh authority check. Setting a clipboard is not a paste: no keystroke
is injected. Echoes of an item back to its sender are suppressed by provenance,
never by comparing text.

On the client the X11 owner runs on a worker thread of `fr` (a separate XCB
connection to `--x-display`). On the host, `frd` never loads X11: the owner is a
per-lane child of the same `fr-input-agent` image started with `--clipboard`,
with a cleared environment, bounded frames and payloads checked before
allocation, and the lease's publication deadline re-checked in the child
immediately before it takes selection ownership. A hung or malformed child is
killed and retires the clipboard only; control continues.

The completion record adds content-free fields: `clipboard_requested`,
`clipboard_active` (the lane completed bilateral readiness and this side's
clipboard worker started; it is not proof that either native owner opened),
`clipboard_received` (host items committed to this display's CLIPBOARD) and
`clipboard_absence`: `null` when active, otherwise a typed code such as
`host_clipboard_unavailable` (the host did not enable it),
`host_clipboard_refused`, `local_clipboard_unavailable`, `clipboard_not_started`
(for example, control never granted) or `not_requested`. Clipboard text never
appears in output, errors or logs.

Evidence is the namespace e2e with real X11 selections on two Xvfb displays
(`crates/frd/tests/native_host_linux_serial/real_clipboard.rs`), described
below. It is not a desktop clipboard manager (no history, no PRIMARY), not
Wayland, not images, and not a live tailnet.

`--experimental-native` is also mandatory because the native transport remains
unqualified. It is a development opt-in, not a change to any protocol, admission
or release gate. There is no alternate QUIC stack, runtime or codec fallback.

Trust roots are local configuration, never supplied by the peer. By default they
are the distribution bundle `/etc/ssl/certs/ca-certificates.crt`, which verifies
the Let's Encrypt certificates Tailscale issues; `--trust-roots` selects another
file. It must be a regular (non-symlink) PEM file with at most 256 certificates
and 1 MiB. Certificate checking cannot be disabled. The media worker defaults to
the `fr-media-worker` installed beside `fr`; `--worker` selects another absolute
path. A worker path does not itself establish a trusted package: the
installer/user must select the protected verified worker image.

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

`--control`, `--clipboard` and `--audio` end to end are exercised by the namespace suite, not by these commands.
Build the real binaries and the test executable, then pass that executable to
`scripts/test_linux_serial_lifecycle.sh` (set `FR_NS_SUDO=1` where unprivileged
user namespaces are restricted). It runs the ignored namespace suite serially,
including the `real_control::`, `real_clipboard::` and `real_audio::` tests of
`crates/frd/tests/native_host_linux_serial.rs`:

```sh
cargo build -p fr-native --features linux-desktop,linux-displays,linux-input,linux-clipboard,linux-audio \
  --bin fr --bin fr-media-worker --bin fr-input-agent --locked
cargo build -p frd --bin frd --locked
cargo test -p frd --test native_host_linux_serial --no-run --locked
```

The `real_audio::` tests of the same suite start two private PulseAudio daemons
with null sinks (one per side) inside the namespace. An independent
libpulse-simple player writes a tone into the host sink; `frd run --audio`
captures its monitor through the real `fr-media-worker --audio` child; the shipped
`fr connect --view-only --audio` plays into the client daemon, where an
independent recorder detects the tone with a Goertzel filter, measures the delay
of a frequency change and requires it sustained for two seconds. They also cover
a host without `--audio` (typed absence, no tone) and a missing monitor (typed
stop, video continues). Private null sinks are not a desktop PipeWire session,
speakers or audibility evidence, and any printed delay is only for that scope.

They start two Xvfb servers, the production `frd run` composition with the
real `fr-input-agent`, and the shipped `fr connect --control`. A python/Xlib
harness on the viewer display focuses the new window (as a window manager
would) and types and clicks with XTest; an independent python/Xlib client on the
host display observes the pointer position and the delivered Key/Button events.
Planted negatives freeze the client past its lease, click the host indicator
with XTest, and connect to a host without an input agent. The `real_clipboard::`
tests add one independent python/Xlib application per display that copies
(owns CLIPBOARD with a real server timestamp) or pastes (`XConvertSelection`
of `UTF8_STRING`): a unique non-ASCII text copied on the viewer is pasted on
the host, then the reverse; a host without `--clipboard` keeps control and
reports `host_clipboard_unavailable`; and a client frozen past its lease loses
the clipboard child, after which copies cross neither way. The tailnet
`LocalAPI`, CA and firewall are namespace fixtures; this is not live-tailnet,
physical-device or hardware evidence.

The CLI tests execute real child processes, parse output independently with
Python's JSON parser, and check refusals, output errors, trust-store bounds and
SIGINT during an actual stalled Unix/HTTP lookup. Display inspection process
tests prove that local renderer configuration is unnecessary and identity/trust
failures remain explicit; they do not fake a successful remote catalog. The
production inventory API is separately exercised over real localhost UDP/TLS
with explicit host identity/catalog fixtures, including host-local approval,
renewal, expiry and foreign-session refusal. The metadata is an explicit
fixture, not a real installed Tailscale daemon. Running as root exercises positive
CLI discovery; running as an ordinary user verifies that the same user-owned
socket is refused. Production has no UID override. The native-window regressions
separately exercise the existing composed owner. None of these tests establishes
an end-to-end installed-tailnet + FFmpeg-worker desktop, optical presentation,
hardware acceleration, independent QUIC interoperability or a completed desktop
shell. Existing required native-input and release qualification gates remain.

## Choose a display in the current connection

For multiple monitors, select in the same approved session instead of copying a
session-local alias from an earlier inspection:

```sh
fr connect NODE_ID --view-only --experimental-native --display choose
```

The native X11 chooser opens only after host approval (when required) and receipt
of the current catalog. It lists the displays' pixel dimensions and signed
desktop origins; click a row, use arrows then Enter, or press a row number.
Nothing is preselected. Escape or closing the chooser cancels. No thumbnails,
control grant, clipboard access or screen capture are introduced by this UI.

Every reconnect asks again with a fresh picker and the new catalog; neither a
row number nor an old handle carries over. The original startup/approval budget
still applies while waiting for a choice. No blocking UI work or terminal output
runs on the session thread. The picker is closed and joined before the display
selection hands off to the native renderer, and its old handles are retired.

The desktop/reconnect owner also retains picker cleanup after failure. A native
call that has not ended prevents replacement. A genuine Escape/close *before*
the one-use renderer factory starts has positive no-decoder evidence; after the
picker is joined the command reports `display_selection_cancelled` (exit 130),
not successful viewing or a fabricated decoder-reap receipt. Deadlines, link
failures and unconfirmed decoder bootstrap are not relabelled as user intent.
Cleanup errors still take precedence over cancellation.

This is the existing bounded XCB shell, not a new toolkit or browser. It needs a
local X11 display large enough for its fixed 560-pixel-wide menu. Wayland, scaled
UI/accessibility, physical scanout, hardware codecs and a complete installed-
Tailscale-to-FFmpeg desktop remain separate qualification work.

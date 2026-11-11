# Running a macOS host

A Mac shares its desktop with `frd run`, built with `--features macos-host`:
ScreenCaptureKit capture, VideoToolbox HEVC, and (with `--input-agent`)
control through a Quartz input agent with an on-screen indicator and default-on
plain-text clipboard sharing with its own local Off panel. It runs as a
per-user LaunchAgent in your login session: macOS allows screen capture and
input only there, so there is no system-wide daemon.

## Install

From a checkout, on any Apple-silicon Mac with Xcode:

```sh
scripts/macos/install-host.sh                       # this Mac
scripts/macos/install-host.sh --remote mac-mini     # another Mac, over SSH
```

The script:

1. builds release `frd`, `fr-media-worker`, `fr-input-agent`, `fr-session-monitor`,
   `fr-observation-indicator` and the `fr` client here (never on the target);
2. signs each with your Developer ID (`--identity` to choose), the hardened
   runtime, a timestamp and a stable identifier (`com.frankenremote.<name>`);
3. installs them in `~/Library/Application Support/FrankenRemote/bin` on the
   target, replacing each by rename so a running `frd` keeps its file;
4. runs `frd install`, which writes
   `~/Library/LaunchAgents/com.frankenremote.frd.plist`, then loads it with
   `launchctl bootout`/`bootstrap gui/<uid>`.

Defaults: control allowed (`--input-agent`; `--view-only` omits it), sharing
limited to your own devices (`--approval none --sharing own-user`), 60 fps
and 30 Mbit/s caps. Options after `--` go to `frd run`; `frd install` checks
them with `frd run`'s own parser and refuses anything every start would refuse
(no encoder, a macOS-unsupported option such as `--audio`, a LaunchDaemon).
Clipboard follows the controlling share by default; pass `-- --no-clipboard`
to install with it disabled, or select `--view-only` to omit control entirely.

`frd install` alone (without the script) writes the plist and prints the
`launchctl` steps; it never starts the agent.

## Permissions

macOS attributes everything the agent's workers do to `frd`. On the first
start the probes ask once, and macOS shows its prompts naming `frd`:

1. **Accessibility** (only with `--input-agent`): the input agent's probe.
2. **Screen Recording**: the capture worker's encoder probe.

Allow both, at the Mac itself (macOS never lets these be granted remotely).
Until then `frd` binds nothing and waits, saying so once in its log:

```
frd: waiting for macOS Screen Recording permission: at this Mac, allow frd in System Settings > Privacy & Security > Screen Recording; sharing starts by itself once it is allowed
```

It checks again every 5 seconds for the first minute, then every 30 seconds,
and starts sharing as soon as the permission is allowed. A Developer ID
signature keeps the grants across updates; an unsigned or ad-hoc build would
need them again after every rebuild. Both entries can be revoked any time in
System Settings > Privacy & Security.

## Input at the final authority boundary

The Quartz input agent prepares each complete keyboard, pointer, button and
scroll event before its final original-deadline and revoke check. Native event
allocation, display validation, pointer queries and event-property setup may
block; none can consume an already-checked action lifetime and then post the
action. After authorization, the agent posts only the event it already owns.
An expired or revoked action releases that event without posting or retrying it.

Pointer-query failure refuses the action instead of inventing a position at the
display origin. Cancelled button preparation does not advance double-click
state. Held-state cleanup retains possibly submitted presses and releases only
this owner's state; a completed release is removed from that state only after
native submission succeeds. This does not make Quartz submission an application
acknowledgment or eliminate OS changes racing an already-posted event.

`macos_input_preparation` exercises the production Rust owner with a clearly
identified deterministic FFI test boundary on non-Mac builders. It checks
prepare/check/post ordering, expired preparation, refusal, retained coordinates,
click state and release custody. It does not test Quartz, Accessibility or the
native display server; `macos_input_events` and real Mac session qualification
remain separate requirements.

## Committed Unicode input

The Quartz input executor implements the optional `native-input-text` v1
capability. Before advertising it, the real agent constructs and reads back
ASCII, BMP and supplementary Unicode event strings without posting input.
The per-lease agent repeats that probe. A failed optional probe leaves physical
key control available.

The existing Linux foreground robot owner exposes this through
`fr robot input NODE_ID ... --text-file PATH --json`. Start that owner with
`fr robot session open NODE_ID --foreground --role control --accept-input
--experimental-native --display only`, retaining the usual native trust and
worker options. Obtain the exact connection, lease, request and observation
handles using `fr robot input NODE_ID --connection-id ID --status --json` or
the matching robot screenshot. Supply those original handles and
`--max-observation-age-ms` with the text command. The ready status must include
`committed_text` in `input_support.capabilities`; older and text-incapable hosts
keep their other input operations.

The selected file must be one regular, non-symlink UTF-8 file of 1..4096 bytes.
Its content is preserved exactly, including whitespace and combining sequences.
The CLI reads it once and never puts its content in process arguments, receipts
or logs. It creates one original input action; no clipboard operation or guessed
keyboard layout is used. A controlling native macOS viewer also has an explicit
[Edit Text / Send composer](native-client-cli.md#unicode-text-and-mac-input-methods)
using the same optional host capability. Native IME and application-delivery
qualification are tracked separately from the source implementation.

Each complete Unicode scalar receives its original submission-time authority
check. A supplementary scalar's two UTF-16 units stay together. Both native
events are prepared before that check; the empty key-up is release-only cleanup.
The executor refuses text while it holds any remote key or button. Expiry and
uncertainty retain the exact submitted scalar prefix, fence the original owner
and never replay the remaining text.

The receipt confirms Quartz submission, not application acceptance. Apple's
[Unicode event API documentation](https://developer.apple.com/documentation/coregraphics/cgevent/keyboardsetunicodestring(stringlength:unicodestring:))
notes that some frameworks may ignore the event string and translate the
virtual key instead. Source tests cover scalar boundaries, optional negotiation,
file bounds and owner receipts; the native event-tap and expired-agent tests
still require a Mac with Accessibility permission. Live application typing and
tailnet execution have not been qualified by the portable tests.

## Text clipboard

With `--input-agent`, the host enables plain UTF-8 clipboard sharing for the
controller. The helper must be built with `macos-input` (the install script
already selects it); this brings in the shared `macos-clipboard` backend.
`--no-clipboard` disables sharing before native access, and `--clipboard`
explicitly selects the default. The installed LaunchAgent preserves that choice.
Read-only viewers receive no clipboard.

The separate `fr-input-agent --clipboard` child opens only after the original
input grant and bilateral clipboard readiness. It shows **Clipboard sharing —
click to turn off** below the control badge on the Mac's main screen. Clicking
it stops the original clipboard lane. Losing the panel or its bounded UI
heartbeat also stops the lane, including while a native pasteboard call is
blocked; the broker retains kill/reap custody of the original child. A UI gap
of 250 ms or more, including time asleep, permanently stops that clipboard
owner. Activity after waking cannot reopen the old clipboard lane.

Both directions use the existing bounded protocol: complete UTF-8 items,
at most 1 MiB, original item deadlines, exact-origin echo suppression and
local-copy supersession. Cleanup clears private buffers without clearing a
newer OS pasteboard value. No images, file promises or rich-text conversion
are enabled. A clipboard refusal normally leaves viewing and input usable.
If an outgoing clipboard payload is already retained inside active native
transport I/O, the original connection closes to enforce the privacy stop.

The host implementation and private-pasteboard/IPC regressions are in source.
Live Mac-host copy/paste, the local Off panel and stalled-provider behavior
still need native qualification; this is not a hardware or tailnet pass claim.

## Operate

| | |
|---|---|
| Log | `~/Library/Logs/FrankenRemote/frd.log` (directory 0700) |
| State | `launchctl print gui/$(id -u)/com.frankenremote.frd` |
| Restart | `launchctl kickstart -k gui/$(id -u)/com.frankenremote.frd` |
| Stop until next login | `launchctl bootout gui/$(id -u)/com.frankenremote.frd` |
| Uninstall | `frd uninstall`, after `bootout` |

launchd restarts the agent only after a failed start (`KeepAlive` on
unsuccessful exit, throttled to 10 s); waiting for a permission is not a
failure. It starts at every login and stops at logout. Graphical screen-lock
revocation remains unqualified; the console-session scope is described below.

## Local Stop sharing

Every macOS `frd run` requires the read-only `fr-observation-indicator` beside
`frd`, or the absolute image selected by `--observation-indicator`. The signed
installer includes it. View-only hosting uses this surface too; a controlling
session retains its independent control and clipboard panels.

Before listening, the helper must put a FrankenRemote item (a monochrome
screen icon) in the menu bar. It stays there while the host waits for peers.
Clicking it opens **Screen sharing is on** with a **Stop Sharing** button. A
local Stop ends the original host and drains its existing owners. It is a
successful cancellation even during startup, so launchd's failure-only restart
does not undo that user action. A removed or never-shown item, malformed IPC
and expired UI progress refuse or end the original run, and a new host start
is required afterward. Display changes do not end it.

The native event loop and independent command/watchdog worker retain original,
sleep-inclusive visibility deadlines. A request waits for a completed native
visibility turn that began after that request arrived; cached evidence cannot
extend it. No peer approval, capture or input authority lives in this helper.
The selected desktop user and OS remain trusted. This implementation is source
and component tested; actual AppKit visibility, physical input and installed-host
qualification remain pending. See [observation indication](observation-indicator.md).

## Console-session lifecycle

The shipped macOS `frd run` path now requires the read-only `fr-session-monitor`
beside `frd`, or at the absolute path supplied by `--session-monitor`. Build it
with `cargo build -p fr-native --features macos-session-monitor --bin fr-session-monitor`;
the signed installer includes it. A missing, invalid or unresponsive helper
refuses startup with `session_monitor_unavailable`, before tailnet I/O or
listening. This applies to viewing as well as controlling shares.

The helper reads the current process's actual Security.framework session ID
and the public CoreGraphics session dictionary. It binds the original UID,
security session and console set, and requires graphical access, on-console
state and completed login. It subscribes to AppKit user-switch, sleep,
logout/power-off and protected-data loss events before obtaining its first
snapshot. These events can only stop the original owner; none grants authority.

It also requires an online main display whose current `CGDisplayIsAsleep`
result is false, and subscribes to `NSWorkspaceScreensDidSleepNotification`.
A sleeping main display at startup refuses the share; display sleep during a
share ends the original session, and waking cannot reopen it. These checks
cover display power state; graphical screen-lock detection remains unqualified.

Native framework work stays on the helper's main thread. Its independent pipe
responder reports Opening during initialization and can expire evidence while
the main thread is blocked. Each real native read keeps its original 500 ms
deadline on Darwin's sleep-inclusive monotonic clock. IPC queries, later replies,
wake and switch-in cannot refresh expired evidence or restore the old session.
The broker retains the original child's kill/reap custody.

When console identity, active state or evidence ends, the host uses its existing
local-stop teardown: revoke input authority, retire the original clipboard and
file lanes, and stop observation/capture. It reports a typed cause such as
`session_inactive`, `session_suspending`, `session_ended`,
`session_identity_changed` or `session_evidence_lost`. The normal service manager
may start a new host process; no prior remote input lease is restored.

**Graphical screen-lock detection remains unqualified.** Public CoreGraphics
console properties contain no documented lock flag. Protected-data availability
is not proof of an unlocked graphical session, and no private lock key is used.
Apple DTS explicitly distinguishes those states in its
[supported lock-state API discussion](https://developer.apple.com/forums/thread/850327).
The listening report therefore uses `session_lifetime: "console_and_sleep_monitored"`
and names this limit, rather than claiming the Linux logind lock contract.

The selected desktop user, macOS and the signed native package remain trusted.
Darwin launches the helper through a canonical installed path whose opened inode,
ownership, executable mode and parent directories are checked; this is not an
isolation boundary against that same user. Native subscriptions, SDK compilation,
and physical switch/logout/sleep behavior require Mac qualification. Portable
state/IPC tests and Rust boundary checks do not establish that hardware evidence.
Tracking: `fr-uvji`; the separate screen-lock qualification is `fr-6y4w`.

API references: Apple's
[Security session declarations](https://github.com/apple-oss-distributions/Security/blob/main/OSX/libsecurity_authorization/lib/AuthSession.h),
[Window Server session properties](https://developer.apple.com/documentation/coregraphics/window-server-session-properties),
[session switch-out notification](https://developer.apple.com/documentation/appkit/nsworkspace/sessiondidresignactivenotification),
[display sleep state](https://developer.apple.com/documentation/coregraphics/cgdisplayisasleep(_:)),
and [screen sleep notification](https://developer.apple.com/documentation/appkit/nsworkspace/screensdidsleepnotification).

## Tailscale

`frd` reads identity, viewer admission (WhoIs) and its `*.ts.net` certificate
from the installed Tailscale:

- the standalone Tailscale app: its loopback LocalAPI;
- the open-source `tailscaled` (for example Homebrew's, as a root
  LaunchDaemon): its root socket `/var/run/tailscaled.socket`, chosen
  automatically when the app is absent, or named with `--socket`. The user
  running `frd` must be the node's operator (`sudo tailscale set
  --operator=$USER`) to fetch the certificate.

The Mac App Store Tailscale is not supported.

With `--directory` (after `--` at install), `frd` also answers a viewer it
already admits with a list of this Mac and that viewer's own untagged
machines, read from the same Tailscale: name, node ID, OS, online flag and
tailnet addresses. The iOS app shows them under "Machines on your tailnet".
It starts no capture or approval and grants nothing; it is off by default
(PROTOCOL_DIRECTORY.md).

## Address families

macOS has no per-connection firewall rule here. `frd` serves an address
family only when the kernel already drops that family's packets arriving on
another interface (`net.inet.ip.check_interface` on, IP forwarding off). On a
Mac with IPv4 forwarding enabled (Internet Sharing, VMs, containers) it
serves only the tailnet IPv6 address and says so in its log:

```
frd: not serving this node's IPv4 address: the kernel would also accept IPv4 packets for it from outside the tunnel (...)
```

Clients that find hosts through Tailscale (`fr`) use IPv6 automatically.
MagicDNS may answer only IPv4 (A) records, so a client given just the name
(such as the iOS app) cannot reach such a host by name alone.

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

1. builds release `frd`, `fr-media-worker`, `fr-input-agent`, `fr-session-monitor` and the `fr`
   client here (never on the target);
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
and [session switch-out notification](https://developer.apple.com/documentation/appkit/nsworkspace/sessiondidresignactivenotification).

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

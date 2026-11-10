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

1. builds release `frd`, `fr-media-worker`, `fr-input-agent` and the `fr`
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
Until then each start refuses by name, for example
`Refusal (encoder_unavailable): ... videotoolbox=screen_recording_not_granted`,
and launchd starts it again 10 seconds later. A Developer ID signature keeps
the grants across updates; an unsigned or ad-hoc build would need them again
after every rebuild. Both entries can be revoked any time in System Settings >
Privacy & Security.

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
unsuccessful exit, throttled to 10 s). It starts at every login and stops at
logout. Locking the screen does not end a share yet.

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

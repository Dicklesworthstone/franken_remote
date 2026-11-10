# iOS client

The FrankenRemote iPhone and iPad app (`mobile/ios/FrankenRemote`) views and
controls a FrankenRemote host over the user's tailnet. Its Rust core is
`crates/fr-mobile-ffi`: frd's own reconnecting viewer stack, a named dialer in
`fr-tailnet`, VideoToolbox decode behind fr-media's HEVC guard, and a
hand-written C ABI (`include/fr_mobile.h`, ADR 0003). The SwiftUI app draws
with Metal and turns touches and keys into the host's granted input.

This note records the decisions that differ from the desktop clients and why.

## Tailscale is the network, the name and the host's authority

The owner's direction (2026-11-09): "the whole thing should be built around
tailscale as providing the auth, the list of available machines, etc."

On macOS and Linux the clients read Tailscale's LocalAPI: they resolve a peer,
check its identity and list machines there. iOS gives other apps none of that.
Tailscale's iOS app is a Network Extension; its LocalAPI lives inside the
extension, and there is no shared container, socket or URL scheme another app
can query. What another app does get from it:

- **Reachability.** Packets to tailnet addresses (100.64.0.0/10,
  fd7a:115c:a1e0::/48) go through Tailscale's tunnel.
- **Names.** With MagicDNS on, the system resolver answers
  `<machine>.<tailnet>.ts.net` with the machine's tailnet addresses.

The app is built on exactly those two, plus checks of its own:

1. The user names a machine by its full MagicDNS name. `HostName::parse`
   accepts exactly four labels, `<machine>.<tailnet>.ts.net` (ASCII letters,
   digits and inner hyphens; stored lowercase). An IP literal, a short name, any
   other suffix, a trailing dot or a wildcard is refused before anything
   resolves.
2. The session resolves the name on a bounded resolver thread (at most two at
   once, five seconds). Every address must be a tailnet address; a name that
   resolves anywhere else ends the session (`FR_END_OUTSIDE_TAILNET`), so
   MagicDNS being off can never send the app to a public address with that
   name. No address at all is `FR_END_NAME_UNRESOLVED` (usually: Tailscale is
   not connected on the phone).
3. The host is authenticated by TLS: its Tailscale-issued Let's Encrypt
   certificate must be valid for exactly that name, chaining to ISRG Root X1 or
   X2. The two roots are embedded in the app; rustls cannot use the iOS trust
   store. The repository keeps no certificate files (`.gitignore`), so
   `fr-mobile-ffi/build.rs` copies them from the build Mac's
   `SystemRootCertificates` keychain, selected by their pinned SHA-256
   fingerprints, and the tests check the embedded bytes again.
   A certificate for another name, or one chaining to any other root, is
   refused (`FR_END_TLS_REFUSED`).
4. The first successful connection records a pin: the name and its sorted
   tailnet addresses (`fr-pin-v1 <name> <address>...`). Later connections must
   resolve to a subset of the pinned addresses, or the session ends
   (`FR_END_IDENTITY_CHANGED`). Only the user replaces a pin ("Forget the saved
   address pin"): a machine re-created in Tailscale gets new addresses.
5. The host authorizes the phone. frd reads its own LocalAPI: it resolves the
   phone's tailnet address to a node and user (whois) and applies its host
   policy (own-user sharing, approval, capability limits). Tailscale is the
   authority for who the phone is; frd's policy decides what it may do.

**The machine list.** Tailscale offers no way for another iOS app to list the
tailnet's machines, so a FrankenRemote host lists them instead. A host started
with `frd run --directory` answers an admitted viewer with itself and that
viewer's own untagged machines, from its own Tailscale LocalAPI
(PROTOCOL_DIRECTORY.md). The app asks its most recently used saved machines in
turn (at most three, `TailnetDirectory` behind the `HostDirectory` seam in
`Core/HostStore.swift`) and shows the first listing under "Machines on your
tailnet (via <machine>)", or that machine's typed refusal ("doesn't list
machines", "couldn't read its Tailscale status", "can't list machines yet")
or why it was unreachable. It never shows an entry the host did not send.
The app never fetches while a session is starting or open. frd serves one
connection at a time and listens again only after a short cooldown, so a
session to the machine being asked waits ("Connecting to…", at most three
seconds) for that fetch to end, then 400 ms more, before it dials; dialing
alongside the fetch lost the session's first handshake packets and stalled
it about five seconds (bead fr-w4yu).

A listed machine opens directly: its entry carries a pin of exactly the
addresses the host listed (`fr-pin-v1 <name> <addresses>`), and the session
(`FrConfig.direct`) dials those addresses without the resolver, IPv6 first.
MagicDNS may answer only one family while frd serves only the other (a Mac
with IPv4 forwarding serves only IPv6); the listing has both. TLS still
verifies the certificate for exactly the listed name, and that machine's own
frd still admits the phone by WhoIs and policy. The machine's port is the
app's default port (frd's 8443 unless changed in Settings). A listed machine
the user already saved (the answering host always is) opens exactly as saved
instead: its port, its pin, its name resolved by MagicDNS. The core pins only
names in the answering host's own tailnet, and marks only that host as
serving; a pin a session observes that keeps every saved address and adds a
new address family replaces the saved one, any other change does not.

`fr_directory_fetch(config, context, done)` runs one fetch on its own thread
(at most four at once) and calls `done` exactly once with a JSON document:
`{"status":"ok","via","truncated","machines":[{"name","node_id","os","online",
"serving","ipv4","ipv6","pin"}]}`, `{"status":"refused","via","reason"}` with
the refusal's stable name (`directory_disabled`, `directory_unavailable`,
`required_capability` from an older host...), or `{"status":"failed","via",
"end","detail"}` with an `FR_END_*` code. Debug builds accept
`-FRDirectory <machine.tailnet.ts.net>`: save that machine if new, fetch, and
print a content-free summary (counts only).

### The named dialer (`fr-tailnet::named`)

`NamedClient::new(name, roots, timeout)` and `dial(cx, port, pin)` dial IPv6
first, then one IPv4 address, with the same QUIC profile as the
LocalAPI-verified desktop dial (`fr-tailnet/src/profile.rs`, shared by both).
frd's `native_connection::Client::named` runs frd's existing reconnecting
supervisor over it (`run_named_control_capable`, `run_named_observing`); the
observed pin is read back with `Client::pin_watch`. The desktop paths are
unchanged.

### Asupersync on iOS

Asupersync's kqueue reactor is gated to macOS and the BSDs; on iOS the
runtime silently fell back to a polling path. Measured on the iOS simulator:
500 UDP round trips took 1.72 s unpatched and 21.5 ms with kqueue. The fix is
upstream-only: add `target_os = "ios"` beside `target_os = "macos"` in the
reactor's platform lists, 7 sites in `src/runtime/reactor/mod.rs` and 5 in
`src/runtime/reactor/macos.rs` (pinned revision ee72b665). Until it lands the
iOS build uses a patched copy through an untracked cargo config
(`FR_CARGO_CONFIG` for `mobile/ios/scripts/build-rust.sh`); the workspace pin
is unchanged.

## The C ABI

`include/fr_mobile.h` is the whole interface; `src/abi_check.c` compiles it
with `-Werror` for every Apple target and static-asserts each struct's layout
against the Rust side (also asserted in `ffi/tests.rs`).

- Sessions are generation-checked 64-bit handles (ADR 0003). A handle used
  after `fr_session_free`, a double free or an invented value returns
  `FR_STALE`; it never reaches freed memory. At most four sessions exist at
  once (`FR_BUSY`).
- Every call is non-blocking and safe from any thread. The session runs on its
  own thread (a current-thread Asupersync runtime with the platform reactor);
  freeing a session starts its orderly close and returns at once.
- Events (`FrEvent`: state, pin available, video geometry, ended, overflow)
  are content-free and sit in a bounded queue of 64. When the app falls
  behind, the oldest non-terminal events go first and one overflow marker says
  so; the end event is never dropped.
- Pictures cross by reference: `fr_session_copy_frame` returns a +1
  IOSurface-backed NV12 `CVPixelBuffer` (no pixel copies).
- Nothing in the core logs. Error detail (`fr_session_copy_detail`) is the
  typed failure's name only: no names, addresses, pixels, keys or text.

## Video

frd's worker drives an in-process presenter: fr-media's `HevcGuard` admits the
decoder record (the same admitted subset as the macOS viewer), every unit is
checked against the frozen parameter sets and its reference chain, and only
then reaches VideoToolbox (`src/vt_decode.c`, system frameworks only). A
refused record never reaches VideoToolbox; a decode failure closes that
decoder and recovery comes from a new IDR.

The app's Metal view runs at the display's refresh rate (120 Hz on ProMotion
iPhones: the app's Info.plist sets `CADisableMinimumFrameDurationOnPhone`)
and draws only when a new picture, the geometry or the cursor changed. It
draws at the panel's own density (`UIScreen.nativeScale`), with the picture's
origin on the device pixel grid. Colour is BT.709 with the stream's range. To
stay crisp:

- at an integer number of device pixels per picture pixel ("Actual pixels"
  in the view menu, or a pinch that ends near it) luma is sampled nearest, so
  text is pixel-exact. An untouched view rests there instead of at fit when
  that is at most 15% smaller (a 1080p host on a landscape iPhone is one
  device pixel per picture pixel, not 1.09); "Actual pixels" may zoom below
  fit when the device has more pixels than the picture (an iPad);
- when the picture is shrunk to fit (a 5K desktop on a phone), each screen
  pixel averages up to 4×4 picture samples instead of aliasing small text;
- fractional magnification is bilinear.

Control depends on proof that the user can see the host: the app reports a
frame as drawn only from the drawable's presented handler (`presentedTime >
0`), and frd counts it only while the view is visible in the active app. The
simulator's Metal has no presentation feedback, so simulator builds report a
frame when the GPU completes the command buffer that presents it (the
nearest evidence there; device builds never use it).

## Input

The app requests the host's grant profile without key repeat: keys, absolute
pointer, buttons and line scroll. It never synthesizes repeat; a held key
stays held until released. Pixel scroll and text are sent only if a host
grants them (today's hosts do not; the calls return `FR_UNSUPPORTED`).

- Positions are encoded picture pixels. The app undoes its own zoom, pan,
  letterbox, rotation and safe-area placement (`PictureMapper`); a point
  outside the picture is refused (`FR_OUTSIDE`), never moved onto an edge.
  A drag that leaves the picture releases at its last point inside.
- Scrolling sends whole 16.16 lines and keeps the remainder; at most 16 lines
  per event, the rest queued.
- Hardware keyboards send HID usages one-for-one (`UIKey.keyCode` are page-7
  usages). The software keyboard maps printable ASCII to US-layout keys plus
  Shift. Anything else goes to the host's text input if granted, and is
  otherwise refused with a message that never quotes it: the app never
  guesses the host's layout. Software-keyboard input (a paste) waits in order
  while the host's input queue is backed up (`FR_BUSY`) and goes on the next
  frames; it never overflows frd's bounded queue. While the keyboard is up,
  the picture stays above it.
  The extra-keys bar has Esc, Tab, sticky Ctrl/Opt/Cmd/Shift (tap: next key;
  tap again: locked; again: off), arrows, forward delete, Home/End, Page
  Up/Down and F1–F12.

### Touch

Without control, touches move only the view: drag pans a zoomed picture, pinch
zooms at the fingers, double tap toggles zoom.

| With control | Trackpad (default) | Direct touch |
| --- | --- | --- |
| One finger moves | the pointer, relatively, with acceleration | drags from where it touched |
| Tap (until the hold) | left click at the pointer | left click where touched |
| Two-finger tap | right click at the pointer | right click at the fingers |
| Touch and hold (0.5 s) | then drag: a left-button drag | right click |
| Two fingers move | scroll (natural direction, momentum) | scroll at the fingers |
| Pinch | zoom the view at the fingers | zoom the view at the fingers |

Slow trackpad motion follows the finger exactly as drawn (so zooming in gives
finer control); faster strokes travel up to 2.6× further on a smooth curve.
When zoomed, the view pans to keep the trackpad pointer in view, and a direct
drag held near an edge pans that way. The trackpad pointer starts at the
host's cursor when the host sends its cursor separately (else mid-view). Two
fingers moving together scroll; a pinch, even with one finger still, zooms.

## Lifecycle

- The session is visible only while its view is on screen in the active app.
  Losing that (control centre, app switcher, a covering sheet) ends control
  at once, which ends the session (frd's rule for a native producer): the
  app shows "Control ended" (`FR_END_CONTROL_STOPPED`, also used when frd
  refused input or the host's picture changed size), never "the host ended
  the session". A control request still pending when the view hid ends the
  same way: a grant never attaches after the view comes back. While in
  control, edge swipes need a second swipe (deferred system gestures), and a
  session on screen keeps the device awake. A view-only session survives a
  hide.
- Going to the background closes the session; returning offers to reconnect
  with the same capability, using the saved host as it is now. No session
  ever runs unseen.
- Network path changes are reported in the session; frd's supervisor retries
  a lost connection up to three times before the session ends.
- Rotation and safe-area changes keep the zoom and the centred picture point.

## Building and testing

```sh
# Rust core: macOS host tests run the same decode, session and ABI code.
cargo test -p fr-mobile-ffi --features macos-session
cargo clippy -p fr-mobile-ffi --features session --target aarch64-apple-ios-sim --lib -- -D warnings
cargo clippy -p fr-mobile-ffi --features session --target aarch64-apple-ios --lib -- -D warnings

# App (the project's "Build Rust core" phase runs mobile/ios/scripts/build-rust.sh).
cd mobile/ios/FrankenRemote
xcodebuild -project FrankenRemote.xcodeproj -scheme FrankenRemote \
  -destination 'platform=iOS Simulator,name=iPhone 17 Pro' test
xcodebuild -project FrankenRemote.xcodeproj -scheme FrankenRemote \
  -destination 'generic/platform=iOS' CODE_SIGNING_ALLOWED=NO build
```

Requirements: Xcode with the iOS SDK, the workspace's Rust toolchain with the
`aarch64-apple-ios` and `aarch64-apple-ios-sim` targets, and (until the
Asupersync change lands) the patched Asupersync through `FR_CARGO_CONFIG`.
Device installs need the user's own signing team.

The unit tests (`cargo test`, `xcodebuild test`) cover the decoder, the ABI,
name and pin rules, the touch state machines, picture mapping and keyboard
mapping. They do not show that the app works against a host: that needs a
live session.

Debug builds accept launch arguments for simulator runs without UI
automation: `-FRConnect <machine>.<tailnet>.ts.net [-FRPort N] [-FRControl]
[-FRStatsLog]`. Launch with `xcrun simctl launch --console-pty` so the
once-a-second `fr-stats` lines are not held in a pipe buffer.

### Live check status (2026-11-09)

The simulator and a view-only `frd run` on the same Mac cannot form a
session, for two independent reasons:

- MagicDNS on this tailnet answers A records only (no AAAA for any machine),
  and a macOS frd host with `net.inet.ip.forwarding=1` serves only its
  tailnet IPv6 address (it cannot keep outside IPv4 packets away from the
  port). The named dialer therefore reaches nothing: the session ended
  `Connection(Tailnet(HandshakeTimeout))` after three attempts and frd saw no
  packet. A phone on the same tailnet would hit the same wall with such a
  host; Linux hosts serve IPv4.
- The simulator shares the Mac's network stack, so even over IPv6 the
  connection would come from the host's own tailnet address, which frd
  refuses (`ConnectionAddresses`: local and peer may not be equal).

A live check needs the app on a second machine, or a host that serves IPv4.

### Live session (2026-11-10)

The simulator on mac-studio against a Linux host that serves IPv4
(`thinkstation2`: frd at a678fc10, private Xvfb 1920x1080 with moving
content, NVENC HEVC, the host's default 30 fps limit and 8 Mbit/s),
view-only, launched with `-FRConnect thinkstation2.<tailnet>.ts.net
-FRStatsLog`:

- first view 2.27 s after launch, including name resolution, QUIC/TLS and
  the first IDR;
- 26-28 fps sustained against the host's 30 fps limit, every picture drawn
  (`drawn_frame` within two of `presented`) over 30 s;
- decode 2.1-3.3 ms per picture (VideoToolbox, simulator);
- text sharp at 0.63x fit down to 11 px; 9 px still legible.

The first run found a bug: the app stayed on "Opening the screen…" over a
moving picture. A picture counted as seen only if the Metal pass had drawn
it by the time frd's callback received it, which never happens; it now stays
pending until the app reports it drawn.
The host directory (above) now gives the app every listed machine's
addresses, so a machine reached through a listing is no longer limited by
MagicDNS; a machine saved by name alone still is. The directory itself was
fetched live through `fr_directory_fetch` (19 machines from mac-studio's
`frd run --directory`, run from a second Mac), but not yet from the app's
own screen.

## Not done yet

- Control from the app in a live session (touch to pointer, keys) and a
  60 fps host; the live session above is view-only, in the simulator.
- Reaching an IPv6-only host saved by name only, without a listing.
- Pixel-precise scrolling (the app sends line scrolls; macOS hosts now offer
  pixel scrolls) and Unicode text (hosts do not grant it yet).
- iPad pointer devices as a real mouse (hover, secondary click), clipboard,
  audio and display switching.
- Release signing, App Store entitlements and a device-tested build.

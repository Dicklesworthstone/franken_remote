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
tailnet's machines, so the app shows only the machines the user added (from
the name the Tailscale app displays) and never a made-up list. `HostDirectory`
in `Core/HostStore.swift` is the seam for a real directory later (for example
the Tailscale API with a user-issued read-only key); none ships today.

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

The app's Metal view runs at the display's refresh rate (120 Hz on ProMotion)
and draws only when a new picture, the geometry or the cursor changed.
Colour is BT.709 with the stream's range. To stay crisp:

- at an integer number of device pixels per picture pixel ("Actual pixels"
  in the view menu, or a pinch that ends near it) luma is sampled nearest, so
  text is pixel-exact;
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
  otherwise refused with a message: the app never guesses the host's layout.
  The extra-keys bar has Esc, Tab, sticky Ctrl/Opt/Cmd/Shift (tap: next key;
  tap again: locked; again: off), arrows, forward delete, Home/End, Page
  Up/Down and F1–F12.

### Touch

Without control, touches move only the view: drag pans a zoomed picture, pinch
zooms at the fingers, double tap toggles zoom.

| With control | Trackpad (default) | Direct touch |
| --- | --- | --- |
| One finger moves | the pointer, relatively, with acceleration | drags from where it touched |
| Tap | left click at the pointer | left click where touched |
| Two-finger tap | right click at the pointer | right click at the fingers |
| Touch and hold | then drag: a left-button drag | right click |
| Two fingers move | scroll (natural direction, momentum) | scroll at the fingers |
| Pinch | zoom the view at the fingers | zoom the view at the fingers |

Slow trackpad motion follows the finger exactly as drawn (so zooming in gives
finer control); faster strokes travel up to 2.6× further on a smooth curve.
When zoomed, the view pans to keep the trackpad pointer in view, and a direct
drag held near an edge pans that way.

## Lifecycle

- The session is visible only while its view is on screen in the active app.
  Losing that (control centre, app switcher, a covering sheet) ends control
  at once, which ends the session (frd's rule for a native producer).
- Going to the background closes the session; returning offers to reconnect.
  No session ever runs unseen.
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

## Not done yet

- Reaching a host that serves only IPv6 when MagicDNS returns only IPv4
  (for example: a user-entered tailnet IPv6 address stored with the pin, TLS
  still checked against the name).
- A live session from the app: first frame time, frame rate and decode time
  are unmeasured (see above).
- A machine directory (no iOS API; see above).
- Pixel-precise scrolling and Unicode text (hosts do not grant them yet).
- iPad pointer devices as a real mouse (hover, secondary click), clipboard,
  audio and display switching.
- Release signing, App Store entitlements and a device-tested build.

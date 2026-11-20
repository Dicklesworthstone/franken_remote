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

Before becc05376, Asupersync gated its kqueue reactor to macOS and the BSDs.
On iOS the runtime fell back to timer-paced polling, re-polling every socket
each millisecond. Measured on the iOS simulator: 500 UDP round trips took
1.72 s that way and 21.5 ms with kqueue. While parked for 1 s on one receive,
the process spent 41.5-42.4 ms of CPU before the fix and 0.64-0.86 ms after.
becc05376 (in the workspace pin) adds `target_os = "ios"` beside
`target_os = "macos"` in the reactor's platform lists, so the iOS build uses
the pinned revision as is. `FR_CARGO_CONFIG` (for
`mobile/ios/scripts/build-rust.sh`) stays available for a local `[patch]`.

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
- `fr_session_frame_display` supplies the approved original display for a
  retained frame's live connection attempt. The app uses that lookup and the
  frame's own picture dimensions for mapping; lossy VIDEO events remain
  notifications. `fr_session_frame_cursor` also requires the frame's sequence
  to follow the cursor's decoder reset. Both additions preserve the existing
  `FrFrameInfo` and `FrCursor` layouts and write nothing on refusal.
- Nothing in the core logs. Error detail (`fr_session_copy_detail`) is the
  typed failure's name only: no names, addresses, pixels, keys or text.

## Video

frd's worker drives an in-process presenter: fr-media's `HevcGuard` admits the
decoder record (the same admitted subset as the macOS viewer), every unit is
checked against the frozen parameter sets and its reference chain, and only
then reaches VideoToolbox (`src/vt_decode.c`, system frameworks only). A
refused record never reaches VideoToolbox; a decode failure closes that
decoder and recovery comes from a new IDR.

The app states its screen's pixels when it starts a session
(`FrConfig.picture_width`/`picture_height`, longer side first). A Mac host
that can scale then sends the largest picture that fits instead of its native
pixels (`scaled-picture`, PROTOCOL_NEGOTIATION.md): a 5120x2880 desktop
arrives as 2350x1320 on a 2868x1320 iPhone. A Linux host still sends its
native pixels.

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

### Scaled picture, touch and cursor geometry

The original display supplies the image aspect and cursor coordinate space.
The encoded picture supplies the texture samples and the input area's size.
`PictureMapper` fits the original display into that area using the shared Rust
viewport's integer dimensions and centered offsets, then applies local zoom,
pan and safe-area placement. Metal draws the complete texture into that same
image rectangle. Direct touches in the internal bars are refused; trackpad
movement stays inside the image.

For example, a 5120-by-2880 display encoded as 1582-by-888 has an input image
rectangle starting at x=2 with width 1578. Drawing across all 1582 columns
would make visible edge clicks fall outside the shared input viewport.
The mapper retains those offsets explicitly. Host cursor positions, shape
dimensions and hotspots start in original display pixels and scale through
that same rectangle before drawing or seeding the trackpad pointer.

Each mapping belongs to a live retained frame owner. An identical-size
reconnect still retires its predecessor's gestures, pointer and queued typing.
A decoder reset within one attempt cannot place its new cursor on an earlier
picture: the scoped cursor accessor requires a later publication sequence.
Cursor metadata is checked again after copying shape bytes so a concurrent
shape update cannot pair one revision's geometry with another revision's data.

The added Rust/C and Swift regressions cover geometry, ownership and refusal
policy. They are source-reviewed changes, not a live device result. The
`macos-session` Rust suite and the Xcode builds/tests below still require
execution after the 2026-11-16 changes. The dedicated
[iOS client workflow](../.github/workflows/ios-client.yml) now targets the
Apple session and shipping Simulator app; its results remain required.

## Approved display selection

The app opens a host with `fr_session_start_with_display` and
`FR_DISPLAY_CHOOSE`. A catalog containing one approved display opens it
directly. With several displays, a native sheet lists every approved row
(at most eight), with pixel resolution, desktop position, scale and
rotation. This runs before the first frame: the session screen polls startup
even when Metal has no picture or the picker covers its surface. Host
approval, catalog validation and the original startup deadlines continue
while the user chooses; opening a picker grants no input authority.
The remote surface remains hidden through the sheet's actual dismissal,
so a frame behind the picker cannot count as observed. As with any hidden
view, this retires a queued control-on-connect request; after choosing from
multiple displays the user takes control with a fresh **Control** action.

The C bridge copies a complete catalog using one nonzero, never-reused local
request token. The original host aliases, geometry generations and catalog
remain in Rust's existing bounded `DisplayRequest`. Row reads, selection
and cancellation must name that exact request. An incomplete copy is never
displayed, an accepted selection is consumed once, and a delayed tap or
sheet dismissal cannot act on a replacement attempt with identical rows.
Interactive dismissal and Cancel close the original still-pending startup.
The existing `FrConfig` layout and `fr_session_start` primary-display
behavior remain compatible with earlier callers.

**View options → Change display…** closes the current session first. The
app retains its original handle until `fr_session_reopen_status` confirms
that both its service and bounded cleanup succeeded; an ended UI event,
including `FR_END_OK` after a cancelled dial, is insufficient. A failed or
unconfirmed cleanup refuses the switch. The UI waits at most ten seconds
and, after successful cleanup, uses the same cancellation-bound 400 ms host
listener cooldown as the Windows viewer and directory handoff (fr-w4yu).
Disconnect, scene inactivity and backgrounding cancel the pending switch.

The replacement opens the original host name, port, dial mode and observed
address pin, even if its saved-machine row changed meanwhile. It obtains a
fresh approved catalog and starts observing. A control-capable session
stays capable of a fresh **Control** action, but the automatic control-on-
connect preference does not carry into a display switch. Queued typing,
held keys, sticky modifiers, gestures, inertia and the old picture mapping
are retired with their original owner; even an identically sized display
starts with a new mapping. This changes one selected full display by opening
a new session; it does not add simultaneous multi-display streams or an
in-place protocol switch.

Evidence: portable Rust selection tests cover one/many displays, original
request identity, token exhaustion, catalog changes and consumed choices.
C/Rust layout and prototype assertions cover the new bridge. Swift tests
cover bounded complete catalog copies, cleanup/cooldown/refusal sequencing,
original pinned destinations and gesture owner replacement. Apple SDK
builds, XCTest execution and live iPhone/iPad display switching still need
native qualification; source policy is not hardware evidence.

## Send a selected file to the host

A control-capable session now offers **View options → Choose upload copy…**
while it is viewing, before control is requested. Before that action the menu
explains that choosing creates a local copy, up to **256 MiB**, and may download
the file from its provider. The system document picker selects one regular
file; folders, packages, symbolic links and unknown metadata are refused. The
picker can cover the desktop, so the app marks the remote view hidden
throughout its actual presentation and dismissal. It never keeps an obscured
controlling session alive or claims that frames behind the picker were seen.

Selection asks the OS to make an upload snapshot on the one preparation worker
and pins that snapshot's native descriptor. This local copy and any provider
download happen before Send; no file bytes are sent to the remote host. After
the picker closes and a fresh picture is visible, choose
**Control**, wait for the host grant and file channel, then press **Send once**
in the small transfer panel. The host must separately enable receiving files
through its existing `frd run --files DIR` configuration. A host that declines
the optional file capability still permits ordinary viewing and control.

Choosing is disabled while control is pending or granted. If already controlling,
use **In control → Give back control and keep viewing**. This fences the original
input, waits for successful service and cleanup plus the existing host-listener
cooldown, then opens a fresh observing session to the same original pinned host.
It clears automatic control intent even when **Ask for control on connect** is
enabled. A multi-display host may ask for a fresh approved display choice.
Backgrounding, disconnect, expiry or failed original cleanup prevents that
replacement. Selected files, earlier Send actions and input never carry into it.

The local choice has the native owner's original two-minute budget and exact
connection-attempt token. Provider preparation, picker time and waiting for a
grant all consume that same budget. A changed connection, close, background
transition or discarded choice cannot adopt a later token. A provider callback
that returns after cancellation is collected only for its original selection;
it cannot replace a newer choice or send automatically. Only one preparation
task exists per session, including a cancelled task still waiting on its file
provider.

**Send once** is a separate explicit action after selection. It samples the
current granted input view and original native deadline, consumes the prepared
descriptor even if refused, and never queues a retry with a renewed ticket.
The current session cannot replay a chosen file after reconnect or display
switch. Files are uploaded through the existing bounded ATP lane and original
disk worker. Swift does not hash or buffer contents; the OS prepares the
disclosed local copy, and native content reads require the later granted Send.
Source validation, host quotas, safe staging and atomic publication remain in
the existing Rust file implementation. The host's receiving quota can be lower
than the iOS copy limit. A published file is not opened automatically.

The transfer panel distinguishes bytes **queued** from the host's actual
publication receipt. It also preserves host refusal, interrupted or uncertain
publication, unconfirmed durability and failed local cleanup. **Pause** and
**Resume** affect only that original batch's pacing and do not extend its
deadline. **Cancel** stops the current upload without deliberately ending the
desktop; it cannot retract a file the host already published. Every command
retains the batch whose state the user saw. Earlier receipts remain in the
native owner's bounded history, and the panel summarizes received and other
outcomes independently of the current transfer.

### Document-provider access and cleanup

The picker obtains open-in-place access without creating persistent bookmarks.
On the preparation worker, an ordinary coordinated read keeps the original
file protected while its type and known size are checked. A nested
`NSFileCoordinator.ReadingOptions.forUploading` read then creates the OS-owned
snapshot. Both the original and snapshot must be regular non-package files,
at most 256 MiB, with the same byte count and exact basename. A provider that
renames the snapshot is refused instead of publishing a temporary filename.

The native bridge opens the snapshot descriptor inside its accessor. Apple
explicitly permits later upload from that descriptor after the OS unlinks the
snapshot when the accessor returns. No coordination claim remains open while
the user requests control or the network transfer runs; the original document
can be edited without changing this selected upload copy. The app does not
implement its own temporary-file copying or deletion. Ordinary coordination
may itself download a provider placeholder before size preflight, so the copy
limit does not promise a limit on provider download traffic.

The original security-scoped URL remains retained for every accepted upload
until the native session reports that all of its original file sources have
actually been released. A terminal event or a cleanup deadline is insufficient
proof.

Closing the UI hands the original session handle and its bounded access owners
to a cleanup task. That task frees the handle and balances document access only
after the source-release witness. A stuck cleanup retains its original slot
under the existing four-session bound; it does not recycle permissions or
create unlimited replacement workers. Discarded, refused and expired choices
release their own access after preparation returns and their unsent descriptor
is discarded. Nothing from a selected URL, local filename or file contents is
written into diagnostic JSON.

Source regression coverage includes original selection tokens, bounded status
decoding, one-use picker callbacks, publication versus queue progress,
independent cleanup outcomes, and local-copy type, size and name boundaries.
The executor became unavailable during this implementation; Apple SDK
compilation, XCTest execution and live
iPhone/iPad transfers with local, iCloud and third-party providers remain
unverified. In particular, real picker dismissal, provider cancellation,
background cleanup and host publication require native qualification.

Apple API contracts:
[document-picker access](https://developer.apple.com/documentation/uikit/uidocumentpickerviewcontroller),
[balanced security-scoped access](https://developer.apple.com/documentation/foundation/url/startaccessingsecurityscopedresource()),
[coordinated reads](https://developer.apple.com/documentation/foundation/nsfilecoordinator/coordinate(readingitemat:options:error:byaccessor:)),
and [upload snapshots and descriptor lifetime](https://developer.apple.com/documentation/foundation/nsfilecoordinator/readingoptions/foruploading).

## Receive selected host files

The machine list's **Connect and receive files…** action asks for local
permission for that one control-capable connection. It prepares a private
per-session directory beneath the app's Documents/Received folder, with a
256 MiB file limit and a 1 GiB cumulative session limit. Repeated names use
the existing native keep-both conflict policy. A normal connection has no
receive permission. Read-only viewing cannot receive files, and a configured
receiver still waits for the current input grant and the host's positive
`file-download` negotiation. The host chooses what to send; the phone cannot
browse arbitrary host paths. Nothing received is opened or executed.

Filesystem preparation runs on one bounded Swift background worker.
`fr_download_destination_prepare` reserves one of four native preparation
slots **before** validating and pinning the directory. Its local token is
one-use and never reused. The foreground app checks that its original choice
is still current, then calls `fr_session_start_with_files`; every attempted
start consumes the token, including invalid arguments or a failed start.
That entry point performs no filesystem I/O and uses the existing
`Config.download`/`Download` owner, ATP framing and bounded disk worker.
The C registry admits the session before spawning its native thread, so a
capacity refusal cannot leave a running receiver with no retainable handle.
The existing `FrConfig` layout and start functions are unchanged. A cancelled
preparation, background transition, reconnect, control-to-view transition
or display replacement never replays receive consent into another session.

The session shows separate staged-byte progress, original atomic-publication
receipts, directory durability and cleanup. Reaching 100% staged is not a
saved-file receipt. **Stop receiving** cancels only the original download
direction; viewing and control continue, and prior publication receipts stay
visible. A missing host capability, size limit, integrity failure and uncertain
publication have distinct states. Status documents contain no names, paths or
file contents and are bounded to 32 KiB and 64 results.

The local **Received files** sheet opens from the machine list, or from the
finished session once its receive cleanup is confirmed. Files from active or
uncollected original receivers remain excluded. The app retains the native
session handle and destination through actual disk cleanup, even after its
screen is dismissed. A failed cleanup is not converted into a clean close by
a later cancel or UI dismissal; subsequent connection attempts wait for the
original receiver's positive cleanup witness. Old committed receipts remain
available separately from that result.

The file list is local, bounded and explicit about truncation. It excludes
staging names, symlinks, directories and packages. Selecting a row rechecks
its app-owned parent, file type, device/inode identity, modification metadata
and size before opening the native system share sheet. That is a check at
handoff; later URL access belongs to the OS share sheet, not an immutable
export snapshot. The user chooses where to copy or share it; the app does not
automatically preview it. After process restart, no previous native disk
worker survives, so completed regular names from an interrupted session can
also appear; private staging still does not. That filesystem listing is not
substituted for an earlier transfer receipt or a durability claim.

Source verification covers one-use destination ownership, reservation and
descriptor bounds, cancellation and stale-token rejection, staged versus
published results, preserved receipts after cleanup failure, and bounded
status projection. The iOS test target additionally exercises status parsing,
malformed receipt rejection, export policy and an actual replaced-path/symlink
case. Actual Apple C ABI compilation, XCTest, native share-sheet interaction,
device lock/data-protection behavior and a live host-to-iPhone transfer still
require the Apple SDK and device qualification; Linux policy tests do not
establish those results.

## Input

The app requests keys, absolute pointer, buttons, line scroll, optional
precise logical scroll and committed Unicode text. It never synthesizes
repeat; a held key stays held until released. The original connection offer
advertises precise scroll and committed text independently. The requested
operations are then intersected with the host's negotiated profile before
the control request: a host without text support keeps physical-key control,
and a text call returns `FR_UNSUPPORTED`. Hosts without precise scroll retain
the separate line-scroll gesture producer described below.

- Positions use the encoded-picture-sized input area. The app undoes its
  local zoom, pan, rotation and safe-area placement (`PictureMapper`) and
  keeps the original display's integer image rectangle within that area.
  Points in its letterbox bars or outside it are refused (`FR_OUTSIDE`).
  A drag that leaves the image releases at its last point inside.
- Scrolling sends whole 16.16 lines, at most 16 per axis in one event.
  Fractional carry retains its earliest input deadline and clears when it
  expires or other input intervenes. Oversized whole actions are refused;
  they do not become a backlog for later gestures.
- Hardware keyboards send HID usages one-for-one (`UIKey.keyCode` are page-7
  usages). Single ASCII characters and shortcut keys use the US-layout
  physical-key path, including sticky modifiers. A callback with multiple
  ASCII characters uses the whole-text path when the host grants that
  capability and no sticky shortcut modifier is active, so ordinary paste
  and dictation do not become a long series of physical key presses.
- A callback containing Unicode or another character without a physical-key
  mapping submits the **entire completed value once**, synchronously through
  `fr_session_text`. Mixed ASCII/Unicode values are never split into a key
  prefix and a text suffix. The limit is 4,096 UTF-8 bytes; an empty, oversized
  or embedded-NUL value is refused whole.
- Unicode never enters the app's typing queue. Pending ASCII keys, hardware
  keys held down or active sticky modifiers refuse the new value before any
  prefix or modifier side effect. An unsupported, busy or unavailable host
  produces a content-free notice, and the value is discarded. A later frame,
  readiness change or reconnect cannot replay it; the user must enter new
  input explicitly.
- Physical strokes that wait for capacity retain one opaque Rust capture
  taken before the original callback is classified or mapped. Every retry
  keeps its original source, complete view generations, suspension history,
  stroke index and exclusive input deadline. That deadline is at most the
  existing 100 ms native-event budget, shortened by the original checked
  input bound and time spent waiting to capture. A busy stroke has queued
  no modifier or key effects. Rust reserves capacity for its complete
  modifier/key/release sequence and releases only modifiers it pressed.
  An expired stroke, changed context or newer input overtaking a waiting
  callback ends the original control source; dependent keys are discarded
  and the normal host-held-state cleanup runs. A delayed shortcut cannot run
  after an intervening pointer or hardware-key action.
  A later capture or ticket cannot give the old callback a new lifetime.
- The physical queue holds at most 4,096 strokes; an oversized callback
  refuses as a whole before a prefix is queued. Rust also bounds outstanding
  callback captures to 64. Closing the keyboard, losing focus, replacing the
  session or ending control clears retained callbacks. A host without text
  capability retains physical ASCII input with these same limits; a long
  paste that cannot fit its original deadline is refused rather than typed
  after a stall. Whole-text refusal never falls back to physical keys.
- The Rust committed-text capture retains one original timestamp and source/view context
  through its existing bounded queue. A changed or unready context refuses
  the whole capture while keeping physical-key control available. Acceptance
  into that queue is not a guarantee of atomic OS insertion: native delivery
  can still fail or report a partial effect, and those outcomes are never
  replayed automatically.
- The view uses the default keyboard with `UIKeyInput`. This handles complete
  committed callbacks from supported keyboards or paste; it does not add a
  `UITextInput` composition or preedit implementation. While the keyboard is
  up, the picture stays above it.
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
host's cursor when the host sends its cursor separately (else mid-view).
While a finger is down, and for a second after it last moved the pointer,
the host's cursor shape is drawn at the pointer this device sent, so it
moves with the finger rather than a network round trip later. Otherwise the
cursor is drawn where the host reports it, and the pointer follows the
host's cursor (a host-side move is kept by the next drag). Two fingers
moving together scroll; a pinch, even with one finger still, zooms.

The two-finger gesture now chooses precise logical scrolling when the host
has actually granted `native-input-pixel-scroll` v1. Finger travel stays in
UIKit logical points, with the user's scroll-speed setting; picture size,
Retina density and local zoom never multiply the distance. Hosts without that
grant use the existing separate line-step gesture producer. The mode is fixed
before the first delta and stays fixed through momentum: a refused precise
event is discarded, never retried or converted to lines. Both modes stop the
gesture on refusal. A display callback delayed more than 100 ms discards old
momentum instead of resuming it after the stall.

The additive C calls are `fr_session_supports_pixel_scroll` and
`fr_session_scroll_pixels`; the existing line-scroll call and structure layouts
are unchanged. Rust quantizes precise distances to signed 16.16 and retains at
most one sub-quantum fraction per axis. Fractions keep the oldest contributing
capture time and the earliest exclusive deadline, including time spent waiting
for the native grant lock. Mode changes, other input, expiry, source retirement
and view replacement discard this state. Enqueue validates the original
uninterrupted input context under the same queue lock; a suspend/resume with
identical numerical view generations cannot rebind the old action. The existing
host ticket and submission-time authority checks remain in force.

This slice tracks `fr-p3-ios-app-rh8.1`. Portable Rust regressions exercise the
real quantizer and event queue; Apple session/C ABI and Swift regressions cover
negotiation, legacy lines, scale independence, momentum, refusal and replacement.
Simulator execution and real-device scroll feel require the Apple validation
lane; this source change does not establish native touch or host-injection
qualification.

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
`aarch64-apple-ios` and `aarch64-apple-ios-sim` targets.
Device installs need the user's own signing team.

The unit tests (`cargo test`, `xcodebuild test`) cover the decoder, the ABI,
name and pin rules, the touch state machines, picture mapping and keyboard
mapping. They do not show that the app works against a host: that needs a
live session.

Captured typing adds production-policy regressions for whole-stroke modifier
ownership, bounded callback custody, unchanged busy retries and consumed
indices; the source-queue cases cover every view generation, same-view
suspend/resume, unchanged deadlines and key input without Unicode negotiation.
Swift tests cover original capture reuse, callback bounds, refusal and session
replacement, plus the negotiated whole-text paste choice. The source/ABI and
Swift suites require their normal workspace and Apple builds; standalone Rust
policy execution does not qualify a live iOS keyboard or host insertion.

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
- Device qualification of negotiated precise scrolling, a full native
  text-composition interface, and live device qualification of the
  committed-Unicode callback path. The callback and
  negotiation regressions establish source policy, not native OS insertion.
- iPad pointer devices as a real mouse (hover, secondary click), clipboard
  and audio. Approved display selection and cleanup-confirmed switching
  are implemented above; native device qualification remains outstanding.
- Release signing, App Store entitlements and a device-tested build.

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

On Linux, `hosts` reads the protected installed `tailscaled` Unix socket. A locally selected
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

To explicitly check for FrankenRemote's native capability handshake on those
known peers, use:

```sh
./target/debug/fr hosts --probe --experimental-native --probe-limit 16 --json
```

At most four probes run at once, with one original twenty-second deadline for
discovery and probing, and at most five seconds per peer. Results name each
peer's acknowledged capability intersection or typed failure; unfinished and
omitted candidates stay explicit. This opens no observation, media or input.
The advertisement does not qualify the current desktop or codec; see
[bounded capability discovery](../NATIVE_CAPABILITY_INSPECTION.md#bounded-tailnet-discovery).

## macOS discovery

### Default-build identity inspection

Build the default CLI on macOS with the repository-pinned Rust toolchain and
Apple's command-line development tools:

```sh
cargo build -p fr-native --bin fr --locked
./target/debug/fr hosts --json
./target/debug/fr peers --json
./target/debug/fr inspect NODE_ID --json
./target/debug/fr inspect machine.example.ts.net --by-name --port 9443
```

`hosts` and `peers` take one informational snapshot. `peers` is a snapshot-only alias;
it rejects saved-host operations and remote probes. The Linux `hosts` behavior
above remains unchanged. macOS reads the installed Tailscale app's loopback
`LocalAPI`, as Tailscale's own CLI does; [native discovery](native-discovery.md#macos-installed-app-identity-and-outbound-connections)
says what that trusts and what it does not check. Status is bounded to 1 MiB
and parsed by the existing status projection. A missing or untrusted endpoint,
a refusal, malformed output, cancellation and timeout produce typed errors
within one one-second lookup budget.

The default build also supports `inspect NODE_ID`, or a full canonical name
with `--by-name`, through a fresh status/WhoIs/status sequence. All three
reads must come from the same daemon run. The selected node,
key, user, tags and exact node-owned addresses must agree; routed subnets and
conflicting identities refuse. All reads and parses share the original
one-second budget. The result
carries the verified stable ID even when selected by name. It cannot select a
replacement from a name prefix or accept caller-supplied identity JSON.

Default-build inspection uses the existing inspect envelope with state
`identity_checked`. `displays_count` and `requires_approval` are null (shown as
`unknown` in human output). The direct, relayed or unknown path is Tailscale's
last observation in the final status snapshot; it does not measure
FrankenRemote reachability or verify a remote TLS connection. `--port` defaults
to 8443 and is reported as configuration; no connection is made to it. This is
a snapshot, not a reusable authority handle or desktop readiness result.

On macOS, `--socket PATH` reads only that root `tailscaled` Unix socket (the
open-source daemon); without it, the app's endpoint or else
`/var/run/tailscaled.socket` is used ([native discovery](native-discovery.md#macos-installed-app-identity-and-outbound-connections)).
A missing socket is `tailscale_unavailable`, one this user's process serves is
`untrusted_localapi`. Default-build inspection also rejects `--saved`/`--hosts-file` with `saved_hosts_unsupported`
and remote `--probe` with `platform_unavailable` before runtime or process
creation; it never downgrades those requests to an ordinary lookup. This lookup
grants no session authority and does not loosen Linux's credential-checked Unix
socket adapter. Other commands need the desktop build below; its existing
command dispatch and feature choices remain separate. Hosting is not enabled.

**Evidence (2026-11-08, macOS 27, Tailscale 1.104.1, `io.tailscale.ipn.macsys`):**
`fr hosts --json` listed the tailnet's peers, and `fr inspect NODE_ID --json`
reported `identity_checked` for a live peer in the default build. Twenty
inspections took 0.23 s of CPU in all, process start included, or about 12 ms
each. The App Store variant (`io.tailscale.ipn.macos`) is not supported.

## macOS desktop client

```sh
cargo build -p fr-native --bin fr --features macos-desktop --locked
./target/debug/fr connect NODE_ID --control --experimental-native --display only
```

The `macos-desktop` feature builds a native viewer from system frameworks only.
VideoToolbox decodes the host's HEVC stream into IOSurface-backed buffers, and
Metal draws them, with the client-rendered remote cursor, into a
`CAMetalLayer` in an AppKit window. AppKit keyboard, pointer and wheel events
enter the same `frd` input path as the Linux viewer. As on Linux, losing focus,
hiding the window or a change of picture size stops local input and ends
control; closing the window ends the session. **Ctrl+Alt+Shift+Escape** also
stops the original session locally before any other input from that native
batch can be forwarded.

The Mac client offers precise trackpad scrolling through the independent
optional `native-input-pixel-scroll` v1 capability. A Mac host offers it only
after its actual input-agent probe reports `PixelScroll`; existing Linux and
older hosts retain their ordinary control grant. Precise AppKit deltas keep
their logical native distance, both axes and signed 16.16 fractions through
the wire and Quartz continuous-scroll fields. Neither the viewer's Retina
scale nor `--fit` changes scroll distance. Wheel events whose native unit is
lines continue to use the separate line-scroll capability.

There is no guessed trackpad-to-wheel conversion. If the host did not grant
precise scrolling, a precise event is refused locally and counted in
`native_scroll_refusals.unsupported` in completion JSON; keyboard, pointer,
buttons and native line scrolling remain available. The same object's
`expired` and `invalid` fields count aged and invalid/out-of-range native
scroll events; it is omitted when all three counts are zero. Nonprecise scrolls obey the host's
32-line combined-axis action bound; oversized events are refused, not clamped.
Fractional carry is less than one wire quantum per pixel axis or one line per
line axis, expires within the original 100 ms local capture budget, and never
crosses another input action or a new grant. All native input events now retain
their original AppKit capture time through a fixed client-clock correlation;
draining a delayed UI queue cannot refresh their action deadlines.

The new precise-scroll path has portable production-policy and negotiation
regressions. Native tests extend the existing process-local Quartz event tap
to check continuous mode, both fractional fields, integer point fields,
modifier flags and position, plus final-authority refusal and cancellation.
These tests route events to their own process, not the owner's desktop. The
new Objective-C path, SDK build and native event readback are **not tested in
this Linux session**; smoothness and target-application behavior still require
Mac qualification. The relevant native targets are:

```sh
cargo test -p fr-native --no-default-features --features macos-input --test macos_input_events
cargo test -p fr-native --no-default-features --features macos-input --test macos_input_agent
```

The unit contract follows Apple's [`scrollingDeltaY`](https://developer.apple.com/documentation/appkit/nsevent/scrollingdeltay),
[`CGScrollEventUnit`](https://developer.apple.com/documentation/coregraphics/cgscrolleventunit)
and [fixed-point scroll-field](https://developer.apple.com/documentation/coregraphics/cgeventfield/scrollwheeleventfixedptdeltaaxis1)
documentation. Fixed-point event-field retention is distinct from proving that
every target application consumes subpixel fractions.

### Unicode text and Mac input methods

A controlling Mac window reserves a local **Edit Text** composer. Once the host
has selected and granted the optional `native-input-text` capability, release
all remote keys and mouse buttons, click **Edit Text**, and type with the Mac's
normal input method. AppKit owns preedit, conversion and candidate placement in
that local editor. Finish composition and click **Send** to queue one complete
Unicode value, limited to 4096 UTF-8 bytes. **Cancel** discards the local draft.
Local paste and file/rich-text import are unavailable in this bounded editor.
Physical key and shortcut forwarding remains the default outside the editor;
the keys used for composition are never also sent as remote physical keys.

The composer belongs to the original controlled source and its uninterrupted
input view. A changed display, viewport, codec/recovery generation, actual input
suspension, focus loss or hidden window cancels its draft and any unsent commit.
Ordinary compositor candidate gaps temporarily disable Send while preserving a
valid draft. An explicit Send retains its original native event timestamp,
the existing 100 ms local-input age bound, and the original owner's checked
send deadline. The completed value keeps that deadline through the native
handoff, local queue and encoded transport; a later renewal cannot extend it.
A refused admission never retries automatically; if the same draft remains
valid, a fresh Send click is required.
Ctrl+Alt+Shift+Escape remains a local stop in editing mode.

Each opening starts a fresh local editor, so a delayed completion from a
cancelled draft cannot enter a later draft. Stop and focus-loss handling fences
control before asking the native input method to clean up. Mouse selection is
serviced through the same bounded event pump; wheel and caret scrolling remain
available without a blocking scrollbar drag.

This composer sends committed text through the existing host text-input path.
It does not synchronize a remote caret, implement remote preedit, infer a remote
keyboard layout, or substitute clipboard sharing. Hosts without a granted Text
capability keep the editor unavailable and retain ordinary control.

Completion JSON retains `native_text.queued_locally` and
`native_text.refused_locally` when text was attempted. A locally queued value
does not confirm OS submission or application delivery. The separate
`native_input_receipts` object preserves validated original host results:
confirmed submitted action-operation prefixes, partial and uncertain effects,
rejections/expiry/cancellation, duplicate receipts and unavailable retained
history. Pointer reports can repeat and add no action effects. Duplicates add
no effects; unavailable history leaves the corresponding
effects unknown. These reports survive a later connection or cleanup error.

The original queue/context diagnostic passes 16 tests, and the native text
policy diagnostic passes 8 tests; both pass strict Clippy under the pinned
toolchain. These checks use production source and dependencies, but do not
exercise the full session runtime or AppKit FFI. The full runtime remains
locally blocked by the existing dependency compiler memory limit. Actual
AppKit/IME editing, the Apple SDK build and a Mac-to-Mac text session remain
unqualified; CI has explicit native syntax, text and receipt-policy checks.

The initial `--control` request is consumed once for the whole invocation.
Automatic reconnects obtain fresh viewing only; after the recovered window is
visible, focused and fresh, press and release **F8** to request new control.
Held, repeated, modified or stale keys cannot request a grant. No queued
gesture, input action or grant crosses an attempt. Focus/visibility loss and
known host revocation remain terminal. Completion distinguishes request counts
and viewing reconnects, confirmed local cleanup, and the host's independently
reported cleanup/effect stages. A close click cannot hide a native cleanup
failure or an adverse host report.

`--fit WIDTHxHEIGHT` bounds the initial macOS content rectangle in **AppKit
points**. The picture preserves its aspect ratio and fits inside the available
local screen. Rendering stays in Metal; input coordinates use the actual
drawable picture after layout, including the local Retina scale. The same
ceiling includes the 44-point toolbar for Change Display and sharing controls.
Controlling windows reserve an additional 76-point local text row before input
mapping, so opening the editor does not resize a controlled picture. They need
at least 184 points of height and the greater of 480 points or the sharing-strip
width: 152 plus 230 per file direction, 184 for the default-on clipboard, and
144 for `--file-drop`'s **Send copied files** button.
Both file directions with clipboard need 796x184 points; with
`--no-clipboard`, one file direction needs 480x184 and both need 612x184.
Adding `--file-drop` makes those two-direction minima 940x184 and 756x184;
upload with native selection alone needs 710x184, or 526x184 with `--no-clipboard`.
Viewing-only windows retain the 152x108 minimum. Smaller explicit ceilings
refuse before a connection. A narrow picture can have horizontal letterboxing
to keep the local controls accessible. Without
`--fit`, the remote logical size remains the picture's default.

The admitted native presenter runs on one bounded Asupersync blocking task.
Decoder configuration, frame submission, cursor updates, idle repaint and the
configured presenter's destructor do not run inside the UI/session future.
There is one queued operation and one reply. Cancellation fences further work
and rejects late replies; a native call that remains blocked leaves cleanup
pending until that exact task retires. It does not permit a replacement decoder
or an automatic control retry.

AppKit publishes resize/expose requests through a short geometry snapshot; it
does not wait for the media render lock or acquire a Metal drawable. The media
owner repaints the retained picture, including when no new frame arrives.
Changed geometry cannot authorize input until that repaint is submitted.
Repaint alone creates no new frame receipt or visibility freshness. Escape and
local close still work while the geometry is pending. Missing Metal resources
fail the media operation instead of counting a black or unsubmitted frame.

AppKit window creation and final window retirement still run on the main
thread. Final window-owned renderer releases can therefore delay that native
boundary after input has been fenced; this change does not claim a hard
deadline for arbitrary OS or driver calls.

The original foreground connection also answers local commands:

```sh
fr robot session status NODE_ID --json
fr disconnect NODE_ID
fr robot session close NODE_ID --connection-id INSTANCE --json
```

These authenticate the same-user local peer and exact target/instance. Close
cancels the original invocation and waits for its native cleanup result; it
does not create another remote connection. An unavailable reply leaves the
outcome unknown. These local operations do not need a responsive Tailscale
lookup. Plain `fr status` continues to report installed-Tailscale metadata.

The client's tailnet lookups (the peer target before dialing, and its renewal
once a second during a session) use the status/WhoIs/status sequence above over
the loopback `LocalAPI`, about 12 ms of CPU per lookup. The signed-CLI
transport it replaced used 60-80 ms of CPU per read.

Not implemented on macOS yet, and refused by name before any network I/O:
`--capture-system-shortcuts`, `--audio`, `--microphone` and
`fr robot session open`. The Mac client offers the controlling text clipboard
by default; the host must independently enable it. A Mac host still has no
clipboard backend and returns a typed absence without disabling control.
Local status identifies the foreground invocation; optional quality and
automation observations remain unavailable on macOS.

**Evidence (2026-11-08):** from the owner's Mac to a Linux NVENC host (ts2), the
window showed the host's live desktop and typed keys reached the host. A build
whose lookups used the signed CLI also showed the live desktop with control
granted. With the current lookups, `--control` sessions opened their window and
ran for about a minute until ts2, under heavy memory pressure, ended them
(`Renewal(Transport(Expired))`); no frame was captured from that build.
Pointer, wheel and click delivery and the window-close outcome have not been
confirmed live. fr-native's macOS tests decode a real HEVC stream through VideoToolbox
into the Metal surface.

The reconnect-consent, emergency-release and fitted-window policies pass
portable tests. Real runtime regressions for the local owner, completion and
stalled media-task cleanup are included but have not run in this environment:
compilation of the unchanged pinned Asupersync dependency was killed by
SIGKILL. Native AppKit event delivery, Metal repaint, the macOS pathname owner
and a full live reconnect/close sequence still require execution on a Mac;
Linux-side policy tests are not hardware qualification.

### Native macOS host and display selection

```sh
fr connect --choose-host --control --experimental-native --display choose
```

The first AppKit window lists a bounded snapshot from the installed Tailscale
app: canonical certificate name, stable node ID and an informational network
path hint. A selection retains the exact name/ID pair shown in that original
snapshot. The ordinary connector freshly resolves and authenticates that pair
before dialing; a renamed or reassigned peer refuses. The picker never carries
a cached network address, readiness assertion or input grant.

The display picker opens only when the original authenticated session supplies
its approved catalog. It shows the current pixel/logical dimensions, desktop
origin, scale and rotation. An empty or changed catalog cannot reinterpret a
queued row. `--display only` and explicit handles retain their existing policies.

Pickers have no initially selected row. Select a row with the mouse or arrow
keys, then use the confirmation button or Return; a double click also confirms.
Home/End and Page Up/Down navigate the list. Horizontal scrolling or Left/Right
reveals long names without replacing their identity text. Escape, Cancel,
window close or Ctrl-C cancels. The native event pump is bounded and nonmodal;
the original choice budget is at most 120 seconds, and an earlier session
deadline still wins. Native cleanup precedes returning a selected identity or
display handle. All of this is a local choice, separate from host approval and
control consent.

The running viewer's **Change Display** button ends the original observation
through its own close path. It parks new input immediately and fences the
original file choices. A replacement opens only after confirmed local input,
file, decoder and window cleanup, with no adverse host terminal result. It then
performs the existing fresh connection, identity/admission checks and any host
approval, and asks for a selection from the new catalog. Old display aliases,
input mapping and control consent are never carried into the replacement.
The new viewer starts without control; release and press F8 to request it.
Previously selected files are not offered again.

Explicit changes share the connector's existing attempt/owner bounds. Closing
the window, the emergency escape, an input/native failure or failed cleanup
takes precedence over a pending change. Completion retains each original
display-change close and lease report even if a later choice is cancelled or
a reconnect fails; unknown host cleanup remains unknown. The Change Display
button is unavailable while a control request is pending. Its reserved toolbar
area cannot produce remote pointer coordinates.

**Verification (2026-11-09):** six display-switch, nine control-consent and five
sizing tests pass with the pinned compiler and strict standalone Clippy. Four
tests of the exact picker Rust boundary also pass against the real shared
client/wire/core crates, with strict Clippy; they do not execute AppKit.
Workspace formatting, documentation links and diff checks pass. Session and
CLI integration regressions are included but remain unrun: the pinned
Asupersync dependency previously exceeded this executor's memory, and no
matching compiled dependency is available. These extensions still require a
native Mac build and a live picker/change/close run. Portable policy and source
tests do not establish AppKit event delivery, native cleanup or a successful
remote display change.

### Explicit macOS file transfer

The desktop build accepts explicit `--send` selections and a local
`--receive` directory on a controlling connection. Both directions can be
selected independently or together:

```sh
fr connect NODE_ID --control --experimental-native --display only \
  --send /absolute/source.txt --send /absolute/source-directory \
  --receive /absolute/private-destination --receive-conflict keep-both
```

Sources are classified and pinned before AppKit or network startup. The
existing eight-selection bound, portable names, symlink/special-file refusals
and original sender limits still apply. Receiving opens the explicit existing
directory through the existing protected-directory checks; the peer never
chooses the local destination. `--receive-max-file-bytes`,
`--receive-max-session-bytes` and `--receive-conflict reject|keep-both` use the
same native receive policy as Linux. The default conflict policy is reject;
nothing here enables overwrite, automatic resume or general filesystem access.

The original observer registers those selections once. Contents still need
the actual host control grant and the matching negotiated file attachment.
Unsupported optional directions produce typed absence results while ordinary
desktop control can continue. Automatic reconnect and a later F8 request do
not select the files again or reopen the original destination for a new
transfer. A failed dial before observer configuration has started no file
owner and can retain the pending choice for its normal bounded retry.

A reserved strip above the Metal picture shows send and receive status with
independent **Cancel** buttons. It does not cover the remote viewport or enter
the remote pointer mapping. Cancel requests act on mouse-down, before another
ordinary network/input turn, and affect only their original file direction.
A request during setup still settles the already negotiated attachment before
retirement. The window's close/escape and connection cleanup fence both file
owners; successful native cleanup additionally requires their actual disk
workers to be reaped. A stalled worker leaves cleanup unconfirmed.

Progress is content-free and sampled at most ten times per second. Queued
upload bytes and staged download bytes are not publication receipts. The
completion report retains the original indexed outcomes, successful
publication/durability evidence, missing or uncertain outcomes, cancellation
requests and local cleanup status, even when a later viewing attempt fails.
Cancellation does not undo files already published. Unknown publication or
pending cleanup remains explicit; neither becomes “nothing sent.”

The shared CLI report projection also preserves these distinctions on Linux.
Audio and microphone support remain separate unavailable Mac features. Later
native selections and explicit local file commands are described below.

**Verification limit for this extension (2026-11-09):** five portable sizing
tests pass with the pinned compiler and strict standalone Clippy, including
the new fitted-control bounds. Workspace formatting, documentation links and
diff checks pass. Consent/profile, cancellation, indexed receipt projection
and CLI refusal regressions are included in source, but remain unrun:
`cargo check -p fr-native --features linux-desktop --all-targets --locked`
was killed by SIGKILL while compiling the unchanged pinned Asupersync
dependency, before it reached the new runtime integration. Native macOS
builds and a real two-direction transfer/cancel/reconnect run remain required;
the earlier viewer evidence above does not qualify this extension.

### Native macOS file selections

`--control --file-drop` explicitly enables later native uploads on the original
connection. It may accompany initial `--send` paths, `--file-commands`, and an
independently selected `--receive` directory. `--send` alone does not enable
native selection. The host still needs control approval and `--files DIR`.

```sh
# First copy the desired files or directories in Finder.
fr connect NODE_ID --control --file-drop --experimental-native --display only
```

After the original control grant and file attachment are ready, click
**Send copied files** in the viewer's toolbar. That fresh local click reads
the currently copied file URLs and reserves one selection. Copy the files
before connecting or before requesting control: activating Finder during
control triggers the existing focus-loss stop. The button does not switch
applications or open a chooser. It never polls for copied files or sends them
automatically. `--no-clipboard` disables text synchronization independently;
the explicit native file-selection gesture remains available.

The picture also accepts copy-only file-URL drops while the original window
remains focused, visible, current and authorized. A drag that changes focus
cannot restore control or defer itself until a later grant. Ordinary Finder
dragging may activate Finder and therefore be refused; use the copied-file
button for the same-window workflow. The exact AppKit focus/drag behavior
still needs a native run and is not inferred from portable tests.

Each gesture accepts at most eight local file or directory URLs in 32 KiB of
UTF-8 metadata. Remote URLs, nonlocal authorities, file promises, malformed
escapes, duplicate paths and oversized selections refuse. A refused gesture
shows **Send: Selection refused**. Pasteboard metadata is read only after the
click or actual drop, and a change during that read refuses the whole selection.
No promise is materialized, no URL is resolved, and no source is opened by the
AppKit callback. The existing descriptor selection worker performs filesystem
checks off the window/session thread. It checks the original two-second
gesture deadline and original intake eligibility before and after selection;
slow filesystem work cannot refresh the gesture or schedule a retry.

Both native gestures and local commands share the original upload intake,
sender policy, attachment, control lease, cumulative host limits and publication
receipts. Finish the current batch before selecting another. There is one
pending native selection and at most eight native batch results per invocation.
The native progress strip follows the current batch. Its **Cancel** retires
upload selection consent and cancels admitted uploads; it leaves the separate
receive direction intact. Closing, Escape, focus loss, display change and
connection cleanup also fence native selection. A later viewing reconnect or
new F8 request cannot replay it. Cleanup must confirm the original selection
worker has exited before another attempt can begin.

Completion retains initial, native and local-command outcomes with stable
indices. An accepted gesture that expires during preparation remains a refused
result, and an already published file remains published after cancellation.
Paths and names do not enter progress labels or completion history.

**Verification boundary (2026-11-10):** regression targets exercise the shared
production descriptor-selection boundary with real files, original readiness
checks, bounded native ingress, one-use consent, cancellation and fitted chrome.
These do not qualify AppKit pasteboard delivery, Finder interaction, Apple SDK
compilation, or a real upload/cancel/reconnect session. Those native checks are
still required for this extension; earlier Mac viewer evidence does not cover it.

### Later macOS file selections using local commands

`--file-commands` enables later explicit uploads without restarting the desktop.
It may be used alone, alongside initial `--send` selections or `--file-drop`,
and alongside the independent `--receive` destination. An empty initial upload opens no source
and reads no file contents. The original host still needs to approve control
and enable the receiving file directory.

```sh
fr connect NODE_ID --control --experimental-native --display only --file-commands

# Read connection_id from the original foreground session, then its file lease.
fr robot session status NODE_ID --json
fr send NODE_ID --connection-id CONNECTION_ID --status --json

# Use the exact connection_id and lease returned by those status calls.
fr send NODE_ID --connection-id CONNECTION_ID --lease FILE_LEASE \
  --send /absolute/source.txt --send /absolute/source-directory --json
fr send NODE_ID --connection-id CONNECTION_ID --lease FILE_LEASE \
  --status --request 1 --json
fr send NODE_ID --connection-id CONNECTION_ID --lease FILE_LEASE --pause 1 --json
fr send NODE_ID --connection-id CONNECTION_ID --lease FILE_LEASE --resume 1 --json
fr send NODE_ID --connection-id CONNECTION_ID --lease FILE_LEASE --cancel 1 --json
```

The command first authenticates the same-user foreground owner and its exact
node and connection identity, then addresses that original upload's separate
file instance. It opens no new remote connection. macOS uses a private,
owner-checked Unix socket path; the peer credentials are checked at both ends.
The original fixed selection worker pins at most eight explicitly named files
or directories per command, and retains at most 64 admitted command results.
File paths and contents never enter the native progress view or completion
history. Selection and admission share the existing two-second file-command
budget; an uncertain reply requires inspection of the same instance, not an
automatic retry.

Finish the current batch before sending another; a paused batch still owns
the lane. Pause gates new source and record admission while the original
deadline, authority checks, receipts and cleanup continue. Resume changes
that same batch's pacing and cannot restart cancelled or expired work. Cancelling
one request cannot cancel a later batch. Each `--status --request N` reports the
original request's own publication, uncertainty, pacing and cleanup evidence.

The native upload strip follows the latest admitted batch and samples its actual
progress and cleanup. Its **Cancel** button retires command intake and cancels
all admitted upload batches; the independent receive direction stays available.
Window close, escape, display change and connection cleanup also fence the
endpoint synchronously. A later viewing retry or fresh F8 control never reopens
that consent or resubmits a selection. Cleanup must confirm the original socket
and selection workers as well as the file workers before another native attempt
can begin; a blocked local filesystem operation leaves cleanup unconfirmed.

Completion combines the initial selection and later command outcomes with stable
indices. `files_commands.requests` maps each original command number to its
`first_index` and `selected` range, preserving publication receipts even when
a later batch or viewing attempt fails. The initial source's cleanup evidence
cannot certify later independent batches. Original receipt history remains
readable after the endpoint has retired.

**Verification boundary (2026-11-09):** this extension adds production-owner
consent/profile/cancellation tests, shared indexed command/cleanup projection
regressions, and macOS preflight checks for control consent and toolbar space.
Native macOS builds and a live repeated-send/pause/resume/cancel/reconnect run
remain required; portable source review does not establish native qualification.

### macOS text clipboard

The Mac client shares UTF-8 text in both directions on its original controlling
connection. It uses the existing clipboard protocol and host implementation;
the Linux host must enable its clipboard alongside `--input-agent`. A host
without the optional clipboard profile leaves desktop control available and
reports `host_clipboard_unavailable`. Mac hosting still has no clipboard backend.

```sh
# Clipboard is on by default with --control in a macos-desktop build.
fr connect NODE_ID --control --experimental-native --display only --json

# Decline clipboard before connecting; file choices remain independent.
fr connect NODE_ID --control --experimental-native --display only --no-clipboard
```

The viewer reserves a visible **Clipboard / Turn Off** control. Turn Off fences
the original clipboard immediately. Once its attachment is established, that
lane can retire while input, viewing and independent file transfers continue.
Cancellation during incomplete attachment can still require closing the original
connection. Hiding or closing the viewer, losing focus, changing
displays or ending the original control also stops clipboard admission. The
native worker requires both the original grant and a responsive local privacy
control. A viewing reconnect or later F8 control request does not re-enable
the initial clipboard choice. Start a new explicitly controlled connection to
select sharing again.

No text is read before the original grant, bilateral readiness and local privacy
checks. The adapter observes pasteboard revision metadata and reads only a new
eligible text selection. It validates complete UTF-8 within the negotiated
one-MiB ceiling; it does not transfer images, HTML, files or arbitrary pasteboard
types, open URLs, or synthesize a paste. Exact local publication revisions and
protocol stamps suppress echoes; identical text copied later by the user remains
a new selection. Routine metadata polling is throttled, while publication forces
a fresh revision check.

Replacement uses prepared bounded bytes and the original item's deadline. Each
native mutation rechecks the original controller, both privacy switches and
their generations. Turning a switch off and back on cannot revive a prepared
item. If a native call blocks and a prior mutation may already have happened,
the result remains an unknown publication effect. Closing the owner clears
private buffers and waits for actual worker cleanup; it never clears the system
pasteboard or overwrites another application's newer selection. Native OS calls
and pasteboard providers may block or allocate internally; the application does
not claim to preempt or bound those external implementations.

Completion retains the original clipboard evidence even if another lane or a
later viewing attempt fails:

| Field | Observed meaning |
| --- | --- |
| `clipboard_requested`, `clipboard_configured` | The initial local choice and whether it reached the original observer's configuration. Neither proves native readiness. |
| `clipboard_active` | The original native worker actually started after setup. |
| `clipboard_received` | Host text items submitted to the local pasteboard; not proof that an application pasted them. |
| `clipboard_received_unknown`, `clipboard_received_refused` | Uncertain OS effects and refused incoming commits, retained separately from successful submissions. |
| `clipboard_disabled` | The local Off control was used. |
| `clipboard_absence`, `clipboard_ended` | Typed setup absence or the reason an active lane retired. |
| `clipboard_cleanup` | `not_started`, `unknown`, `pending`, `finished` or `failed`, from the original worker. |

The new native backend and toolbar require macOS SDK/runtime qualification.
Portable policy checks, source review and authored native tests are separate
evidence; they do not establish a live Mac clipboard or tailnet result.

## Check local capture, encoding and input readiness

Ordinary `fr doctor` checks the installed Tailscale identity, service port and,
when requested, certificate lifecycle. The explicit local probe mode works
without Tailscale or an async runtime:

```sh
./target/debug/fr doctor --probe-native \
  --worker /absolute/fr-media-worker --encoder nvenc --x-display :0 \
  --xauthority /absolute/Xauthority --input-agent /absolute/fr-input-agent --json
```

Choose exactly `nvenc`, `vaapi`, or `software`; the selected backend is tried
once and never replaced by another. The worker captures one real X11 root-display
picture, encodes it, and applies the normal HEVC IDR admission checks. Its existing
supervisor allows at most fifteen seconds, then ends and reaps the worker group.
This proves one capture and admitted IDR for that display/backend; it does not
measure decoding, presentation, sustained performance, a selected monitor, or a
remote session. A combined capture/encode refusal cannot identify an individual
capture failure, so that separate row remains `not_tested`.

The optional input-agent probe uses its existing five-second startup bound and
discovers the operations the real executor advertises on the selected display.
It checks the core keys, repeat, absolute-pointer and button operations required
by native control, and lists any additional discovered operations. It injects
no input and makes no claim that an application received an event. All probes
finish before network or runtime initialization; no connection or input lease
is created. The selected binaries remain part of the trusted local installation.

The report includes a `local_native_probe` scope, explicit display and encoder,
typed capability rows and actionable refusal codes. It omits executable and
Xauthority paths, captured pixels and codec payloads. Decode, clipboard, audio,
files and unrequested input remain `not_tested`. Exit status is zero when all
requested probes pass, one when a requested probe fails or is blocked, and two
for invalid arguments. Run ordinary `fr doctor` separately for tailnet and
certificate checks; `--port`, `--socket` and `--trust-roots` are rejected in local
probe mode. `--worker`, `--encoder` and `--x-display` are required, while
`--xauthority` and `--input-agent` are optional; neither display selection nor
desktop capture is inferred from the environment.

## Choose a machine in the native window

The behavior below describes the Linux X11 picker. See
[native macOS host and display selection](#native-macos-host-and-display-selection)
for the AppKit shell using the same identity snapshot and connection policy.

```sh
./target/debug/fr connect --choose-host \
  --view-only --experimental-native --display choose
```

Place `--choose-host` immediately after `connect`, in place of the stable node
ID. The first local X11 window lists machines from the installed Tailscale
daemon. After the selection, the existing authenticated connection and host
approval run; `--display choose` then opens the separate display chooser with
that approved host's current catalog. Use `--display only` for the existing
single-monitor policy, or `--control` for an explicit control request.

The machine window shows each candidate's canonical certificate name, stable
node ID and a direct/relayed/unknown route hint from that discovery snapshot.
Names and IDs wrap rather than hiding their distinguishing suffixes. Up to
1024 candidates are retained in name-then-ID order, with keyboard and mouse
pagination through the entire list. Arrows move the highlight, Page Up/Down or
the wheel changes page, and Home/End reaches the first/last candidate. A fresh
Enter press and release, or a left-button press and release on the same row,
makes the choice. Escape or closing the window cancels. The picker requires
local focus and a supported, pinned keyboard map; changed mappings refuse the choice.

Discovery is local metadata, not a desktop probe or admission result. No remote
session, file source or destination is opened while the user is choosing. The
selected ordinal indexes the same immutable catalog that supplied the visible
rows; addresses are never retained in the choice. The stable node ID and exact
certificate name are pinned together and freshly resolved before every dial,
including reconnect. A renamed or reassigned peer refuses rather than silently
connecting to a replacement. Strict TLS, host approval, role/capability checks
and fresh display selection remain in the ordinary connection path.

The native window must map within two seconds, and the original choice budget
is 120 seconds. Activity and page changes do not extend it. Cancellation also
works through Ctrl-C. A choice becomes usable only after the original native
worker has actually joined. A failed open, timeout, cancellation or incomplete
cleanup opens no remote connection; cleanup has its own two-second wait limit.
The picker is a pre-session owner and grants no observation or input authority.

Ordinary connection options, including `--port`, `--trust-roots`, `--socket`,
`--x-display`, `--fit` and `--attempts`, still apply. `--saved`, `--hosts-file`
and `--by-name` cannot be combined with this mode. Robot commands require an
explicit target and refuse interactive machine selection. The selected machine
is not automatically saved as a profile; [saved hosts](../NATIVE_SAVED_HOSTS.md)
remain an explicit local choice.

Human output prints the selected name and stable node ID before connection.
With `--json`, the `host_selected` event has `schema_version: 1`, `node_id`,
`certificate_name`, `port`, `selection_only: true`,
`fresh_lookup_required: true`, `remote_session_started: false` and
`input_requested: false`. This selection event is not connection success.
Use its stable ID with `fr disconnect NODE_ID` to address a later foreground
session.

The discovery suite passed 19 focused tests and strict Clippy. The exact native
picker sources passed eight lifecycle tests, including cancellation while a real
thread-local destructor blocks cleanup, and three actual Xvfb integration tests.
Those select ordinal 1023 from a 1024-row window with End/Enter, cancel through
Escape, and drop a live picker while checking original native cleanup. The C
boundary also passed compilation with `-Wall -Wextra -Werror`. These are component
results, not installed-tailnet or physical-desktop qualification.

Strict component Clippy also passed for the native owner, lifecycle tests and
Xvfb integration tests. A joined but unconsumed choice remains subject to its
original deadline; returning focus requests a fresh local input baseline.

CLI/parser and pinned-selection regressions are included but remain unexecuted:
the full CLI dependency graph's pinned Asupersync compilation exceeded shared
memory. Complete CLI/TLS and live two-machine qualification for this entry point
remain pending under bead `fr-nyab` (plan sections 6.4 and 16.1).

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
cargo build -p fr-native --features linux-desktop,linux-audio --bin fr --bin fr-opus-worker --locked
./target/debug/fr connect NODE_ID \
  --view-only --audio --experimental-native --display only \
  [--audio-server /run/user/1000/pulse/native] [--audio-sink NAME]
```

`--audio` is an explicit local request: without it the client offers no audio
capability at all. With it, the view-only offer adds the OPTIONAL
`native-audio-down` capability. The host must run `frd run --audio` (its own
local enable); a host without it simply omits the capability, the session
continues with video only, and the completion record reports
`audio_absence: "host_did_not_offer"`. `--audio` with `--control` adds the same
optional capability to a controlled share; audio and input stay independent (a
stalled Opus child cannot stop control or lease renewal, and a host without
`--audio` keeps control without audio). Builds without the `linux-audio` feature refuse `--audio`
(`audio_unavailable_in_build`).

The attached `audio-down` channel carries `AudioConfiguration`/`AudioStop` on a
reliable lane and `AudioPacket` datagrams (Opus, 48 kHz stereo, 20 ms). Every
record is validated, including the negotiated packet/sample bounds, before the
real libopus decoder in the restricted per-epoch child sees it. The client configures its local PulseAudio output
(the server from `--audio-server`, else `PULSE_SERVER`, else
`$XDG_RUNTIME_DIR/pulse/native`; the server's default sink, pinned at stream
start, unless `--audio-sink` names one), and waits for both that output and the
actual child decoder configuration reply before answering `AudioConfigured`.
Only then does it receive packets. A host `AudioStop` fences queued audio at once. A local playout
failure (for example a missed device slot) drops that stream without flushing old
samples into the next one and asks the host for a fresh audio epoch, at most
eight times per session. Audio never feeds video presentation or freshness.

Install the matching `fr-opus-worker` beside `fr`; a missing image is the
pre-connection refusal `audio_decoder_unavailable`. The child requires Linux
x86-64 syscall confinement before reading a configuration or packet, and receives
no PulseAudio/X11 connection, session credentials or input authority. Unsupported
architectures and confinement failures refuse audio without an in-process
fallback. The session thread polls a single pending result and never calls
libopus or waits for process I/O. Late PCM keeps its original arrival deadline
and device slot: it is discarded, not retimed. A reset cannot overlap a previous
unreaped decoder. Native packages, loader and kernel remain trusted. See
[`NATIVE_OPUS_PROCESS.md`](../NATIVE_OPUS_PROCESS.md) and
[`SECURITY.md`](../SECURITY.md).

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
input capture to this window. Keys (with repeat), absolute pointer, buttons
and wheel notches (one line per notch, both axes) are captured; text, pixel
scrolling, relative pointer and audio are not. The wheel is requested only when
the host offers the optional `native-input-line-scroll` capability: `frd run`
probes its input executor once at startup and offers it only when that
executor can inject line scrolling (one X screen, wheel buttons 4-7 mapped).
Otherwise control is granted without the wheel, wheel notches stay local, and
the completion reports `wheel_unavailable: true`. An executor that lacks keys,
repeat, absolute pointer or buttons makes `frd run --input-agent` refuse at
startup (`control_capability_missing`, or `input_agent_unavailable` when the
probe itself fails) instead of failing every later grant. The clipboard and
file sending are separate opt-ins, described next.

The visibility witness is stated, not overclaimed: a frame counts as visible
when the presenter completed `SubmittedToCompositor` into the still-mapped
window (unmapping stops the session). Occlusion by another local window is not
detected, and `physical_visibility_proven` stays `false`.

Control is requested at most once per invocation. The local decision is spent
before submitting that request, even when its admission or reply is uncertain.
After a retryable transport/media failure, the remaining `--attempts` budget
(default five) may restore **viewing only**, after confirmed native cleanup.
The new attempt repeats identity/admission/approval and creates a new decoder,
window and mapping. It never requests control again, attaches native input,
or configures another clipboard, upload or download owner. Existing transfer
results remain available and no selection or consent is replayed. Use
`--attempts 1` to disable automatic retries; a new grant needs a new
`fr connect --control` command.

Explicit control revocation/refusal and input expiry, user close, local escape,
focus loss, native capture failure and unsuccessful cleanup remain terminal.
A genuine native stop or authenticated host terminal report that races with a
link failure still prevents reconnect. Existing observation-session expiration
and renewal-timeout retries remain governed by the unchanged supervisor
classifier; they cannot restore old control.

The completion record keeps `role: "control"` for the invocation and distinguishes
`last_attempt_mode` (`control_capable` or `view_only`),
`viewing_reconnects` (new viewing desktops actually opened),
`control_request_attempt` and `control_attempt_end`. `control_requested`,
`control_granted`, `control_capabilities_granted` (the original host's exact
grant, e.g. `["keys", "repeat", "absolute_pointer", "buttons", "line_scroll"]`,
or `null`), `wheel_unavailable`, `input_results` and
`input_submitted_to_os` describe historical control effects. They do not claim
that the final viewing attempt owns input; the counts are host-reported stages,
not local proof of an effect. `control_close_exchange` and
`control_lease_report` retain the original controller's evidence separately
from the final connection's `close_exchange` and `lease_report`. Both human
output and terminal errors preserve that distinction and the original
clipboard/file outcomes. See [native recovery and reconnect](../NATIVE_RECOVERY_RECONNECT.md).

A stale or unknown view suspends control input instead of ending the session
(plan 11.3).
- While suspended, captured keys, clicks and motion are dropped, never queued.
- The host releases every key and button it holds for this client.
- Input resumes on the same lease once the view is fresh again and a ticket
  issued after the host's own lapse arrives. That is at least about one second
  after the lapse.
- A release of a key held across the suspension is dropped, since the host
  already released it.
- A key or button pressed while suspended is dropped. So is its release or
  repeat, even after input resumes, since the host never saw the press.
- A suspension longer than 10 s ends the session with `view_stale`.
- `input_suspensions` and `input_suspended_ms` report how often and how long
  input was suspended. `input_dropped_suspended` counts the events dropped
  because of a suspension.

Local input has a 100 ms dispatch bound from when it was captured. While a new
picture waits for its visibility confirmation, the client holds input, because
which picture the user saw is not yet known. Under load that wait can outlast
the bound. Control continues when it does:
- An aged key press, repeat, pointer motion, scroll or text is dropped, never
  sent late. The release or repeat of a dropped press is dropped with it,
  since the host never saw the press.
- A release of a key or button the host holds, and held-state
  reconciliation, is sent late rather than leaving input held on the host.
- `input_dropped_aged` counts the dropped events.

The controlled share forwards the host's cursor too (`frd run --input-agent`
selects the optional `remote-cursor` capability the client offers), and exactly
ONE pointer renders it. Before the grant the viewer composites the overlay as
`--view-only` does. After the grant, while the host pointer is where this
client's own absolute input put it, the local pointer is the owner: the viewer
window's X cursor becomes the host's confirmed shape (an ARGB RENDER cursor set
by a separate thread on its own X connection, alongside input capture), and no
overlay is composited. When the host pointer is somewhere this client never sent
it (the host moved, warped or confined it) or the local pointer leaves the
window, the local pointer over the window is blanked FIRST and the overlay then
shows the confirmed position; the owner switches back only after the host is
confirmed to follow new input. A hidden, locked or host-composited cursor draws
nothing, and a shape that has not arrived is the built-in crosshair. Positions
are confirmed host state for drawing only, never input. Evidence is the
namespace e2e on two Xvfb displays
(`crates/frd/tests/native_host_linux_serial/real_control_cursor.rs`), which
reads the viewer window's cursor with `XFixesGetCursorImage` and its pixels with
`XGetImage`. Not evidence for a GPU compositor, HiDPI cursor scaling (the shape
is drawn at the host's pixel size, also with `--fit`) or Wayland.

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
(for example, control never granted), `clipboard_receive_expired` (a completed
lane expired before native worker readiness was observed) or `not_requested`. `clipboard_ended`
is `null` unless an ACTIVE clipboard retired while the session ran on. Then it
names why: `clipboard_receive_expired` (a partial or complete record reached
its original framing deadline, locally or at the peer), `clipboard_item_expired`
(retained outgoing or uncertain native work outlived its original deadline),
`clipboard_disabled`, `clipboard_authority_ended` or `clipboard_ended`. Control
continues, and the clipboard is not re-attached under that grant. Clipboard text
never appears in output, errors or logs.

An incoming item that expires while its complete record is still queued or
explicitly deferred before native preparation is refused individually. The
expired bytes are cleared, its sequence cannot be reused, and a fresh later copy
can cross on the original lane after the view becomes ready again. This case
leaves `clipboard_ended` unset. Partial transport receive expiry retires the
completed clipboard lane without restarting its lifetime. Uncertain OS effects,
privacy-switch changes and lost authority retain their existing terminal behavior.

Evidence is the namespace e2e with real X11 selections on two Xvfb displays
(`crates/frd/tests/native_host_linux_serial/real_clipboard.rs`), described
below. It is not a desktop clipboard manager (no history, no PRIMARY), not
Wayland, not images, and not a live tailnet.

### Send files (`--send PATH`, with `--control`)

```sh
./target/debug/fr connect NODE_ID \
  --control --send /home/me/report.pdf [--send /home/me/photo.jpg]... \
  --experimental-native --display only
```

`--send` names a regular file to send into the host's drop directory after
control is granted. It is repeatable, at most 8 times, with absolute paths
(no `..`). Every path is classified by type before any network I/O and the
whole command refuses (exit 2) on the first problem, naming only the 1-based
position: `send_symlink`, `send_directory`, `send_special_file` (device, FIFO,
socket), `send_missing`, `send_unreadable`, `send_name_not_portable`,
`send_duplicate_name`, `send_changed` or `send_too_many`. `--send` without
`--control` refuses with `send_requires_control`. With `--clipboard` too, the
client offers the combined profile and both lanes run on the one controlled
session (each attaches only under the granted input attachment; the optional
decoder-load extension is left out of that profile to stay within the
16-capability offer limit). Files are opened with `O_NOFOLLOW | O_NONBLOCK` and must be the same
regular file that was classified; nothing is read before the grant.

The host opts in separately with `frd run --input-agent PATH --files DIR`
(`--files` alone refuses with `files_requires_input_agent`). `DIR` must be an
existing absolute directory owned by the user running `frd`, not group- or
other-writable, reached without symbolic links; otherwise `frd` refuses before
any display, worker or network I/O (`files_directory_missing`,
`files_directory_not_directory`, `files_directory_symlink`,
`files_directory_not_owned`, `files_directory_writable_by_others`,
`files_directory_relative`), naming no path. The directory is pinned by
descriptor: renaming or replacing the path later never redirects writes.
`--files-max-file-bytes N` (default 268435456) bounds each file and
`--files-max-session-bytes N` (default 1073741824) bounds the cumulative
declared bytes of one controlled session, refused offers included; both refuse
an offer before its staging file exists. At most one file is staged at a time
and 64 transfer attempts are admitted per session. There is no daemon-lifetime
total: sessions are sequential (one controller at a time) and each has its own
budget, so free disk space and `--once` remain the operator's outer bound.

With `--send` the connection additionally offers three OPTIONAL capabilities
(`native-file-receive`, `file-atp-full`, `file-channel-scope`); a host without
`--files` drops them and control proceeds. Otherwise the client installs its
drop expectation at the start of its first controlled turn, and the host offers
the one-use lane only after the controller's first lease renewal, on the same
connection, under the active input attachment. The attachment's own binding
names the host's directory: the client never sends a host path. The selected
files then travel in order on the existing batch sender (ATP full-object
profile, hashed and read on the sender's bounded disk thread, rate-limited on
both sides). The host's bounded disk worker re-checks the controller's input
lease and its file permission before every write and immediately before
publication, verifies the ATP digest, stages under a private `.fr-part-*` name
and publishes with an atomic no-replace rename. The host name is the file's
final path component, unchanged (a non-portable name is refused locally, never
rewritten). A name the host already holds is refused (`host_conflict`) and the
selection stops there; nothing is overwritten. (PROTOCOL.md's general "conflicts
keep both" is NOT what this slice does: `fr-files` has a keep-both policy as an
explicit local choice, but `frd run --files` keeps the drop directory's existing
default, refusal; choosing between them is an open decision.) Revocation or expiry of the
lease fences an in-flight transfer: its staging file is removed and nothing
appears under the final name.

The completion record adds `files_requested`, `files_sent` (per index: the
host-reported published `bytes` and whether publication was `durable`),
`files_refused` (per index: the local `bytes` and a typed `reason` such as
`host_conflict`, `host_limit`, `host_expired`, `publication_unknown`,
`interrupted_expired`, `not_started` or `not_sent`) and `files_absence`: `null`
when the lane carried the selection, otherwise a typed code such as
`host_files_unavailable` (the host did not enable it), `control_not_granted`,
`files_setup_failed` or `not_requested`. Every requested index appears exactly
once. File names, paths and contents never appear in output, errors or logs.
A host-reported publication is not an independent observation of the host's
disk; an uncertain publication is reported as such and never resent.

Evidence is the namespace e2e (`crates/frd/tests/native_host_linux_serial/real_files.rs`),
described below. This is viewer to host only: no downloads, directories,
resumption, synchronization, picker or drag-and-drop UI, and not a live tailnet.

`--experimental-native` is also mandatory because the native transport remains
unqualified. It is a development opt-in, not a change to any protocol, admission
or release gate. There is no alternate QUIC stack, runtime or codec fallback.

Trust roots are local configuration, never supplied by the peer. By default they
are the distribution bundle `/etc/ssl/certs/ca-certificates.crt`, which verifies
the Let's Encrypt certificates Tailscale issues. That default may be a symlink
(Fedora/RHEL and Arch link it into their extracted trust store): it is followed
only through root-owned links in root-owned, non-group/other-writable
directories to a root-owned file, at most eight hops. `--trust-roots` selects
another file, which must be the regular (non-symlink) PEM file itself. Either way
the bundle holds at most 512 certificates and 1 MiB. Certificate checking cannot be disabled. The media worker defaults to
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
selection. An ordinary interactive viewer can be resized after its first frame
is ready. A checked native window-manager resize immediately fences the original
input and session, waits for positive native cleanup, and opens fresh viewing
fitted to the requested window. It applies the local display preference to a
fresh catalog and creates a new viewport mapping and decoder epoch. Original
control, clipboard and file consent is not restored. Explicit resizes share the
32-attempt lifetime ceiling with display changes and automatic retries, without
resetting the automatic retry allowance.

The checked target must fit the selected image and local screen limits, with
even dimensions from 16 to 8192 and at most 16,777,216 pixels. Unsupported or
synthetic sizes, hide/unmap, focus loss during resize, window loss, adverse host
reports and uncertain cleanup do not authorize reopening. One-shot observation
and foreground automation retain fixed window behavior. Completion preserves
each accepted resize in `window_resizes`, including the original close exchange
and lease report when available; missing remote cleanup evidence remains unknown.

In an already-running view-only session, normal window-manager close now retires
local media and sends one `CloseRequest` over the original connection, retaining
the exact optional host `Closed` report. The exchange has the existing 250 ms
maximum (shortened by earlier session deadlines); duplicate close events cannot
restart it. A native 500 ms fallback stops a parked close handler. Bootstrap and
control-capable windows still stop immediately. No close replays input, reuses
control, or automatically reconnects a normally closed window.

Direct stop, emergency input, hide/window loss and SIGINT/Ctrl-C, SIGTERM
or SIGHUP remain immediate cancellation. A signal cancels the original supervisor
but does not abandon its future: the same independent cleanup path still runs. Native cleanup failure takes precedence over a friendly
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
JSON record distinguishes stopped observation and confirmed **local** cleanup,
and retains `transport_qualified: false` and `physical_visibility_proven: false`.
The legacy `cleanup_confirmed` field refers only to the local supervisor's
completed cleanup. It never describes the remote host.

Both stopped completions (observation and control) include
`last_attempt_media`, content-free counters for the last attempt: `decoded`
pictures, `repair_requests` admitted to transport, `recovered_streams`
(completed recovery handshakes), and `presented_age_reports` with
`presented_age_p50_ms_at_most` / `presented_age_p95_ms_at_most`. The last two
are the presented source-age upper bound at each admitted presented report,
rounded up to 10 ms buckets. They exist only for control sessions: view-only
has no clock exchange, so its age fields are `null`. The whole field is `null`
when that attempt never opened a viewer. None of these counters proves
delivery, visibility or latency.

Observation completion now includes `close_exchange`: `null` means no completed
exchange, including emergency cancellation or unpolled abandonment. Otherwise it
contains independent `request_acknowledged`, `transport_completed`, and
`host_report` fields. `host_report: null` means no host report, even when the request
was acknowledged. A report retains its typed `reason`, `cleanup_stage`
(`unconfirmed`, `complete`, or `incomplete`) and `outstanding_effects`. Unknown
effects use `known: false, pending: null, uncertain: null`; explicitly reported
zero counts use `known: true, pending: 0, uncertain: 0`. They are not interchangeable.
A failed ACK flush does not erase a received report or upgrade its stages.

A local cleanup failure or signal keeps any already-collected exchange in the
failure JSON while retaining the original error/exit code. Human output also
labels the host stages as **host-reported**, separately from local cleanup.
No report emits peer strings, native error diagnostics, input values or session
identifiers. The controller's existing lease-revocation format is unchanged.
See `NATIVE_WINDOW_CLOSURE.md` for the native ownership path and evidence scope.

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

`--control`, `--clipboard`, `--send` and `--audio` end to end are exercised by the namespace suite, not by these commands.
Build the real binaries and the test executable, then pass that executable to
`scripts/test_linux_serial_lifecycle.sh` (set `FR_NS_SUDO=1` where unprivileged
user namespaces are restricted). It runs the ignored namespace suite serially,
including the `real_control::`, `real_clipboard::`, `real_files::` and `real_audio::` tests of
`crates/frd/tests/native_host_linux_serial.rs`:

```sh
cargo build -p fr-native --features linux-desktop,linux-displays,linux-input,linux-clipboard,linux-audio \
  --bin fr --bin fr-media-worker --bin fr-input-agent --bin fr-opus-worker --locked
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
the clipboard child, after which copies cross neither way. The `real_files::`
tests run the production `frd run --input-agent --files DIR` with a private drop
directory and the shipped `fr connect --control --send`, and observe the drop
directory directly: a 3 MiB random file with a non-ASCII name lands under exactly
that name with an identical SHA-256 (the completion reports it sent, by index
and size); a second file whose name the host already holds is refused
`host_conflict` with the original bytes unchanged; a client frozen (SIGSTOP)
mid-transfer past its lease leaves no staging file and nothing under the final
name; and a host without `--files` keeps control and reports
`host_files_unavailable`. A host that writes zeros and a client that never starts
the transfer were planted and both fail the first test. The tailnet
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

## Restricted audio-decoder verification

The process-decoder integration adds four explicitly selected CLI tests using
the actual output owner, private PulseAudio null sink, real Opus child and an
independent native audio monitor. They verify played sound, no acknowledgement
or fallback after child startup failure, bounded handling of a frozen decoder,
and retirement before an output reset creates its next child. These tests use
local protocol/permission fixtures; they do not rerun the two-machine namespace
scenario described above. Invoke them explicitly after building both binaries:

```sh
cargo test -p fr-native --features linux-desktop,linux-audio --bin fr -- --ignored --test-threads=1 linux::audio::output::tests
```

In the implementation environment all four passed, as did 52 ordinary CLI
tests, six existing native Opus playout tests and all 13 existing PulseAudio
playout/device tests. The earlier core playout and process-codec tests remain
separate evidence. This used verified `bd177a8` first-party source plus the
changed audio paths, the pinned compiler and matching retained CI libraries.
Unchanged viewer C archives were reused because local XCB RENDER headers were
absent; the new sandbox C was rebuilt. All modified preimages matched current
main. A cold dependency build, complete latest-workspace run, physical audio
device, desktop PipeWire session and live-tailnet qualification remain untested.


## Bounded robot shortcuts on the original foreground owner

Open an explicit automation owner in one foreground process:

```sh
fr robot session open NODE_ID --foreground --role control --accept-input \
  --experimental-native --display only
```

Keep its native viewer focused. This mode requests control once and disables
human keyboard/mouse forwarding, reconnects and auxiliary sharing lanes. The
host still needs its input agent and any configured local approval. In another
process, obtain that invocation's `connection_id` with `fr robot session status
NODE_ID --json`, then request its current input observation:

```sh
fr robot input NODE_ID --connection-id CONNECTION_ID --status --json
```

When `ready` is true, use the returned lease comparison handle, observation ID,
and next request ID. A command must include its maximum observation age in
milliseconds (1–1000). For example, this requests the physical left Ctrl+Shift+P
shortcut using decimal USB page-7 usages:

```sh
fr robot input NODE_ID --connection-id CONNECTION_ID \
  --lease LEASE_HANDLE --request-id REQUEST_ID --observation-id OBSERVATION_ID \
  --max-observation-age-ms 250 --chord 224,225,19 --json
```

`--chord` accepts one to four distinct modifiers from 224–231 followed by one
non-modifier physical key. Modifiers press in ascending USB usage order, the key
presses and releases while they remain held, then modifiers release in reverse
order. Reordering the same modifier set produces the same canonical request.
Duplicate modifiers, missing keys, unsupported usages and longer sequences
refuse before dispatch. `--key` remains a single physical key tap; `--click` and
`--scroll` retain their existing bounded forms. This interface provides no text
insertion, arbitrary batches, repeated shortcuts or persistent held keys.

Every transition waits for its own matching reliable host receipt from the
original session and lease. The original observation, view generations and
request deadline govern the whole chord; successful intermediate keys and fresh
status reads do not extend them. A distant or stalled host can therefore leave
a confirmed prefix and a terminal partial result. Partial or uncertain input
stops that owner and invokes its existing held-key cleanup. Cleanup does not
undo previously submitted keys, and the command is never automatically retried.

Results report OS submission, partial submission, refusal or unknown effects.
They do not prove that an application accepted the shortcut or changed its UI.
An identical request ID and unchanged arguments retrieve the retained outcome;
changing its arguments is refused, and evicted IDs stay consumed. Do not issue
a fresh request ID to replay uncertain input. These checks have deterministic
parser, action-order, receipt-ledger and owner-state regression coverage; they
are not a new live network or desktop qualification claim.

## One-shot native screenshots

```sh
fr robot observe NODE_ID --native-once --experimental-native --display only \
  --screenshot /absolute/new-screen.png --json
```

The `--native-once` prefix immediately follows the node. This opens one view-only
native connection with the existing installed-tailnet identity, strict TLS and
any configured host approval. It requires a local X11 display and the installed
media worker. No control, clipboard, files or audio is requested; no reconnect
is attempted. Add `--fit 1280x720` to bound the local viewport without changing
the remote display. Each requested dimension is capped at the source dimension.

The sandboxed worker exports its first submitted viewport, bound to the original
frame, codec configuration and recovery generation. Publication waits for the
session's successful operation and confirmed local native cleanup. The PNG
contains the rendered viewport, including fitting/letterboxing and any remote
cursor overlay present in that frame; it excludes the native OS pointer. Its
pixel dimensions are separate from the source display dimensions in the report.

The destination must be new: existing files and final symlinks, including
dangling symlinks, refuse without replacement. Unix files are created with mode
0600. Encoding and validation finish before the destination opens. A later
write or sync failure reports an error and may leave a partial new file; no
successful artifact is reported. The pathname stays local and never reaches
the worker or host. Raw pixels are limited to 32 MiB, with under 11 KiB of
additional PNG framing; the writer uses stored zlib blocks and adds no image
codec dependency.

JSON includes the artifact's frame and geometry binding, configuration/recovery
generations, viewport dimensions, MIME type, byte count, SHA-256 and local path.
Capture/presentation wall times and source freshness remain unknown. Submission
to a compositor does not establish physical visibility or input authority.
Existing explicitly enabled owners can also export the current retained image,
as described below. Without the flag, observation retains metadata only.

**Verification scope:** the focused harness ran 58 actual-source options,
observation parser, report and PNG writer tests, followed by strict Clippy.
Independent zlib/Pillow decoding checked ten generated images, including fitted
viewports; a complete independently generated PNG fixture is also asserted by
the Rust tests. The harness omits native startup and closing transport adapters.
The full `fr`/session regression build remains blocked by the pinned upstream
Asupersync compiler exceeding this environment's 8 GiB memory limit; the new
full-session regression has not run. These checks do not qualify a live tailnet,
GPU compositor or physical display.

## Screenshots from an existing foreground owner

Open one explicit view publisher, then leave that process running:

```sh
fr robot session open NODE_ID --foreground --role view --publish-observations \
  --experimental-native --display only
```

In another process, identify that invocation and request one image:

```sh
fr robot session status NODE_ID --json
fr robot observe NODE_ID --connection-id CONNECTION_ID \
  --screenshot /absolute/new-screen.png --json
```

The same command also reads an owner opened with `--foreground --role control
--accept-input`. Ordinary `fr connect` owners refuse image export. The local
kernel authenticates the caller, and the public connection ID pins the exact
original owner on the same socket. This opens no new host connection and requests
no new control lease. The existing host approval and selected display still
apply. The owner remains running after the image is delivered.

Only an explicit screenshot request copies the worker's retained presentation;
it does not recapture the desktop, advance the decoder, or start recording.
There is one bounded export per owner, including pending worker work and a slow
local reader, with an original two-second budget. The response carries at most
2048 bytes of metadata and 32 MiB of raw pixels plus the fixed image header.
The caller validates the complete response and the owner's final success seal
before opening the file. Recovery, stop, changed generations, expiry or a busy
export produces a refusal; no replacement picture is automatically selected.
PNG encoding and filesystem writes occur only in the requesting CLI process.
The output pathname never reaches the owner, worker or remote host.

### Observe, then act on that same frame

A screenshot from an `--accept-input` owner can return
`data.current_input_precondition: true` and a `data.control_authority` object
containing `lease`, `observation_id`, `next_request_id`, geometry/viewport/codec/
recovery generations, `frame` and `source_age_upper_ms`. These are the original
comparison handles for the image's frame. To request an action, use its returned
lease, observation ID and next request ID with `fr robot input` and the same
connection ID. Coordinates remain signed host desktop pixels, not fitted PNG
coordinates; the source display geometry and viewport dimensions are reported
separately. A screenshot does not prove application-level success.

Do not insert an input `--status` query between observing and acting: that query
creates a new pin and invalidates the old unsubmitted observation. A concurrent
status/input request can also refuse an in-progress export. Neither an image
nor a fresh status refreshes an already accepted action or authorizes its replay.

Original source age includes the local transfer and PNG-writing time. At or
above 1000 ms, the result clears input readiness and its lease/observation/view
handles while preserving the historical artifact. The input command independently
checks its requested maximum age, original lease and all generations again at
submission. If the old observation is no longer useful, obtain a new one and
make a new decision. No action is automatically retried. A view-only screenshot
always has null control authority and unknown host source freshness.

The new-file, mode0600, size, hash, fitted viewport and cursor rules in the
one-shot section apply here too. `--max-receipt-age-ms` limits local receipt age,
not host source freshness. It is checked before creating the file and after
writing: if it expires during the write, the error explicitly reports that the
new file was written and supplies no current input precondition. File write or
sync errors may leave a partial new file.

**Verification scope:** 111 focused tests and strict Clippy passed for the actual
parser, owner ledger/capture methods, mailbox, metadata/framing, report and PNG
writer source. Tests cover concurrent pin replacement, generation changes,
retirement, cancellation, source-age expiry and real file publication. The
harness does not execute socket read/write, native startup or full session
integration. Real abstract-socket regressions are authored but remain unrun
because the pinned upstream Asupersync compilation exceeds this environment's
8 GiB memory limit. This is not a new live-tailnet or physical-display result.

# Windows client

`fr-windows` uses the installed Tailscale service to find a host and revalidate
its identity before opening a native viewer or fetching its directory. The
viewer uses Media Foundation decode and D3D11 presentation. The same session
core serves the iOS app, whose named connection path remains available through
its existing C interface.

Build on Windows with the repository's pinned Rust toolchain. Supply the two
public ISRG root certificates as local PEM files in one absolute directory:
`isrg-root-x1.pem` and `isrg-root-x2.pem`. Use the ISRG Root X1 and X2 roots
from your installed system trust data or their official distribution, not an
intermediate certificate or a combined CA bundle. Set that directory explicitly
before building:

```powershell
$env:FR_MOBILE_ISRG_ROOTS_DIR = 'C:\build-inputs\isrg-roots'
cargo build --locked --release -p fr-windows
```

This input is used only for Windows targets, including cross-compilation.
The build accepts exactly one certificate per file, bounded to 8 KiB of PEM
and 4 KiB of DER, and checks the decoded DER against these existing SHA-256
pins before embedding either certificate:

| File | SHA-256 of DER |
|---|---|
| `isrg-root-x1.pem` | `96bcec06264976f37460779acf28c5a7cfe8a3c0aae11a8ffcee05c0bddf08c6` |
| `isrg-root-x2.pem` | `69729b8e15a86efc177a57afb7171dfc64add28c2fca8cf1507e34453ccb1470` |

A missing, malformed, extra, swapped or different certificate fails the build;
the directory cannot add another trust anchor. There are no build-time
downloads. If this variable is absent on a build Mac, Windows cross-builds
continue to export the same pinned roots from the system keychain. Apple
targets always use that keychain path. Runtime hostname and certificate-chain
verification use the same two embedded roots on every supported platform.

Run the resulting executable in the interactive desktop session. Tailscale
must be installed, signed in and running on this machine. The remote host must
be running FrankenRemote and its own admission policy must allow this viewer.

## Find and select a host

Run `fr-windows` or `fr-windows view` without a destination to open the native
machine picker. It lists the eligible machines from one bounded, authenticated
installed-Tailscale snapshot. Select a row and choose **Connect**, press Enter,
or double-click it. **Cancel**, Escape, or closing the picker starts no session.
The local choice expires within two minutes of starting discovery. Reading
the snapshot, constructing the window, choosing a row and closing the picker
all consume that same budget; the connection's startup budget begins only
after the choice and confirmed picker cleanup have completed.

The click must come from after this picker was shown and remain fresh when
the native selection read and window cleanup complete. Cancel, Escape or a queued close can
still cancel a pending Connect during the same native event batch. A queue
flood, clock regression, expired snapshot or unconfirmed window cleanup
refuses the choice. The command reports the reason and asks you to start a
new choice; it never retries that click against a replacement catalog.

```text
fr-windows
fr-windows view --control
fr-windows view --control --no-clipboard --ipv6 --display choose
```

The picker retains the exact stable device ID and canonical certificate name
shown in its original catalog. It never interprets an old row number against a
new discovery result. After selection, the ordinary installed connector
revalidates that identity through the daemon before dialing and on renewal.
A removed or renamed device cannot silently turn the choice into another host.

The sampled direct/relayed path labels are informational. They do not claim
the machine is online, that FrankenRemote is running, or that a desktop is
available. No remote probe or observation starts while the picker is open.
An empty snapshot remains empty; it never falls back to a guessed hostname.

All viewing options retain their existing meaning with the picker. In
particular, choosing a machine alone starts an observing session; `--control`
still uses the host's separate admission and grant flow. Explicit destination
commands remain available, and `directory` still requires a destination:

```text
fr-windows hosts
fr-windows view <stable_id-or-full-name>
fr-windows directory <stable_id-or-full-name>
```

`hosts` reads one bounded snapshot from the installed service's authenticated
LocalAPI. Its JSON document contains:

- `machines`: each eligible peer's `stable_id`, canonical `name`, current
  Tailscale `addresses`, and sampled `path` (`direct`, `relayed` or `unknown`).
- `readiness: "not_probed"`: listing a machine says nothing about whether
  FrankenRemote is running or whether its desktop is available.
- `excluded`: counts of shared, expired and otherwise unusable peers.

No remote probes run during this command. An empty list is a valid result.
Select one exact stable ID or a full name from the listing. Full names accept
ASCII case differences and a trailing DNS dot; short hostnames, URLs and
ambiguous choices are refused.

Each `view` or `directory` command reads its own current snapshot and retains
the selected stable ID and certificate name. The connection then resolves both
again through the installed daemon. An unavailable daemon or refused selection
ends the command with an explanatory, content-free error.

## View, control and choose an address family

```text
fr-windows view remote.example.ts.net
fr-windows view remote.example.ts.net --control
fr-windows view remote.example.ts.net --control --no-clipboard
fr-windows view remote.example.ts.net --ipv6
fr-windows view n-selected --ipv4 --port 8443 --seconds 30
```

Replace the sample name or ID with a value from your own listing. The default
port is 8443. Without a family flag, the existing native connector prefers an
authenticated IPv6 route and can try its authenticated IPv4 alternative before
application startup. This works when Windows MagicDNS answers only an A record
but the host serves IPv6: the installed metadata supplies the node's current
address families.

`--ipv4` and `--ipv6` are exclusive choices. A missing selected family refuses
the connection. These flags never add an address absent from the authenticated
metadata.

`view` starts an observing session. `--control` opens a control-capable profile
and requests control once through the existing host admission and grant flow.
`--seconds N` requests an orderly close
after a positive number of seconds. Closing the window also closes the session.
The viewer prints its existing JSON completion document with counts, timings
and the typed ending.

## See the remote cursor

When the host sends a separate cursor, the viewer draws its confirmed shape
and position over the decoded picture. Cursor motion, shape changes and hiding
repaint even when the desktop has no new video frame. The overlay follows the
picture's scale, hotspot and clipping; it cannot draw into the letterbox.
Hosts that already include their cursor in video keep that existing picture
without a second overlay.

During control, the ordinary Windows pointer remains the local cursor while
the host's confirmed position follows submitted input. If the host moves or
confines its pointer elsewhere, its confirmed overlay appears and the local
pointer is hidden only over the actual picture. The local pointer is restored
after the overlay has been erased, or when leaving the picture, opening a
local dialog, ending control or closing the view. This client does not yet
install the host's shape as a zero-latency native Windows cursor; that path
still uses the default local pointer.

One bounded 256-by-256 cursor bitmap is composited on the existing GPU device;
decoded video stays on the GPU. A replacement briefly retains the old and new
cursor bitmaps. Reconnect and decoder reconfiguration retire the former
shape; an old cursor cannot be shown over a replacement decoder's picture.
Cursor-only draws retain the original video frame ID and count separately
as `repaints` in completion JSON. They provide no new video freshness or input
authority. Live Direct2D rendering and cursor handoff qualification on Windows
remain pending.

## Request or stop control in the viewer

The native **Input** menu now contains **Request control**, **Stop control and
keep viewing**, **Change display…**, and **Send text…**. The window title shows whether it is waiting
for a ready view, viewing, requesting control, controlling, or stopping. A
request is not a grant: keys and pointer actions stay local until the host has
granted control and the original input producer is attached.

Start with `--control` to make explicit control requests available. If an
automatic reconnect or a permitted resize opens a replacement view, it retains
that control-capable profile but starts viewing. Open **Input → Request
control** to make a fresh choice for that current view. A connection started
without `--control` remains observation-only, and its Request action is
disabled. Neither receiving a picture nor reopening a connection restores the
earlier control request.

Request is enabled only while the shared session has current visible-view
evidence. Each menu choice belongs to its original view and is consumed once.
A stale native command, lost readiness, hidden window, changed mapping or
replacement connection invalidates it. The original native queue age and the
session's bounded callback wait count against the same request lifetime; a
delayed command is never automatically retried. Reopen the menu for a fresh
choice after the view is ready again.

**Stop control and keep viewing** is available after control has been granted.
It fences that original input source immediately, then closes the controlled
connection through its existing terminal exchange and bounded cleanup. Only
positive original service and cleanup results permit a fresh viewing connection
to the same selected machine. The new view requires the normal host admission
and approval again. It does not request control, restart the clipboard or send
any file selection again. **Request control** remains a separate fresh choice.

The Stop choice is bound to the original session, connection attempt, native
window lifetime and menu gesture. Failed cleanup, a hidden window or expiry
ends the connection instead of reopening. The existing ten-second replacement
budget, finite replacement count and invocation-wide `--seconds` still apply.
Close the window to disconnect completely, including while a control request
is pending. The final JSON counts completed handoffs in `control_releases`.

Switching to another application invalidates pending control choices and ends
any granted control. For an active controller, the viewer now keeps its window
open while the original connection closes and its held input, clipboard and
file owners finish cleanup. The old picture is cleared. If that original service
and cleanup both succeed within the existing ten-second bound, the disconnected
window waits for you to return; it opens no connection while waiting. Its title
shows **Control stopped; return to this window to keep viewing**.

A fresh foreground event opens a new viewing connection to the same selected
machine, subject to normal host admission and approval. That event has its own
bounded opening request; repeated activation callbacks do not renew it. The
new connection starts viewing. Choose **Input → Request control** again to
request a new grant. Clipboard synchronization and previous file selections
are retired permanently for this invocation's replacement connections. They
are never resumed or replayed by returning to the window.

Closing the dormant window or reaching `--seconds` ends the invocation. Stale
foreground callbacks are discarded and leave the window disconnected. Failed
or late original cleanup, another hide during an accepted return, a failed
picture clear, or an exhausted replacement count ends the connection instead
of reopening. Dormant time does not renew the original
cleanup budget; returning to a disconnected window creates a fresh viewing
choice only while the invocation remains live. The existing cap of 32 native
session owners includes focus replacements, display changes and resizes.

The JSON completion document counts fresh viewing owners started after
confirmed original focus cleanup in `focus_releases`. `last_focus_end` retains
the original `CONTROL_STOPPED` / `Hidden` ending and its positive cleanup
result; the replacement's admission and final ending are reported separately.
These fields contain no input, clipboard text or file paths. Starting a
replacement never changes the original hidden ending into an ordinary
successful close or proves that the new host admission will succeed.

This continuity path covers switching from an active controlling viewer to
another application and returning to its still-open window (`fr-o26d`).
Minimizing, native resize/mapping changes, and external focus loss from a file
picker retain their existing terminal behavior. A view-only session can remain
connected. Existing resize rules still require successful original cleanup
before reopening; a controlled resize that cannot establish orderly closure
ends the session instead.

## Choose a remote display

```text
fr-windows view remote.example.ts.net --display choose
fr-windows view remote.example.ts.net --control --display choose
fr-windows view remote.example.ts.net --display only
```

`--display choose` opens a native display picker after the host has approved
observation and supplied its current display catalog. Each button shows the
display's pixel size, desktop position, scale and rotation. Choose one to start
video; **Cancel**, Escape, or closing the picker ends that connection. The
picker remains responsive while the session continues network and deadline
processing. The existing 30-second startup budget includes time spent choosing;
it does not restart when a picker appears or a button is pressed.

The available policies are:

| Option | Selection |
|---|---|
| `--display choose` | A fresh native choice from the current approved catalog. |
| `--display primary` | The display at desktop origin `(0, 0)`, otherwise the first approved entry. This is the default. |
| `--display only` | The sole approved display; multiple displays refuse as ambiguous. |
| `--display HANDLE` | An exact nonzero decimal host catalog handle, if present in this connection's catalog. This is not a Windows monitor number or picker row number. |

An empty catalog, a missing explicit handle, or a catalog that changes while
the picker is open refuses selection; it never opens another display as a
fallback. An explicit `HANDLE` is re-evaluated against each fresh catalog,
matching the `fr` CLI: its number is not a permanent physical-monitor identity.
The host still decides which displays are shared and permitted by local approval.

Each picker belongs to one connection attempt and one complete catalog. A
choice is consumed once. Reconnection and a permitted resize reopen read a
fresh catalog; `choose` opens a new picker and an older window cannot answer or
cancel it. Existing resize rules still resume viewing only. The selected
display's own signed origin and geometry feed the existing pointer mapping,
including displays left of or above the desktop origin.

### Change displays while viewing

After a picture is visible, choose **Input → Change display…** to choose a
different remote monitor in the same window. The action is available while
viewing or controlling, with no pending control request, text editor, native
resize, queued key transition or held mouse button. It consumes one fresh
menu choice from that original session. A queued command from an earlier
popup, native mapping or connection is refused.

Changing displays immediately fences the original input and clipboard owners.
The viewer waits for that original session's actual successful service and
cleanup result, then the serial host listener's 400 ms cooldown. Only then
does it open a new authenticated viewing session to the original selected
machine and request a new approved display catalog. The host's current
admission and local-approval policy still applies. No old display handle,
coordinate mapping, queued input, text composition or clipboard owner is
carried into the new session.

The replacement always shows the native picker, including when the command
started with `--display primary`, `only` or an exact handle. Subsequent resize
replacements also ask from a fresh catalog; they do not silently return to the
command's original display. Selecting a row starts viewing only. If the
original command offered `--control`, **Input → Request control** supplies a
fresh request for the new ready view. Clipboard sharing stays retired for that
command and requires a new controlling command to enable again.

Closing the window, stopping, hiding the application or changing the native
mapping cancels a pending display switch. A host refusal, revoked grant,
incomplete cleanup or local failure also prevents replacement and retains the
original completion report. In particular, a controlled session whose
cancellation cannot prove orderly cleanup ends rather than opening another
display. Elapsed time alone is never cleanup evidence.

Each replacement has a 10-second absolute retirement/cooldown budget,
including time the client spends suspended. That same deadline is checked
again after native preparation immediately before starting the replacement.
The new connection retains its existing separate startup deadline. One
command opens at most 32 fresh native session owners, counting the initial
owner and resize replacements. `--seconds` remains a deadline for the entire
command and prevents a replacement once it expires. Completion JSON records
`display_changes` and any content-free `reopen_refusal` separately from the
original session's ending. Runtime and live Windows multi-monitor
qualification remain pending.

A Change display popup also belongs to one internal connection attempt. If the
shared session reconnects while the popup is open, that old choice is refused.
The replacement must have a picture drawn from its own attempt before a fresh
display choice becomes available.

## Send Unicode and IME text

During a controlled session, choose **Input → Send text…** from the viewer's
local menu. The native multiline editor keeps typing, paste and IME preedit on
this Windows machine. Finish composition and choose **Send once** to submit one
complete value through the existing negotiated direct-text input capability.
Physical keys are not forwarded while the editor is open; close it to resume
ordinary remote shortcuts and typing.

The host must separately grant direct Unicode text. A host that supports only
physical keys gets an explanatory local refusal; no guessed keyboard layout or
clipboard insertion is used. Opening while keys/buttons are held or earlier
input is pending refuses the draft. Release them and reopen the editor for a
new local choice.

The editor is tied to the exact input source and remote-view context that
existed when it opened. A genuine suspension, new input grant or changed view
invalidates the draft even if the same geometry later returns. A short
compositor readiness gap temporarily disables Send without moving the draft
to another context. Resize, minimization, closure and reconnection discard it.

**Switching applications, minimizing, or resizing the viewer while the editor
is open ends the original control session.** Its independent lifecycle fence
stops input before native IME cleanup, including during a nested Windows
callback. This also prevents queued text from being sent after that boundary.
Cancel clears only local composition; text already admitted to the host path
is not rolled back.

One Send is limited to **4,096 UTF-8 bytes**. Supplementary characters and
combining marks retain their complete Unicode representation. Empty values,
embedded NUL, unpaired UTF-16 surrogates and oversized values refuse as a
whole. Native editor overflow or a change during the Send read discards the
draft so a truncated prefix or inconsistent value cannot be transmitted.
Local text, preedit and the native undo buffer are cleared
after Send or cancellation; none is included in logs or completion JSON.

Send retains the original gesture's queue age, source timestamp, view and
exclusive deadline. A queued Send that predates the current editor is refused.
Waiting for native text reads, transport capacity or a renewed ticket cannot
give that completed value a new lifetime. Both accepted
and refused attempts consume the editor's original draft; another Send
requires reopening and a new explicit local choice.

The status **Queued once for the host** confirms bounded local admission only.
It does not promise atomic host insertion. Existing host scalar-level
partial/uncertain outcomes still apply, and neither the original value nor an
unconfirmed suffix is retried automatically.

## Share the text clipboard

`--control` enables the optional text clipboard by default. Add
`--no-clipboard` to keep it off for that command. Observing sessions do not
offer clipboard capability or open the local clipboard. The host must also
enable the same optional capability, admit control, and complete bilateral
clipboard readiness. A host that omits clipboard still permits its ordinary
negotiated keyboard, pointer and direct-text capabilities.

A Windows host enables its clipboard with `frd run --control`. An X11 host
needs `--input-agent` and a `fr-input-agent` built with the `linux-clipboard`
feature. The controller offers its current local text once when the lane
starts; the host sends only copies made after its lane starts, preserving the
controller's initial selection instead of swapping both pre-session values.

Once the lane starts, its independent **Text clipboard sharing is on.** window
provides **Turn off** on this Windows machine. The host has its own local Off
control. The window does not activate over the viewer; its independent
supervisor fences clipboard publication if the window closes or stops
responding. Turn off retires only this clipboard lane. Ordinary control can
continue. Closing, minimizing or switching away from the viewer fences the
original clipboard immediately along with the existing input lifecycle.

The existing native adapter copies validated Unicode text in both directions,
up to **1 MiB of UTF-8 per item**. It checks the local clipboard revision
around reads and final publication, preserves source labels to suppress echoes,
and discards superseded or expired work. Clipboard text remains in the bounded
clipboard worker. It is never routed through keyboard synthesis or the local
Send text editor; updating a clipboard does not paste into an application.

Each command retains its original configured clipboard owner. Turn off,
retirement, a viewing reconnect or a permitted resize cannot restart that owner
or replay an earlier clipboard value. Start a new controlling command to
enable sharing again. A replacement view still requires its own explicit
**Input → Request control** choice for keyboard and pointer input.

The completion JSON includes a `clipboard` object with `requested`,
`configured`, historical `native_started`, `received_submitted`,
`received_unknown`, `received_refused`, `cleanup`, and a typed content-free
`reason`. The three receipt counters describe host-to-local publication:
submitted to the Windows API, uncertain effect, or refused. Native startup
does not claim publication. Counters survive Off, later failures and permitted
viewing reopens; no text, clipboard item identifiers or revision values appear
in the report.

The session reaps clipboard, media and input owners under the original cleanup
deadline before any reconnect. Pending native cleanup ends the command with a
failure and prevents a replacement session. The original session thread keeps
the same stopped owner until its native cleanup actually completes; a later
completion cannot change the earlier failure into success.

## Send files from the connected desktop

Start a controlling viewer with `--control`, then open **File transfer → Pick
and send one file…** after the host grants control. Choose one regular file in the
native Windows dialog. The original session negotiates an optional file lane
even without `--send`; no file is opened or read merely by connecting. A host
that does not offer files leaves this action unavailable without preventing
ordinary viewing and control.

The native chooser runs on a separate thread so the viewer keeps presenting
and servicing the original connection. Physical input stays local during the
choice. Its result belongs to the original input source, view, connection
attempt and window lifetime. A real view suspension/recovery, hidden window,
close or replacement cancels the choice; a temporary compositor candidate gap
does not invent a new view. The actual Send admission rechecks the original
context after lock waits. There is no queued choice that becomes valid later.

Focus can move between the viewer and its open file dialog. Switching to
another application cancels the choice and ends the original control grant.
Returning does not restore that grant. A delayed dialog callback cannot hide
or end control on a replacement connection, and sending also requires the
original viewer to be back in the foreground after the dialog has closed.

The dialog has one original two-minute budget. Its accepted native gesture has
a further two-second bound covering native destruction, selected-handle
preparation, worker collection and submission. Expiry discards the selection;
it never reopens the path or retries with a fresh deadline. Native worker
cleanup is checked separately from file selection. Closing the viewer gives
that original cancelled worker a bounded collection interval; an unresolved
or failed cleanup cannot count as successful completion or permit replacement.

### Select the first file on the command line

```powershell
fr-windows view remote.example.ts.net --control --send 'C:\Users\me\Documents\draft.txt'
fr-windows view --control --send 'C:\Users\me\Documents\Résumé final.txt'
```

`--send FILE` selects one regular file before the remote session starts. It
requires `--control`, may appear once, and also works with the native machine
chooser. The host must separately enable its existing file receiver with
`frd run ... --files DIR`, alongside the host's normal control configuration.
Use a Linux or macOS receiver for this workflow; this does not add a Windows
receive directory, downloads, directory uploads or drag-and-drop.

The sender retains the selected read-only Windows handle and its actual file
identity. It never reopens the source pathname during transfer. Conflicting
writes and renames are denied while that original handle or its duplicates
remain open. Directories, leaf reparse points, device namespaces, alternate
streams and explicitly mapped network drives are refused. Ordinary relative
paths resolve against the original working directory. The existing portable
basename checks still apply; no remote-supplied path selects local content.

No file contents are read until the host grants control and the original
optional file attachment is ready. Hashing and reads use the existing bounded
disk worker, ATP object protocol, quotas and session authorization. The host
stages privately and publishes without overwriting an existing destination;
a conflicting name is a refusal. The transfer does not open the received file.
If the host does not negotiate files, ordinary viewing and control can still
continue, and the upload reports that refusal.

The native **File transfer** menu shows the current upload's state, queued
bytes, host receipt and source-cleanup state. **Pause** and **Resume** change
only that original upload's pacing; they do not extend its transfer deadline.
**Cancel this upload** requests file-only cancellation and leaves the desktop
connection running. Every command names the exact batch shown when its popup
opened, so an old command cannot affect a newer selected file. Cancellation
cannot retract a file the host already published, erase its receipt, or turn an
uncertain result into success. A transport or authority failure can still end
the containing session under its existing rules.

A command admits at most nine explicit single-file selections, including an
initial `--send`. After a batch finishes, **Pick and send one file…** becomes
available again only if the original lane remains ready. Earlier publication
receipts and failures remain in bounded history. A cancelled or failed batch
is never silently repeated.

A resize, Change display, Stop control or reconnect never sends a selection
again. The menu and final report retain the original transfer evidence across
a permitted viewing replacement; that replacement has no upload configured.
Closing, minimizing, or switching away from an active
controlled viewer also fences its original file work through the existing
control lifecycle.

The completion JSON includes `upload` for a configured original upload owner
and `null` for an observation-only command. It retains the current batch,
bounded `batches` history, chooser result and chooser-cleanup outcome.
Per-file fields include `selected_bytes`, `configured`, `phase`,
`cancel_requested`, `queued_bytes`, `total_bytes`, `receipt`, `receipt_detail`,
`published_bytes`, `cleanup`, `reason`, and `succeeded`.
These fields contain no path, filename or contents. Queued bytes are not proof
of host publication. Receipts distinguish durable host publication, publication
with unknown durability, refusal, interruption before publication, and unknown
publication. Every accepted explicit upload must have a durable host receipt
and successful original source cleanup for the combined upload result to
succeed. An interactive lane on which no file was selected adds no file-success
requirement to the command. Successful video alone cannot make a refused or
unfinished upload look successful.

File cleanup shares the original session cleanup deadline. If that deadline
expires, the command fails and no replacement session starts. The original
session thread retains the stopped file owner and nonblockingly collects it
when its actual worker returns. Later collection cannot repair the earlier
failure or supply a new transfer deadline.

## Explicit address constraints

`--address IP` remains repeatable, up to eight distinct addresses. It now
constrains the host's **complete current authenticated address set**. The order
does not matter. A subset, duplicate, additional address or different node
address refuses the command.

For a node listed with both of these addresses:

```text
fr-windows view remote.example.ts.net --address 100.64.0.2 --address fd7a:115c:a1e0::2
```

Supplying only the IPv4 address would refuse. Use `--ipv4` to choose that family
without supplying an explicit address set. A complete address constraint may
also be combined with a family flag.

The constraint is checked against the selection snapshot and both fresh
metadata reads used to authorize the connection. It never replaces
authenticated metadata or turns the command into a direct IP or DNS fallback.
Reconnection and live renewal retain the connector's original identity
checks: changed names, addresses, keys, membership or local daemon identity
stop the attempt instead of silently selecting a replacement.

TLS still verifies the selected canonical certificate name and chain against
the bundled ISRG trust roots. Discovery alone grants no observation, input,
clipboard, audio or file authority.

## Fetch a host's directory

```text
fr-windows directory remote.example.ts.net
fr-windows directory n-selected --ipv6 --port 8443
```

The directory command uses the same installed identity path as the viewer.
It performs one bounded fetch, prints the shared directory JSON document, and
exits successfully only when the host returned a listing. The host must enable
directory service; its refusal remains visible in the result.

## HEVC decoder requirements

The viewer requires the system HEVC decoder to return each admitted frame's
own NV12 picture synchronously. A missing picture, delayed picture belonging
to an earlier frame, incomplete output or unexpected native event fails the
current decoder through the existing typed error path. It cannot advance the
reference chain or report an older picture as a newly decoded frame. Native
samples carry the original frame ID for ownership checking, independently of
capture timestamps; capture times and freshness deadlines remain unchanged.

Output types must retain the admitted coded dimensions or visible crop.
Buffer requests are checked before allocation, including their required
alignment. The per-picture NV12 budget allows rows padded to 512-byte
boundaries and heights padded to 64-line boundaries; returned texture arrays
are limited to 16 surfaces with a valid slice. Larger requests, changed
geometry and oversized returned memory are refused. These checks also apply
when the decoder renegotiates its output type.

## Local capture probe

```text
fr-windows capture-probe --seconds 10 --fps 30
```

The existing capture probe remains independent of host discovery. It exercises
the local Desktop Duplication, hardware HEVC encode and decode path in the
interactive session and prints counts and timings.

## Validation scope

`cargo test --locked -p fr-mobile-ffi --lib` passes ten portable decoder-policy
regressions with the pinned toolchain, covering original frame ownership,
missing/incomplete output, geometry, allocation bounds, alignment and readable
luma rows. A Windows-only real HEVC regression additionally checks distinct
frame ownership when capture timestamps are identical and exceed signed
native timestamp range. That new native regression has not run on a Windows
desktop; portable checks do not qualify decoder or GPU behavior.

`cargo test -p fr-mobile-ffi --test build_roots --locked` exercises the actual
build-time PEM parser, byte bounds and wrong-certificate rejection on every
build host. With `FR_MOBILE_ISRG_ROOTS_DIR` set to the two public local roots,
add `-- --ignored` to check both published pins and reject swaps and changed
certificate bytes. This never uses a live host or changes runtime trust.

Portable machine-picker lifecycle regressions live in
`crates/fr-windows/src/host_picker.rs`. They cover exact original identity,
one-use selection, empty and invalid rows, cancellation after Connect, queued
event freshness and timestamp wrap, clock regression, original-deadline
expiry through setup and cleanup, and repeated selection/show attempts.
The actual package's 85 portable tests and strict all-target Clippy passed with
the pinned toolchain after integration with the explicit Windows uploader.
The real window, cancellation ordering and blocked or failed native
destruction still require Windows desktop qualification.

For focus continuity (`fr-o26d`), the package's 106 portable tests and strict
all-target Clippy passed with the pinned toolchain. The new regressions cover
positive original cleanup, failed/late cleanup, long disconnected dormancy,
exclusive foreground/opening deadlines, stale callback/attempt identity,
repeated activation without deadline renewal, close/expiry cancellation and
the shared replacement limit. The exact original-grant claim policy is also
tested for a stopped source and a prior explicit close. An additional actual
controlled-session regression fences through an independent lifecycle handle
while the original producer mutex is held, then checks queued input and the
original orderly-close ledger.

The repository's actual render and geometry modules also passed a narrow
Windows GNU target compile and strict Clippy check against their existing
Windows bindings. This checked the D3D11 clear API boundary; it did not run the
native viewer or GPU. Real application switching, covered-window presentation,
held-key cleanup, host reapproval and explicit control reacquisition still need
Windows desktop qualification. Portable policy and boundary compile checks do
not establish those native observations.

Portable command-line regressions live in
`crates/fr-windows/src/options.rs`. Installed target selection and named-route
compatibility regressions live in
`crates/fr-mobile-ffi/src/session/connection/tests.rs`. The exact address
selector's metadata, fresh-read and renewal regressions live in `fr-tailnet`.

Clipboard policy regressions cover the default and explicit opt-out, optional
clipboard/Text composition with legacy-host negotiation, initial invisibility
versus actual visibility loss, publication-result retention, and cleanup
states that must prevent a replacement. The production Win32 owner and its
native cleanup protocol are shared with the Windows host. Cross-target build
checks exercise the actual client wiring. Bidirectional copies, real
clipboard managers, the independent Off window and blocked native teardown
still require Windows desktop qualification; portable tests do not establish
those OS observations.

Earlier native evidence is retained separately: revision `1dc02ae3` was run
on 2026-11-14 as a Windows viewer on `oldsurface` controlling the X11 host on
`ts2` at `dd06a4ae` for 75 seconds. The controller's pre-session text reached
the host, the host's pre-session text stayed local, new copies crossed in
both directions, and Turn off blocked later copies while ordinary control
continued. One host-to-Windows copy arrived in about 0.5 seconds in that run.
The `fr-p2-shared-clipboard-wgd` bead records the run and limits: an X11 input
agent built without clipboard support never started the lane, and stopping
the lane emptied its Xvfb-owned selection without a clipboard manager. This
earlier run does not qualify the subsequent retained-cleanup changes, other
Windows clipboard managers, or an iOS or macOS host clipboard implementation.

The native editor's portable whole-value and one-use-context regressions live
in `crates/fr-windows/src/text.rs` and compile on every host, including Linux.
The shared session's no-grant test also exercises the new original-draft API.
Existing source-context and original-capture regressions cover view/source
replacement, suspension continuity and exclusive input deadlines. The Windows
cross-target Clippy lane compiles the actual editor, its Win32/IME bindings and
the shared session integration. These are separate from real Windows
IME/lifecycle and host-delivery qualification.

Those checks cover selection and route policy. They do not establish native
Windows pipe access, GPU behavior or live tailnet connectivity on a particular
machine; native platform qualification remains necessary.

Control-choice regressions exercise the actual portable ownership policy in
`crates/fr-mobile-ffi/src/session/control_request.rs` and the original native
menu timestamp policy in `crates/fr-windows/src/control.rs`. They cover one-use
submission, exact expiry, repeated readiness polls, hidden/replaced views,
cross-session tokens, native timestamp wrapping and stale popup commands.
Shared-session regressions also cover observation-only refusal, no input grant
from a request, lifecycle retirement and input-failure fencing. The real
Windows menu, focus transitions, reconnection and host grant still need native
qualification; portable policy tests do not establish those device results.

The actual shared display-selection policy is also exercised by portable
`fr-windows` tests: primary/explicit selection, empty and ambiguous catalogs,
missing handles, complete metadata changes, one-use replies, cross-session and
cross-attempt isolation, cancellation and the full eight-display bound. Shared
session regressions cover approval-callback publication, no input grant from
selection, stale cancellation and close before consumption. The native picker
still requires Windows UI and a real multi-display host for platform
qualification; these source tests do not claim that hardware run.

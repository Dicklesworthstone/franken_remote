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

## Request or stop control in the viewer

The native **Input** menu now contains **Request control**, **Stop control and
disconnect**, and **Send text…**. The window title shows whether it is waiting
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

**Stop control and disconnect** cancels a pending request or fences the
original input source immediately, before native composition cleanup. It then
closes that connection through the existing bounded cleanup path. Ending
control ends the session; Stop does not silently reopen another view or grant.
The menu remains local even while the remote desktop is controlled.

Switching to another application or minimizing the viewer invalidates pending
control choices and ends any granted control. Returning to the application
cannot restore it. A view-only session can remain connected. Existing resize
rules still require successful original cleanup before reopening; a controlled
resize that cannot establish orderly closure ends the session instead.

## Share the text clipboard

```text
fr-windows view remote.example.ts.net --control
fr-windows view remote.example.ts.net --control --no-clipboard
```

With `--control`, the viewer shares Unicode text both ways with a host that
shares its own clipboard: a Windows host running `frd run --control`, or a
Linux X11 host with `--input-agent` (its `fr-input-agent` built with the
`linux-clipboard` feature). Hosts without a clipboard drop the lane and control
still works. `--no-clipboard` turns it off. A view-only session never shares
the clipboard.

While sharing, a small window says "Text clipboard sharing is on." and has a
**Turn off** button. It is frd's own clipboard owner, the same one a Windows
host shows. Turn off, closing that window, or the window not answering within
250 ms stops sharing for the rest of the session. Control continues.

- Text on the Windows clipboard when control starts is sent once. The host's
  own earlier clipboard is not sent; only copies made there after the lane
  starts arrive.
- A copy made on the host arrives on the Windows clipboard within about a
  second. Switching to a local app to paste it ends control (above), but the
  text stays on the Windows clipboard.
- Only the first controlled view shares. A reconnect or resize reopen does not
  reopen the clipboard.

The completion JSON carries a content-free `clipboard` object: `requested`,
`configured`, `active`, received counts (`received`, `received_unknown`,
`refused`), `off` and `reason`.

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

This selects the display before video starts. To choose a different display
later, close the viewer and run the command again. Selecting a display does
not itself request control; `--control` retains the existing separate grant
and visible-view requirements. The existing mobile C interface retains its
primary-display default.

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

## Local capture probe

```text
fr-windows capture-probe --seconds 10 --fps 30
```

The existing capture probe remains independent of host discovery. It exercises
the local Desktop Duplication, hardware HEVC encode and decode path in the
interactive session and prints counts and timings.

## Validation scope

`cargo test -p fr-mobile-ffi --test build_roots --locked` exercises the actual
build-time PEM parser, byte bounds and wrong-certificate rejection on every
build host. With `FR_MOBILE_ISRG_ROOTS_DIR` set to the two public local roots,
add `-- --ignored` to check both published pins and reject swaps and changed
certificate bytes. This never uses a live host or changes runtime trust.

Portable command-line regressions live in
`crates/fr-windows/src/options.rs`. Installed target selection and named-route
compatibility regressions live in
`crates/fr-mobile-ffi/src/session/connection/tests.rs`. The exact address
selector's metadata, fresh-read and renewal regressions live in `fr-tailnet`.

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

Clipboard offer regressions live in
`crates/fr-mobile-ffi/src/session/control_policy.rs` (the added markers are
exactly the optional clipboard set, and intersecting with frd's real host
offers selects the lane only when the host shares) and
`crates/fr-mobile-ffi/src/session/clipboard.rs` (later attempts drop the
markers; a required marker is refused, never dropped). The Win32 owner itself
is frd's `windows_clipboard`, qualified live on the Windows host. The viewer
was run live against a Linux X11 host on 2026-11-14 (see the
`fr-p2-shared-clipboard-wgd` bead for the run and its limits).

The actual shared display-selection policy is also exercised by portable
`fr-windows` tests: primary/explicit selection, empty and ambiguous catalogs,
missing handles, complete metadata changes, one-use replies, cross-session and
cross-attempt isolation, cancellation and the full eight-display bound. Shared
session regressions cover approval-callback publication, no input grant from
selection, stale cancellation and close before consumption. The native picker
still requires Windows UI and a real multi-display host for platform
qualification; these source tests do not claim that hardware run.

# Native file transfer pause and resume

The sender supports explicit, same-connection pause/resume for an already
selected file or a bounded batch of files and portable directory trees. This is
pacing of the original transfer, not crash recovery, reconnection replay, folder
synchronization, or renewed controller permission.

## Application API

The `frd::native_files::SendControl` returned by file-send configuration has
`pause()` and `resume()` alongside `cancel()` and `snapshot()`. These methods
return `true` only when they change the local request. Clones refer to the same
selection. They never start another selection, open a source, or make a network
call. After cancellation or terminal status, pause/resume requests return `false`.

The snapshot separates the latest `pause_requested` intent from `paused`, the
original sender's last observed local admission gate:

| Observation | Meaning |
| --- | --- |
| `None` | No sender observation yet, including before attachment setup. |
| `Some(false)` | The sampled sender's pause gate is not set. |
| `Some(true)` | The sampled sender defers new records and queued batch sources. |

The observation is not proof that already-admitted bytes stopped, the peer is
idle, a kernel read was interrupted, or a remote effect was rolled back. Consult
`status`, `report`, and `cleanup` for their separate outcomes. Pressing pause or
resume does not rewrite an earlier observation as an acknowledgement. Queued-byte
progress is never a publication receipt.

The running-viewer adapter applies pacing immediately after queuing a selection,
before a later sender turn can start its first source. A request made before
control or during setup does not skip or renew the original one-use attachment
handshake. Cancellation still releases queued selections and settles that same
handshake before file-only retirement.

The adapter calls `ControlledViewer::set_files_paused`, which uses the original
`Sender::set_paused` with the original transport identity. `file_is_paused`
reports the corresponding `Sender::is_paused` value. A nonterminal refusal ends
the local selection, requests cancellation once, and continues polling cleanup;
it cannot become an automatic retry.

Applications must continue servicing the original controller and transport while
paused. The native X11 viewer now binds physical Ctrl+Alt+Shift+F8/F9/F10 to
pause/resume/cancel an explicitly configured upload, without leaving its focused
window. See [Native upload shortcuts](NATIVE_FILE_SHORTCUTS.md) for gesture
custody, unavailable mappings and qualification limits. A separate non-focus-taking
[upload progress panel](NATIVE_FILE_PROGRESS.md) now shows the original snapshot
and offers the same three local requests as buttons. There is no command-line
pause switch; closing the panel only hides it.

## Native Mac upload controls

The `macos-desktop` viewer's upload strip has a **Pause / Resume** button beside
**Cancel**, for initial `--send` or `--choose-files` selections and later
`--file-drop` or `--file-commands` batches. It fits inside the existing native
toolbar; enabling pacing changes neither the remote viewport nor `--fit` limits.
An empty negotiated upload lane has no selected batch and cannot be paced.

The separate label shows **Running**, **Run pending**, **Paused**, or **Pause
pending**. A request becomes an observed state only when the original sender
reports it. The button always retains the exact `SendControl` displayed with
that label. Selecting a newer batch cannot redirect an older click. Terminal
or cancelled owners refuse a request, including a race after the button was
drawn. The download's independent Cancel button keeps its own owner.

AppKit captures one original physical mouse-down timestamp and displayed batch
identity. Stale, synthetic, remotely injected, modified or disabled gestures
do not reach a control. A queued close, focus loss, emergency release, display
change or event overflow suppresses pending pacing before Rust can resume it.
The full bounded event batch is inspected even when download cancellation
follows a resume click. Upload cancellation discards pending pacing and retires
the original upload consent. Neither action creates a selection or a deadline.

## Deadlines and effects do not pause

Already-admitted records can still drain. A prepared but unadmitted packet is
held under its original deadline, not discarded and re-encoded with a fresh
lifetime. No new record is prepared and no additional streaming read is requested
while paused. An already running preparation or bounded read may finish. A
queued batch starts no new source until resumed.

All original record, transfer, and batch deadlines continue to run. Resume is
intent only: ordinary service checks the original context, permission, deadlines,
and rate budget before admitting more work. Host-side timeouts are unchanged. A
pause can therefore expire a transfer, including a held packet; resume never
retries it. Refusal, cancellation, and disconnection remain terminal.

Incoming receipts and cleanup continue during a pause. If completion was already
admitted, the current transfer may still publish. Cancelling preserves prior
publication receipts and publication uncertainty; it never fabricates rollback.

## Regression coverage and qualification

The native control and running-viewer modules contain application-order,
concurrent-intent, stale-handle, early-pause, cancellation, and refusal tests.
Existing `crates/fr-files/tests/sender_pause.rs` contains real UDP/TLS/ATP/disk
fixture cases for the sender. Those transport tests remain separate qualification.

The accompanying validation bundle records execution scope explicitly. Scripted
boundary/application tests are not qualification of the full Asupersync runtime,
network transfer, native desktop, or a live tailnet session. The full workspace
and real transport tests must pass on the pinned toolchain before release.

The Mac routing policy has six dependency-free tests using the production
`macos_file_pacing.rs`: requested/observed state separation, immutable displayed
handles, successor selection, exact gesture expiry, cancellation precedence,
and malformed/disabled actions. Local-command owner tests additionally exercise
retained real `SendControl` handles and cancelled/terminal refusal. The latter
require the full `frd` dependency build. Portable routing tests do not qualify
AppKit event delivery, the Apple SDK build, native window layout or a real
pause/resume transfer on a Mac.

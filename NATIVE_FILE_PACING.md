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
custody, unavailable mappings and qualification limits. There is still no new
progress dialog, graphical button or command-line pause switch.

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

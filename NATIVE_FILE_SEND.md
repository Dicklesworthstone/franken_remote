# Native file sending

`fr_files::sender::Sender` exclusively borrows the original completed
`FilesChannel`; its parent continues to own and drive the existing QUIC
connection. `begin` accepts a locally selected `std::fs::File` and a portable
basename, never a peer-supplied source path. Hashing, metadata checks, seeks and
reads happen on a dedicated disk thread using one outstanding command/result.
No file-sized buffer or new async runtime is created.

The source is hashed with the pinned upstream ATP streaming machinery and offered
once. The sender validates the host's exact size, requested graph root and
full-object profile before reading transfer chunks. It applies both its local
rate cap and the host's byte-rate/chunk limits, charges per-record overhead, and
retains one prepared record under backpressure with its original deadline.
Another source chunk is requested only after previous native file sends drain.
The same selected descriptor is used throughout; source metadata and a second
streaming hash are checked before emitting ObjectComplete. Files whose reported
size disagrees with readable bytes refuse, rather than silently truncate.

Call `service` before and after each ordinary connection I/O turn. The parent
supplies its real session authorization callback and independently services
control renewal and media. Results distinguish host-reported durable publication,
host-reported publication with unknown durability, host refusal, interruption
before any publication request, and unknown publication after completion was
admitted. Neither bytes queued nor native acknowledgement is publication proof.
Success additionally requires a consistent ATP ReceiveReceipt for the exact
transfer and size. No failure or uncertain effect automatically resubmits a file.
After collecting a successful result and source cleanup, another file uses the
same channel and a strictly increasing ID; the host's cumulative quotas remain.

`cancel` retires only the file pair and requests source shutdown. Continue polling
cleanup/result, including after cancellation. Call it before abandoning an active
sender; Drop requests local shutdown only, not remote cleanup. A stuck kernel
read is not claimed killed. The caller must retain/supervise this owner through
teardown rather than spawning replacements behind an unfinished worker.

The native integration harness runs both real sender and receiver over UDP/TLS
with actual selected descriptors and real filesystem publication. It exercises
multiple 120,007-byte files, an empty file, replaced source paths, changed content,
hostile acceptance, contradictory proofs, missing proof after real publication,
no-overwrite conflicts, and queued record expiry without destroying control.
These use explicit admission/controller fixtures, not live Tailscale or native
file-picker authority. Normal host/client optional-channel setup and user-facing
file selection still need integration. Resumption, folder sync and host-to-client
sending are not implemented by this single regular-file profile.

## Running controller integration

`ControlledViewer::attach_files` now consumes one completed optional channel on
its original connection and decoder-backed control grant. It checks the selected
file capabilities and display/generation tuple, uses the already agreed nonzero
file-scope handle, and requires a separate local `Permission`. It does not open
or read a source. `send_file` accepts an explicitly selected descriptor and a
portable basename; ordinary `drive` turns service the sender after input and
renewal. File records are reserved for this owner, never a media/UI callback.

The owner exposes stage, queued-byte progress, actual host receipts and errors.
`cancel_files` retires the completed file pair without stopping desktop input.
`reap_files` uses an independent cleanup context and absolute deadline, joins only
an already-finished original source, and preserves the outcome. A blocked kernel
read is not declared cleaned; a dropped/expired cleanup future retains the owner.
Receipts remain collectable after controller closure, including unknown effects
and already-published files. No retry or new controller is created.

The running-viewer tests use both ordinary controller loops and real TLS/UDP,
ATP hashing and filesystem publication. They cover successive 120,007-byte and
empty files, input and renewal beyond the initial lease, partial cancellation,
local file-permission denial, and abandoning an unpolled drive. Their initial
consent, source-visibility evidence and counted input sink are explicit fixtures,
not native desktop or live-Tailscale qualification. Native file selection,
viewer-side automatic attachment, file-scope agreement, resumption, downloads
and folder synchronization are not completed by this slice.

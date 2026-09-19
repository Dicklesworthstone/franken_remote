# Native file sending

`fr_files::sender::Sender` exclusively borrows or consumes the original completed
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
file-picker authority. Running host/viewer optional-channel setup is described below; user-facing
file selection still needs integration. Resumption, folder sync and host-to-client
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
not native desktop or live-Tailscale qualification. Native file selection, file-scope agreement, resumption, downloads and folder
synchronization are not completed by this slice.

## Automatic viewer attachment

After local file-scope agreement, call `ControlledViewer::expect_files` with the
nonzero handle, independent local permission, sender policy and a bounded setup
timeout. `ControlledHost::offer_files` supplies the matching host offer. Both
normal controller loops now perform the role-specific binding/ticket exchange
and promote the same completed channel. Neither a manual attachment pump nor a
second connection is required. The viewer checks the full selected display and
generation tuple; an offered alternate display cannot replace its local scope.

The original setup deadline starts at `expect_files`, including idle time before
the first poll, and bounds actual UDP service and presentation-callback waits.
No file source starts before completion. `send_file` returns `Busy` while setup
is pending, without reading the supplied descriptor. Pending clipboard and file
handshakes are serialized on the common control route; established lanes remain
independent. Duplicate expectations cannot spend another channel slot.

Cancellation, permission loss, malformed/foreign binding or expiry during the
uncompleted exchange fences the parent. There is no safely completed file lane
to retire independently yet. Established file transfer still cancels separately.
A failed sender construction after attachment also fences instead of discarding
an unacknowledged peer handshake. No setup failure fabricates a publication or
source-cleanup receipt.

Nine additional UDP/TLS controller tests cover automatic establishment and real
120,007-byte publication with input, local refusals before admission, capability
denial, original-deadline expiry (both idle and serviced), unfinished cancellation,
permission revocation, unpolled abandonment and another display's offer. Together
with the four running-sender cases these are 13 integration tests, not a complete
native picker, live-tailnet, download, resumption or folder-sync qualification.

## Closing native desktops with file work

The streaming viewer and `NativeObserver` retain access to the original file
receipt after control ends. Their `reap_files` fences the original session at
call time, before the returned future is polled, and waits only for the original
source under an independent, absolute closing budget. A failed or abandoned wait
keeps both the source and its publication/unknown-effect result available.

`Desktop::reap` now includes file-source cleanup and retains the file receipt in
its `Cleanup` report. The reconnect application refuses to release an attempt
with a file-cleanup error; it cannot start another desktop while silently leaving
a blocked file reader behind. A successful cleanup means no outstanding source
or that the original thread was joined, not that a file was published. The
separate receipt remains authoritative for that external effect.

A real supervised-presentation/ATP source test promotes an existing controller
into streaming, expires its first cleanup budget, then joins the same source and
collects its original interruption receipt without replay. A separate native
reconnect-policy test proves that file cleanup errors block another attempt and
retain an unknown-publication receipt. These do not qualify hardware codecs or
physical presentation.

## Drop-directory scope without prearranged handles

`ControlledHost::offer_file_drop(request, configuration)` and
`ControlledViewer::expect_file_drop(permission, policy, timeout)` use the optional
`file-channel-scope` v1 capability. Unlike the explicit-handle APIs, neither takes
a separately agreed file handle. The host maps its fresh Files attachment to the
already opened, locally approved `DropDirectory`; both endpoints derive its
handle from the authenticated completed channel binding. The viewer never sends
a host path, chooses another root, or uses a numeric label as authority.

The original full-view check, one-use ticket exchange, controlling lease, local
permission, setup deadlines and single-use lane remain unchanged. The viewer
reads the binding only after the exact attachment completes. No source opens
until the caller explicitly supplies its selected descriptor to `send_file`.
Multiple selected files then reuse this lane with increasing transfer IDs.
Established file cancellation remains independent of desktop control. Older
peers retain the explicit-handle APIs; new methods refuse absent/wrong-version
capability selection before allocating a lane or spending the one-shot slot.

Five real UDP/TLS/controller/ATP/filesystem tests exercise scope agreement and
120,007-byte plus empty-file publication with input, refusal without capability
selection or local permission, the immutable pre-poll deadline, and another
selected display's authenticated offer. Native input effects and presentation
are explicit fixtures, not hardware or live-tailnet qualification. This removes
the pre-agreed numeric-scope prerequisite for the drop APIs; graphical selection,
a complete command-line upload workflow, downloads, resume and folder sync still
require their separate implementations.

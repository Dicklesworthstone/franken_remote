# Controller text clipboard: binary transfer and publication

This is the implemented byte profile for `controller-text-clipboard` version 1,
using the existing application-version-0 FRD0 envelope and the kinds allocated
in [PROTOCOL.md](PROTOCOL.md). It implements plan section 15.3 and advances
`fr-p2-shared-clipboard-wgd`; it does not close that feature's application,
platform, approval, or interoperability gates.

## Scope and authorization

Only an explicitly attached, reliable clipboard channel is eligible. Its
nonzero binding, remote session, current controller lease, and authenticated
sender role come from the admission/attachment owner, not the record. No
read-only observer, datagram, input lane, media lane, or ordinary control lane
can acquire clipboard authority. A context describing another lane is refused.
The existing media `Channel` enum is not extended with a non-media channel.

Negotiate the capability before attaching the lane. The host must have the
separately approved native clipboard grant. Both endpoint switches start on
only after this grant and the current controller exist. `Context` and the
constructor's grant argument are trusted local integration inputs, not fields
to populate from unverified peer JSON. No listener or default-enabled network
route is installed by linking the codec.

`fr_core::clipboard::ClipboardSession` retains the original input monitor;
copying numeric IDs cannot rebind it to a replacement controller. The owner
must be retained uniquely for that lease and serviced while idle. It cannot
be recreated to reset a consumed sequence floor. The host library constructor
uses an actual native input owner; viewer lifecycle integration must use its
qualified host-clock/lease projection, not manufacture a host authority.

## Exact encoding

Every integer is unsigned, big-endian. Every record has the 24-byte FRD0 header:
magic `FRD0`, application version u16 = 0, kind u16, flags u16 = 0, reserved u16
= 0, payload byte count u32, attached-channel binding u32, extension bytes u32.
The usual bounded TLV extension rules apply; the encoder emits no extensions.
There are no strings, MIME names, paths, executable commands, or implicit paste
operations in the envelope.

The common payload prefix, in order, is remote-session ID u128, controller-lease
ID u128, transfer ID u128, source u8 (1 host, 2 controller), source sequence u64.
IDs and sequence must be nonzero. The source must match the authenticated
sender. The common prefix including the record header occupies 81 bytes.

| Kind | Suffix after the common prefix | Whole record without extensions |
|---|---|---|
| `0x0050 ClipboardBegin` | Total UTF-8 bytes u32; chunk count u32 | 89 bytes |
| `0x0051 ClipboardChunk` | Chunk index u32; byte offset u32; chunk byte count u32; exactly that many raw bytes | 93 + chunk bytes |
| `0x0052 ClipboardCommit` | Original declared total UTF-8 bytes u32 | 85 bytes |
| `0x0053 ClipboardCancel` | Reason u16 | 83 bytes |

Cancel reasons are 1 user, 2 disabled, 3 expired, 4 superseded, 5 failed; all
others refuse. Cancel references an existing transfer and is release-only,
including while disabled. It never restores or overwrites an OS clipboard.
Each direction has its own source sequence, strictly increasing within the
original controller lease. An opaque transfer ID is not an authority token.

## Allocation and ordering

The selected `ProtocolLimits` applies to every complete record and item.
The absolute ordinary-record limit remains 65,536 bytes and the item limit
remains 1,048,576 bytes. Clipboard never raises the control-message ceiling.
Each chunk has 1..16,384 bytes, each transfer has at most 1,024 chunks, and there
is one incoming item buffer per clipboard owner. Admission checks total and
count before reserving the complete item. Empty text has total/count zero and
uses Begin then Commit, with no zero-byte chunk. A nonempty item has at least
one byte per declared chunk and must fit its declared number of chunks.

Chunks start at index/offset zero and must follow exactly, without overlap,
duplicates, gaps, empty chunks, excess bytes, or excess chunk count. UTF-8 may
cross chunk boundaries; only the complete item is validated as UTF-8. Partial,
invalid, expired, or cancelled items never reach native publication. After an
accepted Begin, its source sequence remains consumed even after cancellation,
malformed chunks, allocation failure, native refusal, or receipt eviction.
No transfer-ID or chunk-index collection grows with peer input.

`Sender` owns at most one selected-limit item and emits one bounded record at
a time. It chooses a chunk size no greater than 16,384 and no greater than the
selected record budget minus 93. A smaller record budget that needs over 1,024
chunks refuses before allocation. `encode_next` does not consume a record;
`accepted` advances only after actual bounded transport admission. Failed
queueing leaves the exact bytes reproducible. Completion/drop clears the
sender's private buffer. Creating/sending a `Sender` does not authorize reading
the native clipboard or disclosing bytes: the runtime rechecks current authority
and both switches immediately before each enqueue and discards it on fencing.

## Final publication and cancellation

The transfer deadline is fixed at Begin admission, no later than three seconds
or the then-current original-owner authorization deadline. Renewal/traffic does
not extend it. The receiver tracks its own clock monotonically; regression
fences the owner rather than manufacturing a new lifetime. A local clipboard
revision change during transfer refuses the stale commit. Native preparation
may block, but occurs outside the shared authority mutex. The clock, original
owner, both switches, and fixed deadline are checked again after preparation,
immediately before the single native publication call. A switch's off/on
cycle advances a generation and cancels a blocked preparation even if it is on
again when preparation returns. Already-entered OS operations cannot be undone.

`SubmittedToOs`, `NotSubmitted`, and `UnknownEffect` remain distinct. A retained
commit receipt is returned without a second native call; uncertain effects are
never retried. This receipt is local API evidence, not a newly allocated wire
acknowledgment message. A successful clipboard set is not application paste.
The native adapter retains the exact source stamp for its selection so that its
own change notifications are suppressed, including an uncertain publication
that demonstrably became the current local selection. A real local change with
no matching provenance remains a real new item, even if its text happens to be
identical. Payloads and opaque IDs are excluded from Debug/errors.

The wire-to-publication bridge closes and clears a clipboard owner on malformed
ordered framing. Closing/disabling clears private transfer buffers where
practical; it does not erase OS/clipboard-manager history, cancel unrelated
input, or overwrite a newer local clipboard. Native preparation cleanup runs
on every exit, including unwinding, without turning cleanup into publication.

## Verification scope

`cargo test -p fr-core -p fr-wire --locked` exercises independent exact fixtures
for all four kinds, all fixture truncations, bounded single-byte mutations,
small selected record budgets, full one-MiB transfers, split UTF-8, lazy sender
backpressure, and the complete bytes-to-authority-to-publication contract.
Recording sinks are explicitly test fixtures, not OS adapters. This is not
native clipboard, live-tailnet, independent-peer, or GUI qualification. The
full feature remains open until its platform/lifecycle attachment, switches,
approval paths, supported-platform OS change detection, and desktop integration pass.

## Opt-in Linux/X11 native observation

`fr_native::clipboard::X11Clipboard` supports explicit `begin_read`, `poll_read`
and `cancel_read` operations alongside native publication. Reads accept bounded
`UTF8_STRING` and `INCR` selections, use a fresh requestor window per operation,
and check the current owner and selection `TIMESTAMP` before exposing complete
UTF-8 text. Owners without the required timestamp target refuse explicitly.
Each turn reads at most 16 KiB; the selected item ceiling and a fixed three-second
lifetime apply. Cancellation destroys only that read's requestor and private
buffer, never a newer local selection. Native X server calls remain a local OS
trust boundary and belong on the interactive worker, outside authority locks.

`X11Clipboard::read_to_channel` connects the read to an existing admitted
`ChannelSession`. Supply a qualified item ID and the original owner's clock,
and call it for an actual current native change, not repeated stale notifications.
The returned `ChannelRead` borrows both owners exclusively while pending. Its
`poll` rechecks the original authority and both switches before and after native
work; only a complete current selection can become `Offer::Queued`. Native text
never escapes this bridge to its caller. Drop, errors, caught clock panics,
revocation and an off/on switch cycle consume the read rather than retrying it.
A new local revision fences stale incoming commits and old outgoing text before
the potentially slow payload read, including when the new selection is missing
or unsupported. Exact publication provenance suppresses echoes; equal bytes
from a genuine new local copy are still eligible to send.

Run the real X11/core/wire composition tests with:

```sh
FR_NATIVE_CLIPBOARD_REQUIRED=1 xvfb-run -a \
  -s '-screen 0 1280x1024x24 -noreset -nolisten tcp' \
  cargo test -p fr-native --features linux-clipboard --test clipboard_x11 --locked
```

These tests exercise actual X11 selections and the real codec/authority owners;
the record handoff between endpoints is an explicit in-memory transport fixture.
This is not live-tailnet or cross-platform clipboard qualification. GUI/session
attachment, Wayland portal support, and a qualified viewer-side host-clock
authority projection remain integration work.


## Automatic native synchronization

`fr_native::clipboard::ClipboardSynchronizer::new(channel, native)` owns an
already admitted `ChannelSession` and its `X11Clipboard`. It watches server-authored
XFixes version-1 selection changes, starts bounded reads automatically, and
queues complete text without manual `begin_read` calls. The initial attachment
observes the current selection once. Idle turns do not request clipboard text;
even same-app copies of identical bytes are new native revisions. Exact own
publication stamps suppress echoes without hashing or retaining text history.

The interactive worker calls `poll(scratch, sink, host_clock, new_id)` during
traffic and silence. `host_clock` is the original admitted owner's monotonic
clock, and `new_id` supplies a nonzero qualified transfer ID. A poll services at
most 32 change events, one native read step, and one outbound record. A backlog
coalesces to the latest copy and suspends payload admission until caught up.
The original read deadline also bounds outgoing transmission; a slow read or
backpressured transport does not receive a fresh three-second lifetime.

Call `receive(record, host_clock)` for one fully framed record on that dedicated
lane. `Received::Deferred` means no consumption: retain at most one bounded
record and retry a later turn. `Consumed` and `Refused` are terminal for that
record; a refusal is not permission to replay. The synchronizer reports native
revisions before incoming publication, interleaves both directions without
holding authority locks across native work, and preserves committed or uncertain
publication receipts. Completing an earlier local read is not another local
copy and therefore cannot spuriously invalidate a later incoming Begin.

Both switch handles remain available on the synchronizer. Disable and off/on
cycles cancel private native reads and transfers. Re-enable waits for a genuine
new copy instead of replaying an interrupted selection. A failed/expired native
read is reported once, not retried while idle. Revocation, malformed input,
identifier failure, and unwinding close the native and wire owners together;
they do not overwrite another application's clipboard or revoke unrelated input.
The transport integration must separately fence records it already accepted.
No new network listener, synthetic viewer authority, native worker thread, or
async runtime is installed by constructing this owner.

The native integration target additionally starts two separate Xvfb desktops and
exercises automatic transfers in both directions for empty, Unicode and full
one-MiB text, idle/echo behavior, INCR replacement, both off switches, concurrent
incoming transfers, original-deadline backpressure, ownership loss, malformed
records, identifier faults and caught clock panics. X11 selection operations are
real; the bounded single-record handoff is an explicit transport fixture, not
live-tailnet qualification. Production channel attachment and interactive-worker
scheduling remain separate integration gates.

### Local copies during native publication

Incoming commits preserve a local copy that arrives during native preparation,
not just copies noticed before the commit starts. The synchronizer binds its
publication adapter to the admitted native revision; preparation services bounded
change metadata without consuming the notification needed for later propagation.
A changed revision refuses before publication. All potentially blocking native
validation still precedes the core's final authority, switch and deadline checks.

The X11 adapter additionally establishes a real server-clock barrier strictly
later than the prepared publication timestamp. Equal millisecond timestamps are
not treated as evidence that preparation is current. The barrier services at most
32 events and has the existing 100 ms preparation budget. A later selection
change then fences the attempted native set through the server's timestamp rules.
Such an attempted external operation retains its conservative `UnknownEffect`
receipt rather than being replayed. Deterministic real-X11 tests inject copies
both before preparation and immediately after it, preserve the newer selection,
and verify that its change notification still starts automatic propagation.

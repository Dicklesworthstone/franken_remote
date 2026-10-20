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
not extend it. On the host, "authorization" is the lease and observation
deadline (`InputMonitor::authorization_deadline`), not the input view window.
That window bounds input submission and is renewed by every presented picture,
so it is only 130-390 ms ahead. Until 2026-10-14 it capped every item, and on a
120 ms path a copy needing a second flight could not finish. Every chunk, the
commit and the final publication still require a live view
(`Authority::deadline`). The sender's read and send cap
(`ChannelSession::observe`/`offer`) uses the same authorization deadline. The receiver tracks its own clock monotonically; regression
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
`UTF8_STRING` and `INCR` selections and use a fresh requestor window per operation.
A standalone read checks the current owner and selection `TIMESTAMP` before and
after reading; owners without that target still refuse explicitly. An already
watched read instead binds the original server-authored `XFixes` revision and
requires a correlated server barrier after the final property read. This supports
owners without `TIMESTAMP`, including owners which answer an unknown target with
text, without a guessed timestamp or an unfenced fallback. Same-owner/same-time
reselection, subscription replacement, or stopping the watch invalidates that
read. A full bounded event turn cannot publish a possibly superseded snapshot.
Each turn reads at most 16 KiB; the selected item ceiling and a fixed three-second
lifetime apply. Cancellation destroys only that read's requestor and private
buffer, never a newer local selection. Native X server calls remain a local OS
trust boundary and belong on the interactive worker, outside authority locks.

Native text is preferred as `UTF8_STRING`. Only an explicit, correlated refusal
of that target may select ICCCM `STRING` (Latin-1 graphics, TAB and LF); a
malformed UTF-8 answer never triggers reinterpretation. The conversion keeps
the same requestor, original deadline, owner and watched revision. Both native
source bytes and the expanded UTF-8 result must fit the selected item ceiling;
INCR lower-bound validation counts source bytes, not expansion. No locale-based
conversion, replacement characters, additional item buffer or wire encoding is
introduced. Unsupported native control characters refuse the read explicitly.

Native publication advertises `STRING` in `TARGETS` only when the entire original
UTF-8 selection is exactly representable. Unicode-only selections remain
available as UTF-8, never as a substituted STRING copy. Each reader retains its
original immutable selection, encoding and fixed three-second deadline. A
STRING read uses one cleared 16 KiB scratch property at a time instead of a second
selection-sized allocation. INCR sizes count the requested representation's
bytes. Replacing the current selection cannot change a pending reader's encoding
or contents, and expiry never emits a false successful terminator.

The `clipboard_latin1` native test target uses independent Python/Xlib owners and
requestors. It covers refusal-only target selection, exact graphic characters,
expanded-size limits, raw INCR size checks, unchanged deadlines, reader capacity,
immutable mixed-encoding readers and automatic copies across two real X servers.
The automatic test uses a bounded in-memory record handoff and fixture authority
clocks; it does not qualify a live `fr`/`frd` tailnet session. Run alongside the
existing `clipboard_x11` target with `FR_NATIVE_CLIPBOARD_REQUIRED=1`, Xvfb and
Python-Xlib available:

```sh
FR_NATIVE_CLIPBOARD_REQUIRED=1 xvfb-run -a \
  -s '-screen 0 1280x1024x24 -noreset -nolisten tcp' \
  cargo test -p fr-native --features linux-clipboard \
  --test clipboard_x11 --test clipboard_latin1 --locked
```

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
attachment and Wayland portal support remain integration work. The controller
authority projection and its native integration are described below.


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
## Native QUIC attachment

`native-clipboard-attachment` version 1 adds native auxiliary role 5 (Clipboard)
to the existing one-use binding/ticket exchange. It requires `RequestControl`,
`native-input-attachment` version 1, `controller-text-clipboard` version 1, and
an already completed input attachment on this exact connection. Only one
clipboard reservation is allowed per connection, including after abandonment;
a new stream must not reset a controller's consumed clipboard sequence floor.

Both directions are allocated reliable unidirectional streams, with the fixed
host-to-viewer primary direction marker 1. Each admits only kinds 0x0050..0x0053,
uses the bulk budget, and has no datagram route. The existing media-channel enum
and input ordering domain are unchanged. The byte reservation is bounded by the
selected record cap, native stream window, and retained-send budgets. Input and
control retain their protected record storage and connection credit.

`fr_transport::quic::clipboard::ClipboardChannel` consumes the completed proof,
retains the original connection identity, derives authenticated sender direction,
and checks the actual session/lease and codec before dispatch/enqueue. Its
`limits()` lowers only the selected record cap to the attached byte allowance;
items remain separately chunked under the selected clipboard-item ceiling.
`dispatch` services at most one record and does no OS work: a native worker must
consume it through the existing authorized synchronizer. A blocked record stays
in transport ownership. `send` takes the original absolute transport-clock
operation deadline. The session's I/O driver must keep checking the original
controller's authorization while accepted bytes remain retained.

The application bridge retains at most eight original record permits and one
bounded worker handoff. This count does not enlarge the native 128-KiB clipboard
byte allowance: at most seven full 16-KiB chunks, including their headers, fit
at once. A permit remains held until the actual native whole-lane drain witness,
not merely until enqueue or a partial transmission. Every retry retains the
original operation deadline; expiry retires this optional lane while control
keeps its independent lifecycle. A newer local copy can discard an exclusively
local unsent record, but records already accepted by transport keep their
original custody until drain or retirement.

This route proof grants neither input nor native clipboard access. A host joins
its actual original input owner and native clipboard grant; a viewer must use
qualified client-side authority, never manufacture a host owner from wire IDs.
Dropping/closing the typed route retires its attachment and fences the original
connection at the next checked operation. It never acts on a foreign connection
whose numeric routes happen to match. Per-lane teardown, native worker handoff,
and application session construction remain separate integration work.

## Controller-side authority and native synchronization

A viewer no longer needs to fabricate a host `InputSession` to participate in
clipboard transfer. The core and wire clipboard owners now accept a read-only
`fr_core::clipboard::authority::Monitor`. The existing host constructors still
delegate to the original host input monitor. A controller obtains its monitor
only after `RequestControl::accept` validates the matching host grant and initial
ticket against this session's measured clock correlation. A bare
`InputClient::new`, copied IDs, or queued challenge response is insufficient.

After the separate clipboard capability, local OS grant, approval, and dedicated
channel attachment have been validated, call `attach_clipboard_lane` on
`InputClient` or `PresentedInput` with the completed route's `parent()`,
`outgoing()`, and `limits()`, plus the separate local clipboard approval. This
matches the original host boot, OS/remote session, parent binding, controller
lease, and sender direction before consuming the one-use local attachment.
Record and item limits can only decrease the original session ceilings; a small
attached record allowance produces smaller chunks, never larger control messages.
The caller still retains and checks the non-cloneable transport route: matching
numeric metadata alone does not establish original connection identity.
`attach_clipboard` is the convenience form for an already validated lane with
unchanged session limits. Mapping and qualified presentation must have been
established on that original owner. Attachment may finish during a temporary
view pause; this retains ownership but permits no clipboard payload until
readiness returns. The lane must differ from the control, display, and input
bindings.
Only one clipboard attachment is allowed per original input owner; closing the
lane does not permit recreating its replay ledger under the same grant.

`ControllerClipboard` accepts client-clock samples for offers, bounded sends,
and incoming records. It projects host time using the full measured uncertainty,
checks exclusive expiry before and after native preparation, and never compares
unrelated host/client clock origins directly. A better correlation cannot make
its accounting clock stand still or extend an existing item's original deadline.
Sending a control response does not prove a host renewal: only a validated newer
host ticket can refresh proof of the original lease. Peers may explicitly select
the optional `input-ticket-authority` version-1 profile. Its 136-byte ticket
includes the host's actual exclusive lease/observation deadline, sampled with
ticket issuance under the original owner's authority lock. The viewer uses
that exact deadline for new auxiliary operations, including when it shortens
the previous proof. This separates a copy's available authority from the short
input-ticket lifetime without changing the input ticket's expiry, proving a
renewal from a queued challenge, or manufacturing a new grant. Existing items,
native reads and retained transport permits keep the deadlines assigned when
they began. The same proof refreshes the separately attached file owner without
sharing attachment or transfer state. Peers that omit this optional capability
retain the exact 128-byte legacy format and its conservative ticket-only proof;
neither decoder guesses or falls back from the received record length.

A lapsed proof or temporary view loss returns `NotReady` without destroying the
attachment. This bounded pause grants no observation or publication authority.
Fresh, authenticated proof
of the same lease and current visible presentation are both required to resume,
in either arrival order. The pause cannot exceed the input owner's ten-second
view-suspension bound, and it never extends an admitted item's deadline. An
expired host lease, explicit revocation or stopped owner remains terminal; a
new grant cannot revive the old clipboard. Ticket-only expiry does not itself
shorten a lease still held and renewed by the host.

`ControllerClipboard::into_native(X11Clipboard)` produces a
`ControllerSynchronizer<X11Clipboard>`, using the existing bounded native
synchronizer, XFixes watcher, UTF-8/INCR reader, publisher, and protocol owners.
Every `poll` and `receive` callback supplies **client** monotonic time. Poll during
silence as well as traffic. The worker does not retain the input owner: drop,
focus loss, explicit lifecycle stop, disconnect, missed input/control obligations,
and clock failure fence it. Closing the clipboard alone does not revoke input.
A temporary view or proof lapse pauses this lane while input keeps its independent
ticket, view and lease checks. Maintenance advances the conservative accounting
clock and expires private buffers during the pause; it never supplies payload
authority. Independent authenticated clock exchanges update that model while
preserving lease proof, the required resume ticket and the fixed suspension
limit. An expired model or terminal owner cannot be revived by a late exchange.
Native watchers retain and service the current OS selection and coalesce copies
made during the pause. A newer consumed revision fences older queued payloads
before an authority recheck can observe recovery. Interrupted partial reads are
consumed, and queued wire records retain their original bounds.
The `PresentedInput` join also retains a read-only lifetime of its exact media
receiver. An explicitly negotiated reference recovery may replace the receiver's
epoch under the same live receiver lineage, unchanged configuration, and a
strictly newer recovery generation. This only permits waiting for fresh view and
ticket evidence. Closing or dropping the original receiver fences detached
clipboard work immediately, even before the next presented-input tick. Equal
numeric bindings on another receiver do not replace or revoke that lifetime.

The controller tests use actual grant, wire, authority, and media-receiver code
with explicitly simulated grant/clock/decoder/visibility and record boundaries.
The authority-profile regressions additionally pass one-MiB text in both
directions over 66 records and 1.95 seconds after the initial grant has aged two
seconds. The host renews the same original lease and observation
owner; each item still has one immutable deadline. These deterministic clock
and transport-boundary cases establish the policy path, not WAN throughput.
They also cover a shorter genuine cap, expired or replayed tickets, obsolete
recovery generations, wrong profiles and terminal-owner replacement. Real
UDP/TLS bridge regressions separately exercise eight retained permits, the
unchanged byte ceiling, final-chunk/Commit ordering, supersession and original
expiry; they require the daemon/native test gate.
The native integration target additionally exercises the controller path across
two real Xvfb desktops for empty, Unicode, and one-MiB text, echo/idle behavior,
original-owner cancellation during INCR, both off switches, and a smaller
2-KiB record allowance without reducing the one-MiB item ceiling. These tests do
not establish live-tailnet, independent interoperability, or cross-platform
qualification. The running session still must negotiate and authenticate the
dedicated clipboard lane, schedule its worker, connect its independent local
lifecycle fence, and fence bytes already accepted by its transport. No listener,
GUI hookup, host grant, alternate async runtime, or synthetic codec is introduced.

```sh
cargo test -p fr-core -p fr-wire -p fr-media -p fr-client --all-features --locked
FR_NATIVE_CLIPBOARD_REQUIRED=1 xvfb-run -a \
  -s '-screen 0 1280x1024x24 -noreset -nolisten tcp' \
  cargo test -p fr-native --features linux-clipboard --test clipboard_x11 --locked
```

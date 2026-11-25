# Implementation status

## Incremental content verification for active folder jobs (2026-11-25)

Live source scans now retain unchanged entries' exact ATP digests when another
file changes. One fresh bounded directory enumeration reconstructs the current
manifest, while changed/new/empty/coarse-timestamp entries follow the canonical
content and EOF checks. Reused file sizes still count against the same source
budgets. Two descriptor-relative opens retain identity checks around authority
callbacks; repeated names from a changing directory refuse the scan.

Any retained digest advances a shared verification age. At most three partial
scans can reuse content before a complete reread, even under continuous edits
to another file. Root or limit changes force a full scan; failure and cancellation
remove reuse eligibility. The sender's independent expected-version check and
confirmed-publication ledger remain the authority for delivery.

Four new filesystem regressions cover mixed-file reuse and forced rereads,
canonical Merkle equivalence across changed names, aggregate byte limits and
cancellation. A separate source test rejects repeated enumeration names.
Existing cache regressions retain their canonical filesystem comparisons,
replacement/refusal and original-authority checks. Two independent source/API
reviews completed against the pinned ATP definitions. No Rust test, formatter,
compiler or Clippy execution is claimed: no executor is attached, and hosted
verification remains queued. No throughput or native/tailnet benchmark is
inferred from reduced source reads. Tracking: `fr-sync-incremental-scan-8r4c`.

## Local folder publication durability (2026-11-25)

The local reconciliation API now stops when atomic file publication has
committed but its directory durability cannot be confirmed. It returns
`SyncError::DurabilityUnknown`, a failed/unknown-effect wire state, the exact
committed receipt, the durable prefix and the advanced baseline. A directional
no-replay receipt preserves that source version even after a local edit or
deletion of its published destination; an explicitly authorized later round
can still deliver the untouched successor or a new source version.

This corrects the local engine's previous false-settlement behavior and aligns
it with the native receiver's existing durability refusal. New real-filesystem
regressions inject only the final directory-sync failure, after normal hashing,
staging and rename. They cover both directions, conflict and ordinary names,
authority refusal, retained counters and no replay. Independent source review
is complete. Rust formatting, compilation, Clippy and test execution remain
pending: no executor is attached and hosted verification has not started.
This is source evidence, not storage-device or native/tailnet qualification.
Tracking: `fr-sync-durability-2q8m`.

## Bounded pages for larger native folder jobs (2026-11-24)

Explicit one-way folder jobs now inspect up to 256 flat directory entries and
deliver changed versions in rounds of at most 32 files. A settled page advances
a fair name cursor; repeated edits to earlier files cannot continually displace
later names. Deferred pages start promptly after the preceding settlement, with
a fresh bounded scan under the original authority. Once the backlog clears,
the configured scan interval resumes. Only the selected page retains source
handles, and only verified publications plus original cleanup advance its ledger.

The native receiver allows 512 attempted versions only for an explicitly
configured folder job, within the unchanged 1 GiB cumulative byte ceiling.
Ordinary file sessions retain their 64-attempt limit. Original folder receipts
are retained and reported within a 513-result ceiling, including the first
terminal refusal. Lower receiver limits still refuse explicitly. Keep-both
publication, independent destination edits, original deadlines, cancellation
and no replay after reconnect retain their existing semantics. This does not
implement recursive or bidirectional remote synchronization.

Added regressions cover all 256 entries in eight policy pages, hot early names,
abandoned cursor state, real UDP/TLS delivery beyond 64 files in both directions,
legacy and duplex upload lanes, cancellation between pages, oversized source
refusal, native receive quotas and bounded receipt reporting. These tests are
authored, not yet reported as passed. The exact pinned compiler and locked
dependencies were installed, but the executor disconnected during the baseline
workspace check before returning a result. GitHub verification is queued;
formatting, compilation, Clippy, tests and native/live-tailnet qualification
remain unverified for this change. See [folder synchronization](docs/folder-sync.md)
and bead `fr-sync-paged-rounds-g9mf`.

## Receive authority and native clipboard recovery (2026-11-24)

Inbound QUIC dispatch now rechecks the original connection lifetime, operation
guard, caller authorization, cancellation and clock after readiness callbacks
and between datagram callbacks. Revocation inside a callback cannot authorize
another application delivery. Optional lane retirement revalidates the exact
stream before dispatch; datagram expiry cannot transfer readiness to a promoted
successor or refresh its original receipt. Added actual UDP/TLS regressions cover
all four stop sources, blocked siblings, expiry, original receipts and clipboard
retirement while bidirectional control/input remain live.
See `QUIC_RECORDS.md` and `fr-inbound-dispatch-authority-47l6`.

The native clipboard bridge now retires only the exact wholly unsent Begin or
Chunk whose original item or local handoff deadline expires. Its original wire
owner clears retained text and orders Cancel(Expired) before another copy,
including when the independent controller/worker clock cursors differ during
view suspension. Original deadlines and privacy generations remain unchanged.
Commit/Cancel expiry and uncertain admission remain terminal; old permits cannot
retire replacement or already committed items. Added wire, bridge and worker
regressions cover these boundaries. See `NATIVE_CLIPBOARD_TRANSPORT.md` and
`fr-clipboard-unsent-expiry-629p`.

Validation: the portable `fr-wire` suite passed all 352 tests, and strict
`cargo clippy --locked --offline -p fr-wire --all-targets -- -D warnings`
passed after the pinned-nightly compatibility and function-size cleanup.
Pinned formatting on touched Rust files and `git diff --check` passed.
The native transport/daemon and full-workspace gates remain unverified:
earlier builds hit shared disk exhaustion and executor disconnection; resumed
native dependency compilation and GitHub checks were still pending when this
source checkpoint was committed. The two beads remain in progress. This is
source and portable-test evidence, not native-device or tailnet qualification.

## Explicit Windows host playback audio (2026-11-24)

The optional `windows-host-audio` worker now connects real WASAPI render-loopback
capture to the existing supervised Opus downlink. The host requires both
`--audio` and `--audio-endpoint-wide`: the latter explicitly authorizes the
default render endpoint's playback mix, including possible sound from other
logon sessions. Observation admission and the viewer's positive audio request
still gate capture independently of desktop control. The default installer
keeps audio disabled; this does not add Windows client playback or a host
virtual-microphone endpoint.

One thread-confined COM owner pins the original endpoint. Shared-mode engine
conversion supplies 48 kHz signed-16 mono/stereo PCM, with native packet sizes
checked before reads. Stream frame positions retain sample chronology while
QPC timestamps retain the original source-age deadline through packing, Opus
encoding and final publication. Device replacement, invalid timestamps and
native failure stop that audio source. Packet leases, bounded queues, parent
supervision and confirmed cleanup preserve the original worker's lifetime.

The twelve actual-source packing/timeline tests and five private audio-record
tests pass with the pinned nightly. Both the native `windows-audio-capture`
library and the complete `windows-host-audio` media worker pass `cargo check`
for `x86_64-pc-windows-gnu`; the latter uses a signature-verified MSYS2 static
Opus archive. These are source tests and target typechecks, not executable-link,
device, audible-playback or live-tailnet qualification. Actual endpoint
conversion, other-session scope, device changes, suspend and native cleanup
remain untested on Windows hardware.

Plan §§15.4 and 19; tracking `fr-p2-audio-playback-lel`;
[source build and scope](docs/windows-host-install.md#optional-host-playback-audio).

## Windows Explorer file-drop uploads (2026-11-23)

A ready, foreground controlling viewer now accepts up to eight regular files
dropped from Explorer onto its client area. The selection uses the same single
preparation worker and original upload owner as picked and copied files,
including pinned source handles, quotas, progress, host publication receipts,
pause/resume/cancel and bounded history. A busy worker or unavailable file lane
refuses the gesture rather than retaining another selection.

Each drop target is armed for one original window, Shared owner, connection
attempt, native lifetime and input/view context. A queued event must postdate
that original arming; ordinary ready polls do not move its timestamp. The
accepted native gesture keeps its original age through copying, preparation
and final submission. A reconnect on the same HWND cannot adopt an older drop,
and this workflow receives no dialog focus exemption.

The receiving callback takes drop-owned HDROP custody before any viewer or
eligibility branch. It bounds native storage to 128 KiB, copies while locked,
unlocks and invokes DragFinish exactly once before source preparation. Missing
viewers, malformed events, busy workers and other refusals retire the same
storage. Clipboard file choices share the bounded copy helper while preserving
their separate borrowed-handle contract. Existing Unicode, path, file-count
and aggregate-length checks apply to the complete selection.

The full portable `fr-windows` suite passes 266 tests, including seven new
custody, arming and native-time regressions; strict all-target Clippy passes.
The actual drop, clipboard, memory and portable choice modules pass all-target
check and strict Clippy for `x86_64-pc-windows-gnu`, using the pinned compiler
and real Windows bindings with dependency versions/checksums matching the
repository lockfile. Independent source reviews and workspace formatting also
pass. These component checks do not qualify the complete Windows viewer,
Explorer activation during a drag, or a live host publication. Those remain
explicit qualification work. UBS is unavailable in this environment.

Plan §§15.6, 16.1 and 19; tracking `fr-1vkr`;
[Windows usage](docs/windows-client.md#drop-files-from-explorer).

## Shared encoder bitrate through reference recovery (2026-11-23)

An established viewer's original connection now remains in the shared encoder's
path measurements while its decoder is fenced, replacement attachments are
pending, or the replacement waits for its first picture. A persistent queue
across the participating paths can therefore lower the next recovery IDR's
bitrate. Both the default path controller and adaptive capture use this rule.
Healthy or unmeasured siblings still veto a cut justified only by another
viewer's queue; recovering paths cannot justify optimistic upward probes.

Decoder replacement clears local sender/receiver evidence and probes without
discarding a conservative cut justified by the same original connections.
Actual membership changes still clear the complete evidence scope. A static
native observation may defer a rate change until recovery; only the original
worker's actual reply confirms it. Pending native effects remain accounted for
through membership changes. Weak owner identities retain no authority, and
capture, consent, replacement and media deadlines retain their original bounds.
Pacing carries the shared owner's latest clock through the whole policy update,
including when a subscriber advances it after the publisher's turn begins.

The complete portable `fr-media` Cargo suite passes all 668 tests, including
five new receiver-reset regressions, and strict all-target Clippy passes.
On baseline `40788b6` with this change, the wire, transport, tailnet, media,
client, files and web dependency libraries also compile with the pinned
compiler and locked third-party inputs.
A focused exact-source run on that recorded baseline passes all 32 tests:
four new shared-session regressions for recovery rate cuts, static deferral,
healthy-sibling veto and a lagged publisher clock, plus 28 existing path,
bitrate and receiver cases. The healthy-sibling fixture services real capture
credit at the admitted cadence and requires a successful source observation;
its assertions and original freshness deadlines are unchanged. Unrelated test
modules were excluded while production code and required helpers stayed intact.
The same finalized production snapshot passes strict daemon Clippy with the
workspace pedantic rules and warnings denied. Pending-capture sampling uses a
small helper that preserves the original expiry, lock, sampling and native-poll
order while keeping the live capture source borrow separate.
The regressions use real sessions, replacement attachments and worker IPC;
acknowledged-path snapshots, encoded bytes and decoder replies are explicit
fixtures. This focused result does not qualify later concurrent folder-job
changes or the complete current workspace. Earlier full Cargo/production build
attempts were killed under shared resource pressure. No new WAN or native encoder
qualification is claimed.

Plan §§12.3 and 13.3; tracking `fr-ktuc`;
[shared recovery](SHARED_RECOVERY.md#bitrate-during-reference-recovery-fr-ktuc).

## Controller-to-host native folder jobs (2026-11-24)

Native one-way jobs now also have a controller-to-host path. A Linux or macOS
host selects `--sync-destination DIR --sync-job ID --files-conflict keep-both`;
the controlling client selects `--sync-source DIR --sync-job ID`. The host
advertises its locally pinned destination and the client accepts the matching
job before scanning. The independent `file-folder-sync-upload` v1 capability
does not broaden ordinary upload or existing download consent.

The original bounded source/cache, ATP sender, tracked receiver and publication
ledger serve this direction. Source and destination consent are consumed once,
remain bound to the original connection and input authority, and cannot replay
after reconnect. Cancellation during attachment is checked before the authority
handoff or source-job creation. The native job excludes simultaneous downloads
and ordinary competing selections. Its `upload_folder_job` report retains
cumulative rounds, confirmed publications/conflicts, original failure and
positive cleanup evidence. See [commands and limits](docs/folder-sync.md).

Pinned-toolchain evidence: workspace formatting passes; all 350 `fr-wire`
tests pass, including the five new upload-capability regressions; all 29 client
`native_profile` tests pass, including the two new upload/duplex combinations.
Strict wire all-targets Clippy and client native-profile Clippy pass. A small
behavior-preserving input-admission helper extraction fixes a lint in concurrent
relative-input work; its 18 `input_roundtrip` tests also pass.

The new real UDP/ATP, filesystem, daemon and native-selector regressions are
source reviewed but unexecuted. The broader all-targets check stopped with
compiler SIGKILL in the unchanged pinned Asupersync dependency before reaching
the transfer/runtime crates. These results do not qualify native execution or
live hardware/tailnet use. Core source: `2b7957e`; parent tracking
`fr-p4-folder-sync-p6i` remains open for bidirectional reconciliation, recursive
trees, reconnect resumption and the remaining qualification.

## Explicit native one-way folder jobs (2026-11-23)

The previously local-only folder engine now has original-connection host and
controller owners, negotiated job-state routing and native daemon/client entry
points. A Linux or macOS host explicitly selects `--sync-source DIR --sync-job
ID` on an original controlled `--once` share. The controller explicitly accepts
the same ID using `--receive DIR --receive-conflict keep-both --sync-job ID`.
Ordinary receive consent cannot enable a standing job. Both roots are pinned
locally; no peer can supply a host path or create a job.

After acceptance, one supervised scanner delivers changed source versions
through the actual ATP sender, receiver and disk worker. Job states share the
ordinary bounded file queue and wait for ATP/proof/cleanup drain. Only confirmed
publication advances the bounded version ledger. A source changed between scan
and preparation refuses before its file offer. Local destination edits and
deletions do not replay acknowledged source versions; occupied names preserve
both versions. Known publication, uncertain durability and cleanup remain
separate results when cancellation races completion.

Each source snapshot permits 32 flat regular files, 64 MiB per file and 256 MiB
total. A distinct cumulative budget allows 64 attempted versions and 1 GiB, so
a full first snapshot can still deliver later changes. The original receiver's
lower limits and control authority remain binding. The job and its destination
consent never migrate to a reconnect. Bidirectional remote reconciliation,
recursive jobs, Windows host scanning and persistent jobs remain outside this
slice; see [usage and limits](docs/folder-sync.md).

Validation uses the exact pinned nightly and genuine pinned runtime dependencies.
The final five wire tests and strict all-target `fr-wire` Clippy passed through
Cargo. The complete six-test `remote_folder_sync` target passes against the
actual independent-root UDP/TLS, ATP and disk owners: multiple rounds, the
full-snapshot follow-up, conflict preservation, cancellation, source mutation
and job/connection refusals. All 24 tests in the complete client `native_profile`
target pass. Its full clipboard/audio/duplex combination caught a real capability
capacity failure; sync now yields only optional decoder-load reporting after
existing optional quality/bulk/credit fallbacks are exhausted. Every requested
auxiliary capability and required gate remains intact, with explicit required
metrics refusal and legacy-peer intersection coverage. Source, transport and
capture-credit pacing remain active without those advisory decoder samples.

Three new transport regressions also pass with the original real UDP/TLS
fixtures, covering queue accounting, scope/role/deadline checks, input-lane
independence and refusal on upload or unselected downloads. Complete current
wire, transport, media, client and files libraries compile. Complete client and
files production sources and both complete test targets pass strict Clippy. The client/files
integration executables and these strict checks use full current sources with
verified pinned build artifacts; they are scoped compiler runs, not a completed
Cargo workspace gate. Final core validation includes concurrent main `413b5ed`
and the subsequent profile and lint corrections.

Three authored daemon/viewer regressions exercise the production loops,
ordinary-download refusal and preservation of an unconsumed host job when the
peer has not selected synchronization. Their execution, full daemon/workspace
gates and native desktop/live-tailnet qualification remain unverified. Full
daemon code generation exceeded available resources; no successful native
desktop run is inferred from the portable tests. UBS found no actionable new
production issue in the reviewed changes, but its broad scan remained partial
on the same higher-order native command analysis as the baseline. The separate
private-portal test fixture disables unsupported TCP AppArmor label lookup
without changing production bus checks or weakening name-ownership assertions.
Transport source: `4da871c`; file-engine source: `4fc8562`; native integration:
`312100a`; scoped work: `fr-p4-folder-sync-p6i.1`. Both the scoped item and broader
folder-sync parent stay open for the remaining qualification.

## Original macOS system-shortcut capture (2026-11-23)

The Mac native client now implements the previously refused
`--capture-system-shortcuts` option. After explicit selection, Accessibility
preflight and an actual control grant, the original viewer owns one modifying
CoreGraphics session event tap. Physical keys, including Cmd+Tab and Cmd+Space,
enter the existing bounded Rust input source with their original native
timestamps, original view context and original client deadline. A later view
or ticket renewal cannot reauthorize a key queued by an earlier callback.
The tap filters only the current focused, visible viewer while
its original input deadline remains valid. Local Send Text composition keeps
its own keys. Fn/media/system-defined events are outside this keyboard slice.

A separate visible Off strip is reserved before picture negotiation. Native
focus, session and geometry checks, a 50 ms pump watchdog, a fixed callback
budget, original timestamp floors and clear-held-state rearming prevent
background or stale keys from acquiring fresh authority. Tap timeout or disable
is terminal and never automatically re-enabled. Explicit capture consent is
consumed by its first input attachment, so a recovered viewer and a later F8
request cannot reinstall a retired tap. Input-source closure and actual native
callback removal are separate cleanup facts, both required before reuse.

The ordinary `fr app` machine picker now exposes **Capture system shortcuts**
for a controlling connection in a `macos-desktop` build. It starts unchecked
on every opening, clears when switching to viewing only, and never becomes a
saved preference. A successful, typed picker report carries the choice to
exactly one selected connector invocation; missing, malformed, unsupported or
viewing-only consent refuses before launch. The picker itself never installs
an event tap or requests Accessibility permission.

Validation so far: exact-source component validation passes 24 tests (seven
deadline/custody cases, fourteen viewport-sizing cases and three actual
key/modifier translation cases). Another 79 tests cover the real picker Rust
boundary (eight) and application consent/report policy plus CLI parsing
(71); strict component Clippy passes for both slices. The application tests
ran on Linux, so they execute the unsupported-build refusal rather than the
positive Mac-only argument branch. Full local workspace check/test compilation
could not finish under shared memory pressure; the actual daemon metadata
compile was killed.
This environment has no macOS SDK, so the Objective-C bridge, complete Mac Rust
integration, native picker interaction and live Accessibility/interception/Off
behavior are **not yet qualified**. No successful native interception or
hardware claim follows from portable tests. Bead `fr-cwcp` retains this
qualification work.

## Windows explicit copied-file uploads (2026-11-23)

The connected Windows viewer now offers **File transfer → Send copied files**.
An explicit local menu gesture reads Explorer's Unicode file list on the
existing bounded selection/preparation worker, pins all selected source files,
and submits through the original controlled upload owner. The same progress,
host publication receipts, pause, resume and cancellation controls apply.
Content-free selection status is visible in the menu, including clipboard
busy, changed selection, no copied files and expired choice.

The original gesture age, control/view context, connection attempt and native
lifetime survive clipboard access, file preparation and final admission.
Copied-file work receives no dialog focus exemption. A changed selection,
backgrounded viewer, cancellation or expired original context cannot authorize
a later send. Native memory is bounded to 128 KiB before copying, with at most
eight files and 32,768 total UTF-16 path units. Clipboard-owned handles remain
borrowed and are unlocked and closed before source handles are prepared.
Malformed, ANSI, unsafe or excessive lists refuse the complete selection.

The full portable `fr-windows` Cargo suite passes all 259 tests, including nine
new parser, original-deadline and focus regressions; strict all-target Clippy
also passes. The exact production clipboard boundary and portable policies pass
all-target check and strict Clippy for `x86_64-pc-windows-gnu`, with the pinned
compiler, real Windows bindings, and dependency versions/checksums matching
`Cargo.lock`. These component checks do not establish a complete Windows viewer
build or live Explorer, delayed-rendering and end-to-end host publication
qualification. The existing clipboard bridge's strict-Clippy `manual_let_else`
finding is also corrected without changing its refusal behavior.

Plan §§15.3, 15.6, 16.1 and 19; tracking `fr-dd5h`;
[Windows usage](docs/windows-client.md).

## Video send capacity, transfer refill and Linux upload choice (2026-11-23)

Native QUIC video admission now reserves a packet for actual pending reliable
work, rather than always withholding an empty packet. This makes both slots of
the minimum congestion window usable when video is the only pending work.
Native congestion control and pacing, critical-first scheduling, original
deadlines and the 128-datagram bound remain authoritative. No impaired-path
throughput improvement is claimed without a measurement.

File and clipboard producers now release the exact fully acknowledged prefix
of their bounded flights, allowing new records while later records are still
retained. File maintenance reconciles acknowledged records before checking old
permit expiry. The original absolute transfer lifetime, outstanding record
deadlines and count/byte bounds remain in force; transport acknowledgement is
not a file or clipboard publication receipt.

Real clipboard regressions exposed an initial integration error: channel
promotion can retain the original `Attached` record, which was incorrectly
counted as preexisting application payload. The transport now captures this
setup prefix once at authenticated promotion and retires it only on actual
acknowledgement. It retains its original byte, count and deadline charges while
being excluded from application flight credit. Both clipboard startup and host
file downloads are covered; arbitrary preexisting application records still
refuse an exclusive producer.

Linux `fr connect --choose-files` now opens the installed FileChooser portal for
one to eight initial upload files. It reuses the existing receiving-folder
portal owner, with no new runtime, dependency, FFI surface or worker. The same
exact service/request identity, original setup/choice/cleanup deadlines, local
path validation and response-age limits apply. Receiving-directory pinning
precedes upload selection, and selected sources are pinned before connection.
Automation refuses interactive choice. See [CLI usage](docs/native-client-cli.md).

The current transport passes all 116 unit tests and all 38 native UDP/TLS
integration tests with the exact pinned compiler and byte-identical locked
third-party build inputs. Focused tests using the current production libraries
and actual UDP/TLS fixtures pass all five file flight, six clipboard batching
and one host attachment regression (covering both download roles). Eight
exact-source file-flight component tests also pass. The shared Linux portal and
URI/drop suite passed 34 tests, including 22 real libsystemd/private-bus cases,
and strict pedantic component Clippy; two later equivalent test assertion
diagnostic changes passed Clippy but their binary rebuild hit ENOSPC.

Whole-workspace formatting and scoped whitespace checks pass. Full Cargo
check/test/Clippy qualification remains open: workspace attempts encountered
disk exhaustion or compiler termination under shared resource limits. These
component results do not establish a full native CLI build, graphical portal
operation, live tailnet transfer, independent interoperability or WAN/hardware
qualification. Plan §§11.2, 12.4, 15.3, 15.6, 16.1 and 19; tracking
`fr-video-send-room-m13x`, `fr-transfer-ack-prefix-6xkv` and
`fr-linux-upload-picker-cdg8`.

## File cancellation at receive publication boundaries (2026-11-23)

The original disk worker now checks the runtime's published cancellation atomic
before every authority projection attempt and again after that projection. Parent
cancellation during a policy-lock wait or contention backoff revokes the separate
file permission before a clock can reach the final rename gate. The final check
does not acquire a runtime lock after sampling time. Mailbox admission uses the
same atomic so that its cancellation check respects the nonblocking contract.
The original authority, clock cursor, bounded retry count and already-published
receipts are retained; file cancellation does not revoke unrelated input authority.

Four deterministic regressions are authored for ordinary and final projection,
cancellation during contention, fresh clocks and bounded live retries, and real
file/nested-directory completion at each of its three clock callbacks. The last
case targets the final rename boundary and also asserts staging cleanup, returned
byte/transfer quotas and retained input authority. These regressions have **not
executed** in this environment.

The exact changed worker and tests pass pinned-nightly rustfmt and whitespace
checks, with independent source reviews. Whole-workspace formatting passed on
`2ad2ad7e` before this patch; a later full formatting attempt timed out. All-target
Cargo and complete dependency-build attempts were interrupted under shared
resource pressure. A final bounded exact-source clock-component attempt stalled
before its first compiler-start marker and was also interrupted: it establishes
no compile, test, mutation or Clippy pass. Complete current `fr-files`, real
publication and mailbox integration execution, and full final-revision Cargo
check/test/Clippy remain open. Scoped UBS exited 127 because it is unavailable.

Earlier commit `2ad2ad7e` corrects the four missing `microphone_available` fields
that broke Linux native picker test compilation, plus the concrete recovery and
Windows strict-Clippy failures in verification run `45108432935`. Those changes
retain the original assertions and behavior. Its hosted checks were cancelled by
later pushes, so no new hosted test pass is claimed. Plan §§7.3 and 15.6; the
broader `fr-p2-file-transfer-m7o` and `fr-rc-gates-cargo-test-6d8` remain open.

## Native file outcomes and Linux receiving-folder choice (2026-11-23)

The native app now checks the original upload outcome as well as the receiving
disk owner before opening another viewer. Its notice distinguishes host
publication, refusal and unconfirmed durability. Unknown publication,
contradictory or missing receipts, and unconfirmed source cleanup stop the app;
known effects from both transfer directions remain in terminal output. A
cancelled batch may have an unstarted suffix: batch completion is not proof that
every selection started or that any file was published. Commit `82f2fee` adds
this behavior and preserves an opted-in empty upload's explicit terminal state.

Linux desktop builds now offer a fresh local receiving-folder choice through
the installed XDG FileChooser portal, using the same one-connection consent and
safe pinned-directory receiver as macOS. The private bus checks the original
same-user portal owner, exact request handle and terminal response. Setup,
choice and cleanup have fixed deadlines; cancellation, expiry, ambiguous
selection and unconfirmed cleanup cannot open a remote session. The directory
is pinned before transfer setup, and a later choice cannot reuse its consent.
Explicit `--receive DIR` remains available. Local setup failures retain an
explicit receive-not-started result; a diagnostics-export error after a completed
Linux session retains that session's original closure and transfer evidence.
This does not add Wayland hosting.

The exact current app completion and terminal source passes 30 focused tests
(17 upload, 11 receive and two terminal projection tests), plus strict
pedantic component Clippy. The exact native portal and URI/drop sources pass
25 tests, including 13 exchanges through real libsystemd and a private
`dbus-daemon`; test and production component Clippy pass with warnings denied.
The actual standalone `fr-native` library passes Cargo check and strict Cargo
Clippy with `--no-default-features --features linux-directory-picker`. The
complete current runtime and chooser wrapper pass a production type check,
strict Clippy and four focused tests against the real pinned Asupersync runtime.
Three preflight/terminal component tests also pass against the original type
declarations and receive-summary methods. Whole-workspace formatting and
whitespace checks pass.
The private test broker uses anonymous TCP because the executor cannot create
Unix sockets; production still requires the local Unix bus and same-user
identity. These checks do not exercise a graphical desktop chooser or a live
file transfer. Full Cargo attempts were interrupted under shared-memory
pressure before tests; UBS reported an incomplete Rust analyzer run. Full CLI,
platform and live-peer qualification remain open. Plan §§7.3, 15.6, 16.1 and
19; tracking `fr-19fv` and `fr-8ieq`;
[usage](docs/native-client-cli.md#desktop-application-fr-app).

## Original application owners during video recovery (2026-11-22)

Controlled video recovery now services the exact existing file and clipboard
application owners before each original parent transport turn. Queued
acknowledgements are reconciled first, as in steady-state streaming. Local
file Resume, Pause and Cancel commands, clipboard retirement and bounded
cleanup no longer depend on receiving a replacement video configuration.

Before detaching the old media binding, recovery reasserts its idempotent
input-suspension fence and services optional application setup against that
original binding. It retains the first suspension timestamp, original control
lease, consent, worker, file receipts and cleanup state. It creates no new
application owner, input grant or recovery deadline. Read-only recovery still
has no controlling file or clipboard owner.

The added real-transport regression deliberately withholds replacement video
while resuming, pausing and cancelling an original two-file selection. It
requires the first file's cancellation receipt and disk cleanup during the
recovery wait, with the second file unstarted, intake closed, no input effect
and no published destination. Video decode and the input sink are explicit
fixtures. The regression and 13 adjacent recovery/audio tests remain
**unexecuted**: the selected-test compiler was killed by signal 9 before
producing a binary. An actual-source, production-only metadata check also
failed on allocation under the 3 GiB virtual-memory cap; it produced no
metadata. Neither attempt establishes a passing `frd` type check.

That bounded attempt used genuine build inputs from successful integration
run `45037551884` (`34628997`), with the exact matching nightly, Cargo.lock and
Asupersync pin. Current `fr-client`, `fr-files`, `fr-tailnet` and `fr-web`
compiled against that graph, as did the locked platform dependencies with
the actual `frd` features. No production source, runtime or test assertion
was substituted. Independent source review, whole-workspace formatting and
whitespace checks pass. The exact full-workspace `cargo check --all-targets`,
`cargo clippy --all-targets -- -D warnings` and `cargo test` each exited 101
while the unchanged pinned Asupersync compiler failed a 276,824,080-byte
allocation under the same 3 GiB cap. Those invocations reached no workspace
test execution; the separately passing 1,804 portable tests are a different
scope. Full runtime/platform qualification remains open.
Plan §§7.3, 11–12 and 15.3/15.6; tracking `fr-bh6c`.

## One-use microphone selection in the native app (2026-11-22)

`fr app --microphone-source NAME [--microphone-server PATH]` now retains an
explicit source for the next confirmed machine choice. A Linux microphone
build exposes **Show microphone controls** in the picker (F8); the choice
starts off, requires control and is cleared by a role downgrade. The existing
supervised viewer opens its muted panel, with separate **Allow microphone**
and **Talk** gates. Playback or a successful connection never starts capture.

Only availability reaches the selection-only child. Source names and server
paths stay out of picker reports and saved preferences. Declining, selecting
view-only, validation failure or a failed viewer launch consumes the source;
an unused picker refresh retains it. Unsupported builds refuse explicit source
options before opening the picker. Existing macOS per-choice default-device
pinning is preserved, and an explicit source can override it for one choice.

The complete actual app/options/terminal sources pass 133 tests with the Linux
microphone feature enabled and the same 133 tests with it disabled, plus strict
all-target Clippy in both modes, including nine new source-custody, grammar and
parent-report cases. This source-component
harness uses exact supporting excerpts and real portable dependencies; it
does not execute the full native CLI, live capture or runtime orchestration.
The complete actual Linux picker and native adapter pass 12 tests linked to
the current C implementation and real X11/XCB libraries. Both native C files
compile with strict C11 warnings. The complete `fr-client` crate passes its
615 tests. Whole-workspace formatting and whitespace checks pass. Scoped UBS
was attempted; changed-line findings were reviewed without identifying a new
defect, but Rust analysis remains incomplete because the command-executable
detector cannot analyze existing native-callable escapes in app/options. The
same limitation reproduces on the original source. Native picker interaction,
live microphone forwarding and full
CLI/platform qualification remain separate gates. Plan §§15.4 and 16.1;
tracking `fr-dyhq`; [usage](docs/native-client-cli.md#desktop-application-fr-app).

## Original Windows keyboard-hook delivery (2026-11-22)

The native shortcut hook now retains the physical event timestamp in a fixed
mailbox instead of replacing it with a window-posting timestamp. The mailbox
bounds both retained transitions and outstanding native notifications at 64.
Notifications carry only never-reused ordinals; input stays bound to the
original connection attempt. Focus and close retire pending keys, repeated
presses consume no new credit, and stale deliveries cannot reach a successor.

Queue overflow, posting loss, missing predecessors, invalid native timestamps
and expired delivery fence the original input owner. An invalid timestamp can
no longer silently lose a release. Full native age still passes through the
current grant's capture gate, and AltGr pairing uses original event times.
The shared close helper claims that exact attempt under its existing owner
lock; input failure never fabricates successful focus cleanup.

The complete production key policy and mailbox pass 20 pinned-nightly tests
(including 12 new hook cases), compilation with warnings denied, and strict
pedantic component Clippy with the repository's existing allowances. These
checks execute the actual portable sources, not Win32 or the full Shared
runtime. A subsequent full Cargo run passes all 250 `fr-windows` tests on Linux;
the combined `fr-core`, `fr-media`, `fr-client` and `fr-windows` baseline passes
1,804 tests, including three compile-fail doctests. Native Windows
compilation/execution and the added Shared-owner
regressions remain separate, unrun gates. Formatting and scoped whitespace
checks pass. Plan §§7.2–7.3 and 15.1; tracking `fr-j2dc`.

## Original-view robot startup and receipt freshness (2026-11-22)

The production macOS desktop build now includes the authenticated local robot
owner. Its opening pump waits for the actual first renderer receipt before
requiring current picture geometry; the same current, nonzero picture and
foreground checks still gate every input action. After presentation, geometry
loss remains terminal. Local stop, focus loss and geometry cleanup retire the
original command owner before native work can delay that fence. Original late
host receipts and the existing graceful close path remain intact.

Linux controlled screenshots and live read-only observation now retain the
original initial presentation timestamp through bootstrap acknowledgements and
native startup. Installing an endpoint cannot reset receipt age. The shared
snapshot owner refuses future-dated receipts and retaining old pixels creates
no input authority. This builds on the concurrent macOS robot integration in
`f1b71d0`; that implementation is preserved.

Two exact-source startup/foreground regressions passed on the pinned nightly.
The portable baseline passed 1,802 tests across fr-core, fr-media, fr-client and
fr-windows. The real CLI screenshot test build was killed with SIGKILL while
compiling the unchanged pinned Asupersync dependency with one compiler job,
debug information and incremental compilation disabled; its tests did not
execute. An isolated metadata-only attempt, with one frontend thread and no
code generation, was also killed before producing runtime metadata. Formatting
passes. Scoped UBS findings on the changed code were
reviewed: test assertions and an integer-geometry comparison are expected;
the existing UID equality warning concerns public kernel credentials. The
freshness scan also encounters the existing linux.rs detector limitation.
Full CLI/Session builds, AppKit/Metal execution and live-tailnet input remain
separate gates. Plan §§7.3, 15.1, 16.1 and 18.1–18.2; tracking `fr-62yx`.

## Explicit capability hints in the native machine picker (2026-11-22)

`fr app --probe` now runs the existing authenticated capability inspection
before showing the Linux or macOS machine picker. Rows show advertised viewing
and control support, a typed probe failure, or that a host was not probed. The
hints follow the original stable identities through sorting. Failed and
unprobed hosts remain selectable; the actual connection repeats identity,
capability and consent checks. Advertisements do not imply desktop readiness or
permission, and probes open no desktop observation or input session.

Probing defaults off, is scoped to this application invocation and is not saved
as a preference. The sweep retains its 64-host, five-second per-probe and
twenty-second overall bounds. Linux uses up to four concurrent probes; macOS
uses one to preserve the installed LocalAPI provider's process-wide lookup
credit. The private picker flag refuses custom-socket ambiguity. Native labels
remain bounded, and the overview help fits its existing 16 KiB limit without
weakening that assertion.

All 21 current discovery tests passed, including the new serial-credit and
identity/hint sorting regressions. A harness including the complete actual
host-picker and CLI help modules passed all 15 tests. It compiled the real
host_picker.c and x11_connection.c with strict C11 warnings and linked the
installed X11/XCB libraries; no native or runtime stubs were used. These tests
cover bounded labels, identity, cancellation, worker joins and help routing,
but do not draw a live picker. A separate CLI component harness passed 67 tests,
including all four new parsing/identity/probe-hint regressions. It includes the
complete actual options and host-choice modules, exact source excerpts of app
parsing and argument generation, and genuine portable protocol dependencies;
it excludes runtime probe orchestration and receive I/O. Independent source
review checked opt-in, deadlines, identity retention and native row layout.
The full CLI/native builds share the runtime blocker above; native picker
interaction and live-tailnet probes remain open.
The picker UBS scan reports analyzer limitations in several CLI files; its
reported changed-line findings were reviewed, not counted as a green scan.
Plan §§4.1, 6.4 and 16.1; tracking `fr-da7t`.

## One-shot native macOS observation (2026-11-22)

`fr robot observe NODE --native-once --experimental-native --display HANDLE|only`
now opens one explicit macOS read-only view, retains the original first receipt
and optional `--screenshot PATH`, then closes with `InspectionComplete`. The
first serve callback completes the observation without waiting for another
frame, including on an idle desktop. The existing authenticated connection,
host approval, native presenter and mandatory cleanup owners remain in use.

The finite session refuses control, auxiliary lanes, a live publisher,
interactive display selection, a second attempt and resize/display reopen.
Screenshot work uses the original observer before serving and preserves the
original monotonic receipt timestamp. Only a successful supervised operation,
confirmed native cleanup and absence of terminal/local stop can release its
single historical receipt. Normal host refusal, cancellation and cleanup
failures keep their actual outcomes and do not publish an image.

The CLI checks requested evidence and original frame identity before opening a
new private PNG. It reports actual physical viewport dimensions independently
of source display geometry, exact snapshot generations and receipt age through
readback, closing and PNG I/O. Capture/presentation wall times, source freshness,
input authority and physical visibility remain unknown or absent. Local cleanup
and the host exchange's acknowledgement, transport and reported cleanup/effects
retain their independent stages, including on an artifact error.

Passed: seven actual-source finite-owner policy tests and 28 CLI component
tests (seven new historical report/artifact tests plus 21 existing tests), with
strict all-target Clippy. The complete actual local presenter trait and Mac
presenter/decoder delegation also pass strict Darwin-target Rust Clippy. Scoped
UBS review found no changed production defect; its existing detector limitation
on `linux.rs` was reproduced on the baseline and those changed lines were
reviewed directly. Pinned formatting and whitespace checks pass. Full Session policy and platform CLI
tests are authored but not executed: the pinned runtime dependency exceeds
executor memory before whole-session builds. Apple SDK compilation, native
Metal/VideoToolbox and live-tailnet execution remain open. These component
checks do not qualify native hardware. Plan §§16.1 and 18.2; tracking `fr-5vqh`;
[usage](docs/native-client-cli.md#one-shot-native-screenshots).

## Screenshots from the original macOS observation (2026-11-22)

An explicit macOS foreground view opened with `--publish-observations` can now
answer `fr robot observe NODE --connection-id ID --screenshot PATH`. The exact
original observer installs its bounded snapshot handle before publishing its
first receipt. Authenticated local IPC retains the existing two-second request,
single export reservation, generation checks and final success seal. The
requester writes the PNG only after validating that complete original reply.
No new remote connection, input authority, host capture or automatic recording
is created.

The local presenter accepts `ReadPresentation` on its existing drained native
task. On-demand Metal readback uses the same admitted picture, color range,
fitted viewport and remote cursor composition as normal presentation. Frame
identity and native geometry revision are checked before and after GPU/CPU
work. Decode-only reference progress cannot relabel the retained image. Routine
absence, insufficient credit and allocation failure leave viewing usable;
native faults retain the original failure and cleanup requirements.

Retina and enlarged viewports report their actual physical dimensions separately
from the remote source. Only explicit snapshot replies receive the independent
32 MiB pixel ceiling; ordinary media/control limits are unchanged. Exact declared
viewport dimensions and frame/configuration/recovery identity are validated
before publication. Existing-file refusal, private new-file permissions and
unknown host freshness/physical visibility remain explicit.

Passed: 1,288 actual `fr-client`/`fr-media` tests; 17 actual-source Mac callback
and retained-snapshot policy tests; 21 actual-source CLI screenshot parser,
metadata and PNG tests; strict all-target component Clippy; cross-target Rust
decoder-wrapper Clippy against `x86_64-apple-darwin`; pinned formatting and
whitespace checks. The wrapper check includes real foreign declarations and no
FFI stubs, but omits AppKit/native linking and the full Mac session module.
Scoped UBS review found no new production defect. The pinned Asupersync build
attempts hit executor memory limits before the new worker tests could execute;
the lower-memory metadata-only check was also blocked in that dependency.
Runtime worker/media tests, full native Cargo gates, Apple SDK,
Metal/VideoToolbox and live-tailnet qualification remain open. Plan §§16.1 and
18.2; tracking `fr-a6gp`; [usage](docs/native-client-cli.md#screenshots-from-an-existing-foreground-owner).

## Native-app microphone controls (2026-11-22)

The macOS machine picker exposes the existing microphone owner through a fresh,
default-off Show microphone controls choice. Only controlling connections in a
local `macos-microphone` build can select it. View-only clears it; every new picker
starts off. The parent requires the explicit Boolean in a successful scoped
selection report and independently checks local support before composing the
ordinary `--microphone --microphone-source default` viewer command. Selection-only
commands refuse microphone/device options and perform no device preparation.

The viewer pins the current CoreAudio input once and checks its original signed
worker before network startup. The existing muted panel still requires separate
Allow microphone and fresh Talk actions. Playback, reconnects, preferences and
permission restoration cannot enable capture. Linux's native picker rejects a
microphone choice and keeps its existing ABI; macOS's native ABI carries the
local build availability explicitly. No native source, worker, host endpoint or
capture-authorization policy was replaced.

Passed: 148 actual-source app/parser/component tests and strict all-target
component Clippy; six tests compiling the complete Mac picker Rust module
without native stubs, plus its strict all-target Clippy; repository-wide pinned
nightly formatting and diff whitespace checks. The picker tests cover every
choice-bit pattern across availability/control states and fresh-window reset.
The component host is Linux: AppKit calls and the app's positive macOS build
availability branch were not executed. Full native Cargo gates, Apple SDK/AppKit
and physical microphone/live-tailnet qualification remain open.
Plan §§15.4 and 16.1; tracking `fr-app-microphone-controls-w7qe`;
[microphone usage](docs/macos-microphone.md#desktop-application).

## Native macOS receiving-folder selection (2026-11-22)

The ordinary macOS application can request receiving without launch-time
arguments. A fresh controlling Receive files choice starts a directory-only
native chooser in the selected viewer, before remote lookup. The existing
bounded NSOpenPanel owner confirms cleanup before exporting one local path.
The original receiver immediately pins that directory before any upload chooser
opens; both directions retain their existing limits, lease and cleanup owners.
The graphical route uses actual receiver defaults and rejects conflicts.
Explicit app `--receive DIR` destinations and custom policy remain one-use.
No directory or consent is saved or passed through the host-picker report.

The direct `--choose-receive-directory` option is control-only, mutually exclusive
with explicit receive policy, and unavailable in selection-only or unsupported
platform modes. Preflight cancellation and local failures retain positive
receive-not-started metadata; later actual publication/cleanup evidence cannot
be replaced by that fallback. Existing uncertainty still stops the app.

Passed: 144 tests using the actual app, options, host-choice, completion,
terminal-report and portable file-choice sources; strict all-target component
Clippy with warnings denied; and repository-wide pinned nightly formatting and
diff whitespace checks. This includes the directory
choice bounds, cleanup/deadline fences and fresh-consent/parser regressions.
Independent review checked native C/Rust signatures and preflight ownership.
The component harness excludes native session adapters; feature-gated disk-owner
producer tests, Apple SDK compilation, AppKit execution and live-tailnet file
receiving remain unverified. Full native Cargo gates remain open. Plan §§15.6
and 16.1; tracking
`fr-app-receive-picker-5cb5n`; [app usage](docs/native-client-cli.md#desktop-application-fr-app).

## Explicit native-app receiving consent (2026-11-22)

The Linux/macOS app picker adds a default-off Receive files control for the
original launch's one-use `--receive DIR` destination. The child receives only
an availability flag. The parent requires a successful, correctly scoped
controlling selection and an explicit Boolean before forwarding the original
validated destination, limits and collision policy. Any confirmed choice,
including declining receiving, consumes the destination before launching the
viewer; unused picker refreshes retain configuration but never restore consent.
View-only clears and disables receiving, and a later control choice requires
fresh consent. No receive path or permission enters saved preferences or
receive-option Debug output. The upstream receive outcome and original cleanup
guard runs after every viewer, including a choice with receiving off.

Passed: ten exact-source Rust picker tests, strict C11 compilation, and ten
actual shipping-C picker scenarios using cookie-authenticated loopback TCP
Xvfb/XTest, including keyboard and mouse consent, unavailable destinations and
role changes. This direct C fixture preserves the production Rust local-display
restriction and is not a full native viewer or remote-session qualification.
App/parser/guard regressions are added; their complete Cargo execution remains
pending under severe shared-host I/O pressure. macOS native execution and live
file receiving are not qualified by these Linux checks. Tracking:
`fr-app-receive-once-59f9`; plan §§15.6 and 16.1; [app usage](docs/native-client-cli.md#desktop-application-fr-app).

## Original file publication receipts at sender expiry (2026-11-22)

The file sender reconciles a terminal receipt already queued on its original
authenticated file channel before returning an original transfer or batch
timeout. A valid durable or durability-unknown publication remains recorded;
the expired file lane still retires. No source read, payload send, renewed
deadline or next selected file is admitted. Identity, live authorization,
transport receive bounds and exact publication-proof checks remain required.
A late acceptance cannot advance the source, and a missing valid proof leaves
an admitted completion unknown.

Source reviewed and scoped pinned formatting passed. Four real UDP/TLS/ATP/disk
regressions cover transfer and batch expiry, late acceptance and authorization
loss. A separate reply-handler fixture covers both publication directions and
durability states; it is not reverse-native publication qualification. These
Rust tests are added but execution is pending at this source checkpoint because
the shared build host repeatedly stalled during dependency compilation. No
build, test or hardware pass is claimed here. Tracking: `fr-inyy`; plan §§7.3
and 15.6; [file protocol](PROTOCOL_FILES.md).

## Native app playback and per-connection file sending (2026-11-21)

The Linux/macOS `fr app` picker exposes the existing host-playback and file-drop
paths. Playback is a remembered, default-off preference for either viewing
role. A selection-only command carries it as a preference without preparing
an audio device; builds without playback disable and clear the option. It
never enables the microphone. The playback slice landed in `96ee288`.

Send files is a fresh choice for one controlling connection. Every picker
starts with it off; switching to view-only clears and disables it, and returning
to control does not restore it. Strict picker completion requires a boolean
and rejects file consent without control. The app passes the confirmed choice
separately from saved settings to the existing `--file-drop` session path.
The picker admits no source files or transfers. Original upload ownership,
host admission/limits, cancellation and no replay on reconnect are unchanged.
Linux provides F5 for playback and F6 for file sending; both native pickers
also expose clickable controls.

Passed: ten retained completion tests and seven final exact-source app tests
after integrating the incoming native-port change. The app tests cover all 32
choice combinations at both ports 8443 and 9443, strict report/port validation,
and actual successful/failed child exits. The original launch port and the
confirmed file choice remain separate through the viewer command. Also passed:
ten retained Rust picker tests against the final production C; eight actual production-C/Xvfb audio
cases and six file-choice cases (initial state, controller opt-in, view-only
refusal, role downgrade and fresh consent); strict C11 checks and scoped
pinned formatting. Component checks use the actual completion implementation
and real client/serialization dependencies. They do not execute the full CLI option
parser or qualify a live desktop session or native macOS UI/audio/file transfer.
The all-feature workspace test attempt was interrupted during dependency
compilation without a final exit receipt or test results; a full-suite pass
is not claimed. Whole-native check/Clippy and live platform qualification remain
open under `fr-3ax0` and `fr-6k00`. See [app usage](docs/native-client-cli.md#desktop-application-fr-app).

## Linux/X11 delayed release reconciliation (2026-11-21)

Release-only native snapshots now preserve bounded ownership of the key/button
releases they already cleared. A matching delayed release is consumed once,
before timestamp validation, without issuing another remote action or advancing
input time. A fresh press clears only its own previous allowance; unknown
releases, wheel input, replacement captures, lifecycle and emergency escape
retain their existing checks. The local Send Text editor retains the same
release custody across cancellation without treating retired gestures as held.

Passed: all 21 shipping-C text-composer scenarios on cookie-authenticated TCP
Xvfb with actual XTest/XIM; the two new delayed local key/button release cases
both failed against the original implementation. Strict C11 compilation,
pinned Rust formatting, independent source review and whitespace checks passed.
The retained-event fixtures demonstrate the failure schedule; they do not
establish a live-session incidence rate. A follow-up exact-source Rust harness
against real client/core dependencies passed all 23 tests: ten held-input/time
cases including the seven new regressions, and thirteen existing display-switch
cases. All four actual callers pass the new decoder argument. Full
native/workspace gates remain incomplete.
This is a focused continuation of `fr-linux-x11-send-text-z14z` and plan §§7.3,
15.1; physical desktop and complete viewer-to-host qualification remain open.

## Bounded native application completion (2026-11-21)

The actual `fr app` launcher now continuously drains its original picker/viewer
output through shared bounded completion capture. Windows preserves its 256 KiB
document allowance, while native commands preserve the producer's 2 MiB limit.
Only a complete final nonempty UTF-8 document can select a host or report an end;
malformed/truncated tails and read failures cannot reuse earlier success.
Picker selection also requires successful original-process completion.

Direct-child stdout drainage is bounded after process exit or pipe closure,
including inherited writers. The macOS Launch Services route uses a private
FIFO with a bounded missing-writer wait instead of an accumulating output file.
`open` success does not establish viewer success. Explicit uncertain outcomes,
incomplete cleanup and missing/contradictory viewer completion stop the app
before another viewer can start, retaining `unknown_external_effect` in JSON.
Known refused/cancelled/revoked failures remain retryable with their guidance.

Passed: 31 exact-source component tests (10 completion, eight real subprocess,
six FIFO and seven report/application-decision cases), strict component Clippy,
28 portable Windows session-output tests, and Windows all-target check/Clippy.
The shared completion code also passed the full `fr-client` suite. Real
`x86_64-apple-darwin` component check/Clippy covers the final report/decision
code and production child/FIFO/open-arguments branch with the pinned compiler.
These counts overlap; they are not a combined unique-test total.

Whole native CLI test compilation and an all-target metadata-only check were
both killed by `SIGKILL` in the unchanged pinned Asupersync dependency before
the modified native app was checked. Whole-native Clippy was consequently not
reached. No full native CLI, live Launch Services or desktop-session
qualification is claimed. `fr-y4fp` remains in progress for those gates. See
[application completion behavior](docs/native-client-cli.md#desktop-application-fr-app).

## Linux/X11 original-view Send Text composer (2026-11-21)

Ordinary Linux control sessions now negotiate optional committed Unicode.
After a host Text grant, F7 opens a local XIM-backed draft, Ctrl+Enter sends one
whole value up to 4096 UTF-8 bytes, and Esc/F7 cancels. Physical forwarding
continues outside the editor and when the optional input method is unavailable.
The original Source/view and non-reused editor serial fence every Send; its
original X timestamp and exclusive 100 ms deadline reach the existing shared
text queue without retiming, splitting, clipboard fallback, or replay.

Editor keys and buttons remain local through their releases. A bounded native
ownership ledger prevents same-batch F7 from swallowing an already-forwarded
release, and stable native snapshots reconcile only missing releases. Emergency
escape and complete-batch lifecycle checks run before text admission. Fresh XICs
fence delayed compose callbacks; queued XIM completion is consumed before a
later Send. A registered-display Xlib error dispatcher contains parent/window
destruction races without changing error handling for unrelated displays.

Passed: all 18 exact shipping-C cases on separate cookie-authenticated TCP Xvfb
servers using actual XTest input and the built-in XIM compose method; strict
C11 compilation with `-Wall -Wextra -Werror`; pinned formatting and diff checks.
Coverage includes Latin/CJK/supplementary Unicode, compose ordering, editing,
cancel/reopen, readiness/context changes, malformed/oversized values, focus
loss, redraw/cleanup after destruction, unrelated-display error isolation,
synthetic input, emergency escape, unavailable IME/surface, key/button routing,
and release-only reconciliation. The TCP fixture is used because this executor
denies Unix-domain socket binds; no user's display or keymap is touched.

Thirteen focused Rust regressions were added, including an actual native-loop
FFI text handoff. Rust compilation and execution remain pending the dependency
build at this checkpoint. External IME daemons, physical desktop input, and
complete live Linux-viewer-to-host text delivery remain unqualified under
`fr-linux-x11-send-text-z14z`. This adds
a viewer producer, not Text capability to host executors that lack it. Usage
and limits are in [the native client guide](docs/native-client-cli.md#unicode-text-on-linuxx11).

## Rust-owned browser WebCodecs media (2026-11-21)

The generated WASM package now exports `BrowserMediaAdapter`, and the production
WebCodecs player consumes that Rust owner. Rust validates the canonical HEVC
configuration and access units, derives the exact `hvc1` configuration, and owns
count/byte reservations across decoder replacement. The shared package loader
provides `createMediaAdapter` without creating an observation or control session.

Original decoder and frame identities survive late callbacks, non-cancellable
support probes, hidden presentation, reentrant callbacks, and native close
failures. Escaped output remains charged after decoder closure until its own
positive frame close. Native cleanup and Rust reconciliation remain distinct.
An unknown output that cannot close triggers terminal quarantine: no new
admission, release receipt or clean reset is fabricated, and JavaScript retains
bounded diagnostic metadata. Resource receipts are not receiver-credit records.

The final pinned release package passes 82 package-lane tests: 66 execute the
actual generated Rust/WASM bindings (40 media, 11 adapter, 15 transport), with
11 loader and five capability-probe boundary cases. All 40 portable `fr-web`
Rust tests executed under WASI and 37 ordinary Node tests pass. Strict all-target
WASM Clippy and pinned formatting also pass. These native WebCodecs handles are
simulated; real browser/OS/GPU support, physical presentation, authenticated
host-session/media/control composition and live tailnet execution remain open.
The host still reports `desktop_available: false`. See
[browser ownership and integration](docs/browser-client.md). Tracked by `fr-j4ra`.

## Canonical native browser startup boundary (2026-11-21)

The native HTTPS/WSS ingress now matches the served Rust/WASM adapter's
bootstrap and `FRD0` hello/refusal schema. An unavailable desktop returns its
explicit capability without allocating credentials or session authority. The
legacy socket authentication/attachment path no longer invents a session.
Canonical hello validation retains peer/role-bound, single-use nonces, bounded
records, exact origin/host/SNI checks, and terminal refusal without a retry loop.
The original startup deadline now also bounds silent TLS/HTTP/body/socket I/O.

New regressions cover the actual Rust adapter over TCP, actual verified TLS/WSS
with an ephemeral CA, replay/role/peer/version refusals, malformed frames, and
stalled headers, body, and first message. Pinned formatting and diff checks pass;
native Cargo compilation/test execution is still in progress at this checkpoint.
These test definitions are not claimed as executed results. `frd run` listener
composition, real host-session/media/control integration, H3, and live browser
qualification remain open under `fr-w9jo` and the browser ingress beads.

## Shared client lifecycle and decoder ownership (2026-11-21)

`ClientSession` now binds each projected control grant to the exact outstanding
request sequence. Focus loss, hidden windows, stale presentation, changed
mapping and display-target changes retire pending requests and retained grants.
Fresh presentation permits another explicit request; it cannot restore old
lease or ticket IDs. Removed displays do not silently retarget control, and
regressed or contradictory catalogs cannot replace the current catalog.
Authority-bearing lease and input-ticket handles no longer appear in its traces.

`DecoderScheduler` now separates admission, native decode completion and
presentation. Counts and bytes are checked before mutation; an oversized picture
or pressure from undecoded references produces a typed refusal. In-flight work
and undecoded references cannot be discarded to satisfy a queue limit. Only
completed presentation candidates and queued nonreference pictures can make room.
Decode callbacks consume an original local owner token, so a replacement
decoder cannot accept an older callback with the same wire frame number.
Disconnect and explicit decoder recovery fence admission and presentation while
retaining original reservations. A separate original-owner close token confirms
native cleanup before those reservations can be released or decoding restarted.
Focus and readiness changes alone do not tear down observation's decoder.

Validation includes the passing full `fr-client` Cargo test suite and 32 focused
session/lifecycle/scheduler cases compiled against the final production source.
All-target `fr-client` check and strict Clippy also pass. Pinned formatting and
targeted UBS analysis passed; UBS reported no critical
findings, and its warnings were confined to test assertions and test unwraps.

This shared session projection does not replace the authenticated input owner's
lease, ticket, freshness or submission-time checks. Portable core evidence is
separate from native decoder, browser desktop and live tailnet qualification.
The implementation is tracked by `fr-m5sn`.

## Served Rust/WASM browser transport and embedded package (2026-11-20)

The served shell now loads the real Rust/WASM startup owner. JavaScript sends
the exact canonical hello returned by Rust and passes bounded control records
back to that same owner. It no longer implements the legacy FRBA/FRAT/FRTK
session protocol or creates session authority from transport events. The DOM
projects Rust startup/refusal state, with desktop controls disabled until real
session/media/control operations exist.

Each attempt owns its adapter, reader, timers, and socket. Retirement fences
callbacks before cleanup, including reentrant replacement, pending body reads,
close-before-open, and late module creation. Package loading has a 15-second
I/O watchdog; Rust still receives the original start time, so loading cannot
extend its connection deadline. Both cumulative bootstrap bytes and control
records are bounded before copying into WASM. Peer diagnostic text stays out
of displayed failures. One shared module-load handler retains at most 16 pending
subscribers; cancellation removes a subscriber immediately, so repeated retries
during a stalled native import cannot retain an unbounded chain of old attempts.

`node scripts/build-web.cjs NEW_OUTPUT_DIRECTORY` checks the pinned compiler
and matching wasm-bindgen CLI, builds the release module, generates the web
pair, and runs the actual-WASM adapter and transport tests. The daemon build
optionally embeds that complete pair from `FR_WEB_ASSET_DIR`; it bounds and
validates both files before selecting them. Missing packages produce typed
HTTP 503, while incomplete or invalid configured packages fail the build.
Self-hosted routes provide the correct JS/WASM MIME types and the narrow
`wasm-unsafe-eval` CSP permission. See [browser build and limits](docs/browser-client.md).

Passed on the restored source:

- 47 Node browser I/O/UI/HEVC ownership tests and eight module-loader tests,
  including 40 cancellation/watchdog retries against one blocked dynamic import.
- 26 tests through the actual generated WASM: 11 exported-ABI tests and
  15 production transport/DOM integration tests with simulated browser I/O.
- All 35 portable `fr-web` Rust tests, exact release WASM build, strict all-target
  WASM Clippy, and pinned formatting of the changed Rust files.
- Six production build-script tests and nine production asset-catalog tests
  in each of the bundled and unbundled configurations, with strict standalone
  Rust compilation and Clippy. The bundled run used the real generated pair.
- Actual package-builder execution and nine malformed/missing/oversized package
  refusals; refused packages did not replace the accepted embedded bytes.

These results cover the Rust owner, actual WASM execution, JS integration and
asset packaging. Complete native-daemon checks remain blocked by the pinned
Asupersync compilation's resource requirements; UBS is unavailable after the
executor restart. The native HTTPS/WSS listener and authenticated host-session
handoff still need qualification, and WebCodecs custody/presentation, receiver
credit, input/control, H3, and live browser/tailnet gates remain open. The host
continues to report `desktop_available: false`. `fr-w9jo` and the broader browser
bead remain in progress.

## Executable Rust/WASM browser startup adapter (2026-11-20)

`fr-web` now builds an actual `cdylib` for `wasm32-unknown-unknown`,
with the exact already-locked `wasm-bindgen 0.2.128` dependency confined to
that target. Its versioned `BrowserAdapter` exports own bounded bootstrap
validation, canonical browser `ClientHello` encoding, canonical connection
refusal decoding, startup expiry, and shared-core lifecycle fencing.

The adapter accepts no JavaScript-created session binding. Bootstrap requires
the exact WSS profile/version, requested role, bounded nonce lifetime, and an
explicit desktop capability; missing capability refuses before nonce retention.
Each local Rust owner allocates non-reused attempt serials. Stale callbacks are
rejected before their bytes or timestamps can affect a replacement. The nonce
is issued once, and neither delayed bootstrap nor progress extends the original
deadline. A DOM snapshot exposes only bounded state and authority facts; it
contains no nonce or peer-controlled diagnostic text.

Passed before executor loss: exact-pinned release WASM build, wasm-bindgen web
generation, strict all-target browser Clippy, and all 11 Node tests that load
the generated JavaScript and instantiate the real WASM. The tests verify the
104-byte canonical hello against an independent literal fixture, typed host
refusal, lifecycle/attempt fencing, invalid clocks, malformed records and
bootstrap bounds. Generated code was 16,545 bytes of JavaScript plus 151,977
bytes of WASM in that build; these are package sizes, not performance claims.

After executor restoration, all 35 portable Rust tests passed (12 adapter,
12 browser-core, and 11 ownership tests). The exact-pinned release build,
generated web package, all 11 actual-WASM Node tests, strict all-target WASM
Clippy, and canonical formatting check passed again. The reconstructed files'
formatting is restored. The served shell and package integration are described
above; native host session/media integration remains open under `fr-w9jo`.
No running browser desktop, native TLS listener validation, positive WebCodecs
closure, or input/control qualification is claimed.

## Browser HEVC configuration lifetime (2026-11-20)

The JavaScript player now gives each asynchronous HEVC probe/configure operation
its own local owner, including retries of the same wire generation. Reset and
disconnect invalidate that owner before cleanup. A superseded success or refusal
cannot replace the current decoder, resize the canvas or publish `onConfigured`.
Cleanup callbacks are checked for synchronous replacement before either success
or failure is published. The application ignores explicit cancellation results
within the same connection as well as results from a retired connection.

Generation, codec and dimensions are captured before awaiting the probe; the
negotiated hvcC bytes are copied from the caller's original view. Actual decoder
construction/configuration errors resolve to a typed refusal with a static,
bounded diagnostic, and partial decoder cleanup is attempted. Probe failures
also omit arbitrary platform exception details.

Evidence: all 28 `node --test web/tests/*.test.cjs` regressions passed: 13 bootstrap
and application ownership tests and 15 configuration ownership tests. Syntax
checks passed for every `web/*.js` and `web/tests/*.test.cjs` file. Coverage includes
reset during pending success/refusal, same-generation out-of-order completion,
caller mutation, reentrant cleanup, constructor/configure failures and a close
attempt that itself throws. These tests use simulated browser objects. They do
not establish native decoder closure, escaped-frame custody, original-channel
credit release or a working browser desktop. The Rust/WASM adapter must connect
the Rust ownership core to actual WebCodecs output and close callbacks; that
integration and live browser qualification remain open under
`fr-p3-web-client-skk`.

## Browser connection cancellation and capability refusal (2026-11-20)

The browser shell now gives each connection attempt its own cancellation and
callback identity. Disconnect, pagehide and bfcache restoration abort bootstrap,
settle pending socket opens and fence queued continuations. Old fetch, socket
and connection UI results cannot mutate a replacement connection.

Bootstrap requires same-origin HTTPS and an explicit `desktop_available: true`
capability before any socket is opened. An unavailable or older host without
that capability is refused. A WebSocket opening is labeled as awaiting session
authorization; it cannot establish observation, fresh-view authority or control.

Evidence: all ten `node --test web/tests/bootstrap_availability.test.cjs`
regressions and syntax checks passed. They exercise asynchronous transport and
UI ownership with simulated browser objects, including cancellation during a
queued response body, reconnect, close-before-open and retired socket callbacks.
These results do not qualify a live browser desktop. The Rust/WASM adapter and
live browser integration remain open under `fr-p3-web-client-skk`.

## Browser decoder custody and local input fences (2026-11-20)

`fr-web` now retains every decoder instance and compressed-input/output charge
through reconfiguration, channel replacement, recovery and teardown. Callbacks
carry a non-reusable local decoder serial in addition to the wire configuration
generation. Actual decoder closure releases canceled input; escaped `VideoFrame`
outputs stay charged until their own close callback. Credit returns to the
original channel and epoch, with both frame and byte bounds retained across old
and new decoders. Reconnection requires positive cleanup of the original owners.

Visible presentation requires a decoded frame from the current owner. Source age
uses the shared authenticated clock-correlation protocol and its uncertainty;
presentation time cannot make old or future source evidence fresh. Hidden tabs,
focus loss, mapping changes, decoder loss and channel replacement retire pending
control requests and existing grants. A fresh view permits a new explicit request.
Host-monotonic ticket deadlines expire locally even while the view remains fresh,
and a late grant for an older request cannot restore control (`fr-57sb`).

Evidence: the pinned nightly browser target (`wasm32-unknown-unknown`) passed
`cargo check -p fr-web --all-targets --locked`. All 23 browser-core and ownership
regressions passed on `wasm32-wasip1` with Node's WASI runner. These are portable
Rust tests, not WebCodecs, live browser, GPU or tailnet qualification. The browser
Rust/WASM adapter, sustained ticket renewal and authenticated unchanged-source
progress remain integration work; this core alone does not provide a browser
desktop session.

## Late ACK reconciliation reads the socket again (2026-11-19 UTC)

The workspace now pins Asupersync 0f955f9d8. Before it, the ACK-only step
behind [late sender reconciliation](#late-sender-turns-reconcile-queued-quic-acknowledgements-2026-11-15-utc)
never read anything once the sender's PTO was due, and that is the only case
where it runs. Its receive wait was capped at the PTO deadline, but only the
transmitting drive services that timer, so the wait was zero and the step
returned at once. Reconciliation therefore always expired the record. The
closing exchange's settle loop, which uses the same step, spun until its own
deadline without reading.

This explains the six ACK-family tests that failed in every Rust verification
run from 565a0201 (2026-11-14) until now: three in fr-transport
(`native_quic`, `native_quic/reconciliation`, `media_attachment` clipboard
retirement) and three frd `running::tests::acknowledgements` loops. Their
negative cases passed only because nothing was ever read.

**Passed:** the new Asupersync regression
`an_ack_only_step_reads_a_queued_ack_after_the_loss_timer_is_due`, which fails
with the old wait (no packet received); Asupersync's acknowledge-I/O, native
QUIC and datagram targets; and, on trj in the builder image at load 35-150, the
full `cargo test --workspace --all-features --locked --no-fail-fast` with all
six ACK-family tests passing. The frd clipboard dispatch test that stalled in
the 2026-11-18 continuous-viewer nightly also passed there. No live
load-induced session end has yet been attributed to this cause.

## iOS scaled-picture input and cursor ownership (2026-11-16 UTC)

The active iOS app now maps the original approved display into the encoded
picture's input area, then applies its local zoom and pan. Metal samples the
whole picture into that same integer image rectangle; direct touches refuse
its bars and trackpad movement stays inside it. Cursor position, shape size
and hotspot are converted from original display pixels before drawing or
seeding the trackpad pointer.

The app obtains geometry from the retained frame's original live attempt,
using additive C accessors rather than relying on lossy VIDEO notifications.
Existing C frame/cursor layouts remain unchanged. Identical-size reconnects
retire the earlier mapping and queued input. The scoped cursor accessor also
requires a picture published after its decoder reset, so a newer cursor
cannot appear on an older retained image within the same attempt. A post-copy
generation check refuses mixed cursor metadata and bitmap revisions.

Rust/C regressions exercise original-owner lookup, output preservation on
refusal, same-size reconnects, cursor publication floors and retirement;
Swift regressions cover scaled and odd geometry, image bars, touch/trackpad
bounds and original-display cursor conversion. Independent source review is
distinct from execution: the local executor was unavailable for these
changes, and no new Apple SDK build, XCTest run or live device session is
claimed. The dedicated iOS client workflow added alongside the concurrent
upload integration now targets the actual `macos-session` and shipping Xcode
Simulator suites; completed results for the reconciled revision remain required.
See [iOS geometry and validation](docs/ios-client.md#scaled-picture-touch-and-cursor-geometry).
The scaling umbrella `fr-foub` remains in progress.

## iOS explicit file uploads (2026-11-16 UTC)

The shipping app now offers an explicit upload-copy picker while observing,
then requires a separate Control grant and Send once action. It uses the
existing ATP upload owner and host drop directory. One non-reused local token
binds preparation to the original connection attempt for at most 120 seconds.
Send consumes that choice once and rechecks its original input source,
visible current view, live session and 250 ms gesture deadline. A refused
choice cannot be retried or rebound to a replacement session.

Document-provider preparation creates a disclosed local upload copy, up to
256 MiB, on one background worker. Ordinary coordinated access protects
regular-file preflight; Apple's forUploading coordination makes a snapshot
whose descriptor is pinned before its accessor returns. The OS may unlink
that snapshot before the later native sender starts; the descriptor remains
the source, without pathname reopening or weaker identity checks. No
coordinator stays active while waiting for user input or transferring data.
Folders, packages, links, unknown metadata, changed names and changed sizes
are refused. Provider materialization and the local copy can happen before
Send; no file contents go to the remote host before that explicit action.
The cap does not promise a limit on provider hydration traffic.

The C ABI adds optional empty upload negotiation, original selection and
preparation, explicit Send, exact-batch Pause/Resume/Cancel and bounded JSON
status without changing FrConfig. Queued progress, actual host publication,
uncertain durability and original cleanup remain distinct. An unused or
declined optional lane cannot invent an upload or a receipt. Paths, filenames
and contents stay out of diagnostics.

A separate positive source-release witness follows actual original worker
collection, including after failed bounded cleanup. Terminal reporting alone
cannot release the Swift owner's security-scoped permission. Pending provider
work remains bounded after Discard, and Discard restores ordinary Control
without permitting another preparation worker. Picker dismissal addresses
only its original session. Give back control waits for successful original
cleanup and reopens the same pinned host for viewing, without carrying files
or automatic control intent.

Validation: all 12 exact committed selection-owner tests passed with the
repository-pinned rustc and -D warnings. Strict all/pedantic Clippy passed for
that real private module. Pinned workspace formatting and diff checks passed
on the reconciled source, including the added real-source regression for an
unlinked descriptor. The full portable Cargo test build was attempted but
rustc received SIGKILL while compiling pinned Asupersync, before project tests
ran. The subsequent lower-memory integrated check remains pending. Fourteen
Swift regressions and integrated Apple session/C ABI/Xcode tests remain
unexecuted; Apple jobs are queued. Live iPhone/iPad provider and host-publication
qualification is not claimed. Tracks `fr-ios-file-upload`, still in progress.
See [upload flow and provider contracts](docs/ios-client.md#send-a-selected-file-to-the-host).

## Original-display mapping for scaled native pictures (2026-11-16 UTC)

The Linux desktop now chooses its original-display mapped renderer whenever a
local picture box is stated, including native-size windows. A smaller canonical
picture retained by a shared publisher can therefore be sampled into the
original target without a native-size decoder mismatch. Odd display axes use
one even target for rendering and snapshot checks while retaining the complete
original input bounds. The even no-box legacy path remains exact-size.

The Windows viewer now derives video placement, cursor placement and pointer
translation from the approved original display. HEVC crop dimensions determine
texture sampling only. Its pointer mapping accounts for the shared input
owner's integer rectangle inside the encoded picture, including canonical
rounding such as a 1582-by-888 crop for a 5120-by-2880 display. Edge clicks and
drag releases remain inside that original input rectangle.

Approved display dimensions are retained under the original frame owner's
publication gate. A bounded event queue losing a VIDEO notification cannot
remove or replace that geometry. Configuration cannot redefine it within an
attempt. Identical dimensions on a replacement attempt still require a fresh
native mapping timestamp, and the viewer rechecks ownership after presentation.

Two Linux regression tests, eight portable Windows geometry tests and two
shared-session metadata tests were added; the existing selection and
configuration tests were updated. They cover shared smaller pictures, original
negative origins, sampling, odd dimensions, edge/letterbox input, late metadata,
notification overflow and stale reconnect/resize events. Independent source and
arithmetic reviews passed. These new tests have not yet executed locally: the
executor disconnected during unchanged baseline dependency compilation.
Hosted verification on the new revisions remains required; no native Windows,
real X11 scaled-host, live tailnet or performance result is claimed.

Baseline hosted compiler failures also revealed incorrect inclusion of the
native session tests by the portable download owner and a file-offer test passing
an Offer where a Selection was required. Those source defects and unchanged-test
lint failures were corrected without suppressing tests or relaxing gates.
See [fitted Linux presentation](FITTED_PRESENTATION.md) and
[Windows client geometry](docs/windows-client.md#scaled-pictures-keep-the-original-display-coordinates).
Tracks `fr-foub` and `fr-foub.1`, which remain open for broader qualification.

## Windows download cleanup and completion (2026-11-16 UTC)

`fr-windows view --control --receive DIR` now connects an explicitly selected
local destination to the existing ATP receive owner. It can receive regular
files selected by a Linux, macOS or Windows host and combine them with its
original upload on a positively negotiated duplex session. The Windows
directory is pinned and validated before connecting; per-file and cumulative
session limits and reject/keep-both conflict policy use the existing safe
receiver. There is no new transfer protocol or native filesystem implementation.
The reused Windows rename record now also reserves the complete native structure
plus the filename bytes, with short and maximum-length publication cases added
to the existing native regression.

The Receive files menu exposes staged progress, publication counts, durability
uncertainty and file-only cancellation. Completion JSON retains bounded original
receipts and actual disk cleanup without filenames, paths or contents. A
committed Windows rename with unconfirmed directory durability remains distinct
from an unknown publication effect. Reconnects and replacement views cannot
inherit receive consent or replay transfers, and original cleanup failures
prevent replacement. A failed session-thread spawn releases newly bound choices.
The original selection API and receipt JSON fields remain compatible with the
concurrent Windows download integration; stable reason/publication codes and
an explicit success result are additive. Overall command success now also
requires the original receive lane's successful publication and cleanup evidence.

The underlying receive path also preserves its first cleanup failure through
later collection, while keeping late real publication evidence. Setup refusal
before a disk worker starts now ends the original receive status and confirms
no-worker cleanup instead of leaving an indefinite negotiating/pending report.

The final reconciled Windows package passed 120 portable tests, including the
receive flags, quota and consent refusals, and strict all-target Clippy.
Pinned formatting, documentation links and whitespace checks pass. Added
shared-owner, capability-intersection and real TLS/UDP receive regressions are
present, but runtime execution was blocked before changed code compiled: rustc
was killed while compiling unchanged pinned Asupersync with one build job and
debug information disabled. Windows all-target cross checks were likewise
killed in unchanged `windows 0.62.2`, including a retry with single-threaded
compiler settings. Native directory/UI/transfer qualification remains pending;
no live Windows result is claimed by the portable checks.
See [Windows client usage](docs/windows-client.md#receive-files-selected-by-the-host).
Tracks `fr-p2-file-transfer-m7o`, which remains open for broader acceptance.

## Windows hosts can opt in to regular-file receiving (2026-11-15 UTC)

`frd run --encoder mediafoundation --control --files C:\Users\me\FrankenRemoteDrop` now reaches the
existing controlled Files lane on Windows. The selected directory must already
exist and pass the native actual-handle owner/DACL and no-reparse checks.
Windows option validation no longer rejects this supported combination; file
receiving still requires the original control attachment and a separate local
file permission. Missing or unsafe roots refuse before host startup with
Windows-appropriate diagnostics.

Three additional Windows regressions cover real root admission and limits,
unsafe DACLs, junctions and path namespaces, and CLI refusal ordering. Existing
Windows option tests now expect the supported configuration. These regressions
and real Windows transfers remain **unexecuted** after executor loss; source/API
review is not native qualification. NATIVE_FILE_RECEIVE.md records the opt-in,
regular-file-only scope, conflict policy and committed-but-unconfirmed directory
durability. Tree transfer, synchronization and reconnect replay remain
unavailable. Tracks `fr-p2-file-transfer-m7o`.

## Windows regular-file receive custody (2026-11-15 UTC)

The existing ATP disk worker can now receive regular files into a Windows
destination selected by its actual directory handle. Selection walks ordinary
local-drive path components individually and refuses reparse points and unsafe
owner/DACLs. The retained root denies delete sharing. Exclusive staging creates
a protected private DACL immediately; cancellation deletes through the original
file handle and closes it before releasing the transfer's reservation.

Before each final original-authority check, publication prepares its bounded
native rename record. The next operation renames the retained staging handle
into the retained root without replacement. Keep-both conflict retries perform
a fresh check. Existing integrity, file sync, separate file permission, transfer
deadlines and cleanup accounting remain in force. Windows tree profiles refuse
before worker admission or reservation.

A successful rename is committed with `DurabilityUnknown`: directory crash
durability is not established, and the destination is never rolled back or
automatically replayed on that account. This increment supplies storage and ATP
custody; the host CLI opt-in follows separately.

Sixteen new portable-component, native-boundary, filesystem and ATP regressions
are present. Independent source and pinned Windows API reviews found no remaining
blocker. The changed feature has **not compiled or executed** after executor
loss. The earlier 16 passing portable fr-winsys tests and successful pinned
Windows all-target check cover baseline `0ed1fb` only. Native filesystem and
original-connection transfer qualification remain outstanding. Tracks
`fr-p2-file-transfer-m7o`, which remains open.

## Late decoder configuration cannot revive retired presentation (2026-11-15 UTC)

Native decoder construction and its hardware query now finish before metadata
is committed under the original frame owner's publication gate. Replacement,
retirement and close refuse a delayed result: it cannot change a successor's
picture metadata or append VIDEO after terminal ENDED. Native image replacement
and clearing detach old images under the gate, then release native resources
after unlocking. No foreign constructor, hardware query or final image release
is added to the publication critical section.

Four further regressions exercise the retained-image configuration gap in both
range directions, publication serialization, stale attempts, close and late
terminal results. These tests use the actual FrameSlot and Shared paths; native
picture cases use real HEVC. Source review and pinned formatting passed before
executor loss. Runtime regression execution, complete workspace tests and
Windows/Apple integration remain blocked. Recovered files have not had a
post-recovery formatter/build run. The Linux workspace check was
interrupted during shared memory pressure; the Windows preflight lacked
MinGW, and the execution environment subsequently disconnected. No timeout,
dependency pin or qualification gate was relaxed. `fr-4cqh` remains open.

## Retained native pictures keep their own color range (2026-11-15 UTC)

Repainting a retained picture now reads the color range from that native image.
Acknowledging a newer decoder configuration cannot recolor the preceding
picture while the renderer awaits the new decoder's first frame. Windows and
the Apple C ABI use the same rule; callers no longer supply mutable current
configuration as metadata for old pixels.

A native regression decodes real limited/full/limited-range HEVC pictures and
checks the metadata returned with each retained image. The fixture's full-range
SPS was produced from the existing stream with FFmpeg's metadata filter.
Pinned source formatting, whitespace checks and independent source review
passed before executor loss. The native regression and complete Windows/Apple
integration remain unexecuted here. Portable baseline results (113 Windows
policy, 10 decoder-contract and six root-bundle tests, one ignored) are
separate evidence and do not qualify this change. Tracks `fr-4cqh`.

## Windows app switch pauses control instead of ending it (2026-11-15 UTC)

Switching from a controlling Windows viewer to another application now
pauses control. At the switch the viewer releases every key and mouse button
it pressed on the host. While another application is in front nothing is
forwarded, including pointer motion over the still-visible window. Viewing,
the grant and its renewal, and the text clipboard lane continue. Input
resumes once the pump sees the viewer in the foreground again, and only new
native events are sent: a release of something pressed elsewhere sends
nothing, and nothing is replayed. `Shared::set_active` closes the input gate
under the grant lock, releases held keys and then buttons at the host's last
pointer point, and retires unsent control choices. It returns `false` only
when a release is refused. The shell then ends control through the existing
focus-close path described below (or a hide), so a paused grant never keeps
input held on the host. Minimize, resize/mapping changes and external
file-picker focus loss still end control.

**Live** (oldsurface viewer `--control --seconds 75` against an X11 host on
ts2; xev on the host records the injected input; a console window takes the
foreground for 10 s). The pause build passed every check in 2 of 2 runs:

- The held key and button were released at the switch.
- Nothing reached the host while the viewer was in the background.
- After return, later key presses arrived, and the stale releases sent nothing.
- Text copied on Windows in the background application reached ts2, and text
  copied on ts2 after return reached Windows.
- Each session ran to `--seconds` with 0 reopens, at 27-28 drawn pictures/s.

The same script ran once on main without this change (3a6dd777 plus its
build fix). Held input was released at the switch. The original session then
ended `ok` / `Cancelled` instead of `CONTROL_STOPPED` / `Hidden`. The
focus-close proof was therefore absent, and the viewer exited 2 s after the
switch with `original_cleanup_unconfirmed`. Nothing after the switch reached
the host, and no clipboard text crossed. That is one run: it shows the failure
exists but gives no rate.

**Passed:** formatting, CI's Windows cross Clippy command, the fr-mobile-ffi
`macos-session` library tests (151) and Clippy, and macOS workspace Clippy.
**Not verified:** the refused-release fallback, live (it needs a busy input
queue at the switch). The macOS viewer still ends control on focus loss
(`fr-native` `halt`).

## Windows focus loss keeps a path back to fresh viewing (2026-11-15 UTC)

Switching away from an active controlling Windows viewer now fences its
original input immediately and keeps the native window. The original control,
clipboard and upload owners end through their existing terminal exchange.
Only successful original service and cleanup permit a disconnected dormant
window to await a fresh foreground event. Returning starts a new observing
connection through normal admission; **Input → Request control** is required
for a new grant. Pending input, clipboard content and file selections never
replay into the replacement.

The focus callback uses an exact-attempt witness and a lock-free input fence,
so a busy producer mutex cannot delay the fence and a stale callback cannot
close a newer attempt. Stop intent is recorded before cleanup can publish its
result. The public ending remains `CONTROL_STOPPED / Hidden`; separate positive
evidence, not that label alone, authorizes replacement. The retained GPU image
is cleared on focus loss and again after foreground return, with original
lifetime/foreground checks after native work and no invented presentation
evidence.

The original ten-second cleanup bound, invocation deadline, replacement cap
and cooldown remain. A fresh foreground request has its own bounded opening
phase; repeated callbacks cannot extend it or repair late/failed cleanup.
Minimize/restore, resize/mapping changes and external focus loss from a file
picker retain their existing terminal behavior.

**Passed:** all 106 portable Windows-client tests, strict all-target package
Clippy, pinned workspace formatting and an actual-source Windows GNU compile
and strict Clippy check of the renderer and geometry boundary. Independent
review checked the original-owner locking, input submission fences and native
reentry checks. **Blocked:** full shared session/frd/native-viewer compilation
and the new shared completion, no-grant and lock-contention integration
regressions, because unchanged pinned Asupersync compilation was SIGKILLed
before project code. No live Windows, GPU or tailnet result is claimed.
`fr-o26d` remains in progress pending those gates. See
[Windows usage](docs/windows-client.md).

## Late sender turns reconcile queued QUIC acknowledgements (2026-11-15 UTC)

A host or viewer whose own task wakes after a retained reliable record's
delivery deadline now has one bounded ACK-only receive turn before judging
that custody. Actual acknowledged stream offsets release retained records;
the unchanged expiry check judges everything left. The helper sends no
application data, retransmissions or probes and dispatches no input. It reads
at most one native receive batch, with a 1 ms receive wait inside a 10 ms
absolute turn bound. This is an observation-order correction, not evidence
that an ACK reached the kernel before its original deadline.

The normal host, admission-refresh and viewer loops call that turn before
synchronous service can judge retained-send expiry. Original cancellation,
connection/operation lifetimes, authorization, input and presentation gates,
receive deadlines and explicit attachment retirement still win. Expired
unsent or partially staged records and unsettled native output cannot use the
exception. Synchronous `tick` retains its original fail-closed contract; there
is no general delivery allowance extension or fresh input ticket.

Eleven real UDP/TLS transport regressions and five host/viewer integration
regressions cover successful reconciliation, missing ACKs, unsent tails,
queued payload, optional clipboard retirement, cancellation, authorization
and original lifetime precedence. **Passed:** pinned workspace formatting and
diff checks; independent source review of the native ACK-only path and its
host/viewer integration. **Blocked:** these runtime regressions and complete
workspace compilation. The unchanged pinned Asupersync compiler was killed
with SIGKILL before first-party code in normal, single-job reduced-memory,
Cranelift and metadata-only attempts. No test assertion has been claimed as
passing from those runs. The original scheduling-stall reproducer is retained
as a real network regression, but no live load-induced end has been attributed
to this cause. `fr-load-ends-cuqi` remains in progress, including its five full
suites at load above 100. See [QUIC records](QUIC_RECORDS.md).
Update 2026-11-19: once run, these regressions failed, because the pinned
Asupersync step never read the socket; see
[the fix above](#late-ack-reconciliation-reads-the-socket-again-2026-11-19-utc).

## Connected Windows file selection and control-to-view handoff (2026-11-14 UTC)

A controlling Windows viewer now offers **File transfer → Pick and send one
file…** without requiring a preselected `--send` argument. One native dialog
worker keeps the presentation thread responsive, then pins the selected regular
file through the existing Windows handle-custody boundary. The original upload
intake admits at most nine explicit single-file batches and retains each batch's
progress, host publication receipt and cleanup result. File-only Pause, Resume
and Cancel refer to the batch displayed by their original popup. Reconnect,
resize, display change and a new control grant never replay these selections.

Native choices retain their original dialog and accepted-gesture deadlines
through destruction, source preparation, joining and lock waits. Final admission
checks the exact original input source/view context and original sender lifetime.
A failed native cleanup remains failed after later collection. The final JSON
contains bounded receipt history and chooser cleanup, without file paths or
contents. No Windows receiving, directory sending, drag-and-drop or reconnect
resumption is added.

The original picker is registered before its worker can create a native
window, so activation of that exact UI thread does not cancel its own choice.
The actual common-dialog parent observes external focus loss and fences only
the captured grant, even if the Shared owner has moved to another attempt.
The viewer's deferred activation is scoped to its native lifetime and attempt;
final file admission checks the original viewer's actual foreground window.
Subclass removal and native destruction are required cleanup evidence, and
worker-thread identity is retired before the thread can exit or be reused.

**Input → Stop control and keep viewing** now fences the original native input
and closes its controlled connection before opening a fresh observing session.
The underlying source is retained through its terminal exchange; emergency
source loss still cancels. Replacement requires positive original service and
cleanup, retains the existing cooldown/replacement/invocation bounds, and starts
without replaying control, clipboard or files. Closing the window disconnects.

**Passed:** all 96 portable Windows-client tests and strict all-target Clippy;
ten new chooser-policy regressions; actual picker/menu/policy sources in a
narrow Windows GNU strict-Clippy check against Windows 0.62.2. That native
compile fixture substitutes the external Shared/Upload API and does not
establish full session integration. Thirteen upload-owner tests, two final
intake-admission tests, two original UDP/TLS close regressions and one stale
file-choice source-fence regression are present
but unexecuted locally: the unchanged pinned Asupersync compilation was killed
before reaching project code, including an isolated one-job reduced-memory
attempt. Complete workspace/runtime and real Windows chooser, transfer and
control-release qualification remain pending. See [Windows usage](docs/windows-client.md).

## Windows choices retain their original gesture and connection (2026-11-14 UTC)

The machine chooser retains its accepted native message timestamp through
queued callbacks and window destruction. Final consumption rechecks that same
event age as well as the original discovery/chooser deadline. A stalled cleanup
cannot turn an expired click into a later connection.

Change Display now binds its local popup to the exact internal connection
attempt, not only the shared session object and native mapping lifetime. An
internal reconnect can reuse that object; an older popup can no longer close
or replace the newer attempt. Current-attempt drawn evidence is required, and
native refresh and deferred processing recheck the original identities. The
existing cleanup proof, replacement bounds and fresh approved catalog remain.

These are narrow follow-ups to the concurrently delivered machine chooser and
runtime switching, preserving their implementations. Both the machine picker
and upload menu use the error constructor exposed by the actual pinned Windows
bindings. The earlier three native SendMessageW signature corrections are also
retained.

**Passed:** all 85 portable `fr-windows` tests and strict all-target Clippy;
pinned workspace formatting; actual final machine-picker source with real
fr-client/fr-core and Windows 0.62.2 in all-target Windows GNU cross-Clippy.
New regressions cover a reused shared owner with a different attempt and the
exclusive native gesture-age boundary after cleanup. Full native-session
integration and live Windows UI/transfer behavior remain separate qualification.


## Explicit Windows file uploads (2026-11-14 UTC)

`fr-windows view [HOST] --control --send FILE` now connects one explicitly
selected regular file to the original controlled ATP upload. The native File
transfer menu shows state, queued bytes, actual publication receipt and cleanup,
and cancels only that upload. An already published or uncertain effect is
never rewritten as cancellation. Host refusal details such as Conflict,
Permission and Integrity remain visible without paths or file contents.

The shared session consumes the selected handle once, offers optional Files
alongside existing text/clipboard, and starts source work only after the
original control grant and attachment. A refused or configured selection is
never replayed on reconnect. Viewing replacements omit the upload while
retaining its original menu and receipts. Shutdown uses the original cleanup
deadline and retains stopped disk owners through eventual nonblocking collection;
a missed deadline stays failed after a later join. The C ABI configures no upload.

The final JSON includes a content-free upload result. With `--send`, success
requires a durable host publication receipt and completed original source
cleanup as well as a successful desktop result; queued bytes cannot satisfy it.
This slice targets the existing Linux/macOS file receivers. Windows receive
directories, directory sending, downloads and drag-and-drop are not added.

**Passed:** all 83 portable Windows-client tests and strict all-target Clippy
after preserving the concurrently landed picker cleanup; pinned workspace
formatting; all 16 portable
`fr-winsys` tests and strict Clippy; isolated actual Windows file-boundary
all-target cross-Clippy, including compilation of its native regression tests.
Eight upload owner/receipt regressions and bilateral Files/text/clipboard
negotiation tests are added in the shared session. They require the full native
session graph and remain unrun locally because the pinned Asupersync graph
exceeds this execution environment's resources. Hosted full Windows integration
and a live Windows-to-host transfer remain separate, pending qualification.
See [Windows upload usage](docs/windows-client.md#send-one-file-to-the-host).


## Windows selected-file handle custody (2026-11-14 UTC)

Windows native file selection now retains the actual read-only disk handle and
its volume/file identity. The sender duplicates that same handle rather than
reopening a pathname or comparing the compatibility layer's placeholder Unix
inode values. Denying write/delete sharing keeps conflicting writers and
renames out while the original handle or a duplicate remains open. Leaf
reparse points, directories, non-disk objects, special device namespaces,
alternate streams and explicitly mapped network drives are refused. Ancestor
junction resolution remains an OS path operation; the drive-letter check is
not proof of physical-local storage.

This is the source-custody prerequisite for an explicit Windows upload, not a
claim of Windows receive support. All 16 portable `fr-winsys` tests and strict
all-target Clippy passed on the pinned nightly, including four new production
path-policy regressions. Native handle-sharing, rename and reparse tests are
present but still need a Windows runtime. Existing Unix selection is unchanged.


## Native Windows machine choice and in-view display changes (2026-11-14 UTC)

`fr-windows` and `fr-windows view` without a destination now open a native
machine picker from one bounded installed-Tailscale catalog. The chosen stable
ID and canonical certificate name remain paired through fresh installed-daemon
validation; cancellation starts no remote session. Explicit destinations and
connection options keep their existing behavior. The shared selected-target
API rejects removed identities, renamed nodes and same-name replacements.

The viewer now offers **Input → Change display…**. It fences the original input
and clipboard, requires positive original service/cleanup completion, and then
opens a fresh approved display catalog to the same machine. Replacement starts
viewing without replaying control or clipboard. Old picture admission stops
during retirement; the original replacement deadline remains enforced after
clipboard locking and native window preparation. Later resize replacements
retain the fresh-choice policy. Failed cleanup remains terminal and visible.

Three command regressions, four shared identity-selection regressions and nine
portable display-replacement regressions were added; existing invalid-metadata
and address tests also cover the new shared entry point. Independent source
review found and fixed the post-native-read gesture-age and replacement-deadline
cases. The exact shared connection files passed pinned formatting before the
execution environment became unavailable. The new Windows/UI/compiler and test
verdicts are pending in the existing CI lanes; no real Windows multi-monitor,
IME, clipboard, GPU or tailnet qualification is claimed for these changes.
See [Windows client usage](docs/windows-client.md). Both scoped feature beads
remain open for the missing verification.

## Windows adaptive bitrate through the live capture worker (2026-11-14 UTC)

The original Media Foundation capture worker now implements `CaptureAtRate`
for private and shared adaptive hosts. A target applies only during useful
capture work, retains the opening bitrate ceiling and both original two-second
clocks, and is reported only after exact native readback and an admitted IDR.
An unchanged desktop retains the previous rate; lost source evidence refuses
capture. Fixed-rate hosting remains the default. See
[the media commit](https://github.com/Dicklesworthstone/franken_remote/commit/a745f925)
and [Windows rate transactions](ADAPTIVE_CAPTURE.md#windows-media-foundation-bitrate-transactions).

**Passed:** 1,214 tests across `fr-core`, `fr-wire` and `fr-media`; all 12 portable
production rate/arbitration tests with strict pedantic Clippy; and strict
Windows MSVC Clippy for the `fr-native` library and binaries. The ignored native
hardware rate probe has not run. Windows device behavior and impaired-path
quality remain unqualified; the broader workspace and combined Windows build
remain resource-constrained, without a passing result.

## Native Windows display choice, capture and local consent (2026-11-14 UTC)

The Windows viewer now provides `--display choose|primary|only|HANDLE`.
`choose` opens a bounded modeless native picker from the original approved
display catalog. Transport and original startup deadlines keep running while
the user decides. A one-use local request binds each reply to the exact catalog
and attempt; cancellation, cleanup, close and replacement invalidate it before
later startup consumption. The mobile C ABI retains primary-display behavior.
This initial-selection slice passed 49 portable tests, portable strict Clippy
and pinned formatting. An isolated actual Win32 picker/binding check also
passed; the three full shared-session integration regressions and real Windows
multi-display operation remain separate qualification. See
[the display commit](https://github.com/Dicklesworthstone/franken_remote/commit/58ca090b6979cded00fd60429f2f769d30889f77)
and [Windows client usage](docs/windows-client.md).

The first full Windows-target integration check reached the actual viewer and
reported one strict Clippy failure in the picker-open branch
(`single_match_else`). That branch now uses the suggested equivalent
`if let`; denial/close behavior and warning enforcement are unchanged.
The complete hosted rerun remains pending.

The Windows host capture path now owns one full-display GPU texture. It copies
each new desktop image once before releasing the borrowed DXGI frame and
composes cursor pixels only in a clipped region using two fixed 256×256 scratch
textures. Cursor shape, visibility and position come from the same successful
DXGI acquisition; color, monochrome and masked-color cursors retain their
semantics without double-drawing an already-composited pointer. New images
discard old underlays; duplication loss also invalidates cursor state and
source evidence. Thirteen cursor
policy tests, six existing freshness tests and isolated actual Windows boundary
strict Clippy passed. This removes a full-display allocation/copy and full-screen
GDI mapping in source; a hardware frame-rate benefit and decoded appearance
remain unmeasured. See
[the capture commit](https://github.com/Dicklesworthstone/franken_remote/commit/f769230e2c65e0382af911bfa74bffe82848c36d).

Windows `frd run --approval local` now opens a native attributed Allow/Deny
surface for the original admitted viewing or control request. It uses the
existing capacity-one approval worker and one-use capability, validates the
same monitored interactive logon, and preserves original opening/startup
deadlines and join-before-consume cleanup. Ordinary injected remote mouse
events cannot approve. Native UI, source visibility and original authority
are rechecked before acceptance; privileged UIAccess software remains within
the trusted Windows/user boundary. All twelve production gesture/budget tests,
including the two reconstructed trailing regressions, now pass with the pinned
compiler and strict standalone Clippy. The isolated Win32 API check passed
before executor loss; the recovered production boundary and policy were
independently compared. Full local Windows feature-union
checking was SIGKILLed while compiling the broad pinned Windows bindings,
before first-party code. Complete host integration and real Windows
input/consent qualification remain unverified. The existing hosted cross-build
now also selects the host and capture entry points, which the viewer-only
graph did not compile. See [native consent details](LOCAL_NATIVE_APPROVAL.md).

## Native Windows committed Unicode text (2026-11-14 UTC)

The Windows viewer now exposes **Input → Send text…**: a native multiline
editor for Unicode, paste and local IME composition, followed by one explicit
Send through the host's negotiated direct-text capability. Physical input
stays local while the editor is open. The complete value is bounded to 4,096
UTF-8 bytes; malformed UTF-16, NUL, empty values and native overflow refuse the
whole draft. Send and cancellation clear the edit, preedit and undo state.

The shared Rust API captures the original source/view context at opening,
then freezes its checked send bound and the native Send gesture's age before
reading text. One accepted or refused attempt consumes that draft; a renewed
ticket, replacement session or changed view cannot revive it. Independent
native callback fences invalidate local cancellation/overflow and stop the
original source on window/session lifecycle loss, including reentrant calls.
This leaves the existing C ABI and physical-key path intact.

Ten portable regressions cover exact Unicode conversion, independent
UTF-16/UTF-8 bounds, consumed or invalidated draft custody, lifecycle loss and
text changes during a native read. All ten passed on the repository-pinned
nightly after the execution service recovered. The same source passed
`cargo fmt --all --check` and portable
`cargo clippy -p fr-windows --all-targets --offline --locked -- -D warnings`.
The pinned compiler's atomic revision API is `try_update`; the deprecated
spelling was corrected without changing its irreversible overflow behavior.

The shared no-grant regression exercises the new API; existing source-context
and capture tests cover continuity and original exclusive deadlines. Source
review found and repaired reentrant lifecycle and whole-value read gaps.
The actual Win32 editor and shared session passed hosted Windows cross-Clippy
in the completed [Rust verification run for 8e44fd60](https://github.com/Dicklesworthstone/franken_remote/actions/runs/43573636474),
which also passed the full Linux workspace checks, strict Clippy and tests.
Those commands used the existing macOS trust-root export for the Windows target.
No real Windows IME, host-insertion or live-tailnet qualification is claimed;
`fr-nuqa` stays in progress. See [Windows client usage](docs/windows-client.md).

## Established audio send expiry preserves the original desktop (2026-11-14 UTC)

Completed AudioDown and AudioUp pairs now retire independently when their
original reliable-send deadline expires between settled native I/O operations.
The transport withdraws that direction's unsent/retransmittable stream work,
retains consumed identities, and conveys a distinct send-expiry cause to its
peer. Playback and microphone owners stop the original native work before
another callback; completion distinguishes `audio_send_expired` and
`microphone_send_expired`. No original deadline, consent or retry policy changes.
Critical and incomplete exchanges, authority loss, unsettled I/O and retained
microphone output retain their connection fences.

Regressions exercise queued and native-staged records in both directions,
original control and sibling traffic, late-admission races, source-demand and
output cleanup, and preservation of the original microphone owners. Local
workspace checking and a later focused transport test build both ended in
SIGKILL while compiling unchanged pinned Asupersync. The focused attempt used
serial compiler frontend/backend work and stopped before any project tests;
its measured peak process-group RSS was 7.6 GiB. The kill's cause was not exposed.

Completed hosted native-audio and native-client jobs for audio source
commit [903d02d2](https://github.com/Dicklesworthstone/franken_remote/commit/903d02d2a5d6881a79d85392db47d762c3d07e20)
passed their actual logged checks and test commands. The completed Linux Rust
job confirms successful all-target/all-feature Check and strict Clippy, plus
128 passing media-attachment tests including every new transport send-expiry
case and passing host-owner expiry regressions. The full run was red on
formatting and one new client fixture, repeated in the private-microphone lane.

That fixture waited 2.1 seconds without new presentation evidence, exceeding
its 1.5-second view age. The production microphone correctly refused stale
readiness before receiving the peer reset. The corrected test keeps only its
explicit presentation fixture current on the same original input owner; no
grant, ticket, send deadline or production authority ordering changes. This
correction passed in both the workspace and private-microphone lanes of the
completed [Rust verification run for 8e44fd60](https://github.com/Dicklesworthstone/franken_remote/actions/runs/43573636474).
The run's actual Linux aggregate records success for formatting, all-target
checking, strict Clippy, the full test suite and its additional native/document
lanes. These results come from completed command logs, not continued-step API
success flags.
Native-device, live-tailnet and loaded-host qualification remain unclaimed;
`fr-jy4e` stays in progress.

## Complete iOS Unicode callbacks without delayed replay (2026-11-14 UTC)

The shared native control profile now includes the optional committed-text
boundary in its original reconnect offer and requests Text only through the
existing negotiated capability intersection. Hosts without that boundary retain
physical-key control. The existing C ABI functions, layouts and status values
are unchanged.

An iOS callback containing Unicode or any character without a physical-key
mapping now submits its entire original value synchronously once. Unicode no
longer enters the Swift typing queue. Pending ASCII, held hardware/sticky
modifiers, embedded NUL and an oversized UTF-8 value refuse before any prefix or
modifier effect. The 4,096-byte bound is checked on both sides of the C-string
boundary. Rust retains its first sample and exact source/view TextContext;
changed or temporarily unready contexts refuse without ending physical-key
control. Immediate refusal produces content-free feedback and retains no text
for a later refresh or reconnect.

The default keyboard admits supported committed callbacks. This remains
UIKeyInput rather than a full UITextInput composition/preedit implementation.
Admission to the bounded source is not proof of atomic host insertion, and no
per-action receipt UI is added. The existing host's partial-effect and no-replay
semantics remain intact. See [the iOS input guide](docs/ios-client.md#input).

Nine regressions were added: one portable native-profile offer/codec/legacy
negotiation case, four control-policy cases and four Swift keyboard cases.
Independent review checked the actual callback, context, refusal and ABI paths,
including preservation of the preceding wheel reset. These tests remain
unexecuted: the focused Cargo attempt first hit disk exhaustion; an alternate
output path began dependency compilation but produced no test result before
the execution environment became unavailable. Native iOS/host delivery is not
qualified by this slice; the broader iOS bead remains open.

### Verification update for the preceding Windows slices

All 31 tests in the rebased production Windows portable modules passed under
nightly-2026-08-31 with rustc --test -D warnings: geometry/mapping, original
capture, bounded key queues, four CLI cases and six fractional-wheel cases.
This standalone run does not compile cfg(windows) bindings or the native
session integration. The nine native Target and seven selector/fresh-read/
renewal tests remain unexecuted.

The exact pinned formatter successfully produced the final form of all 20 Rust
files touched in this session, and its corrections are applied here. A separate
follow-up check could not start after the environment disappeared. Full
workspace checks, strict Clippy, native builds and live platform qualification
remain pending; the existing GitHub verification workflows continue to cover
these gates after publication. No dependency versions or workflow policy changed.

## Fractional wheel input retains its original expiry (2026-11-14 UTC)

A native wheel fraction can no longer wait indefinitely and become a new scroll
when another fraction arrives later. Accumulation retains the earliest exclusive
deadline, drops expired carry, refuses oversized whole-action backlogs, and
submits through the existing original-source `push_until` path. The newest
capture's original sample preserves ordered-source timestamp monotonicity;
only its deadline can be shortened by an earlier contribution.

Pointer, button, key, text and release boundaries clear carry. Windows mapping
invalidation and replacement also clear it. Refused native submission drops the
original granted owner, so consumed fractions cannot survive for replay. Local
last-pointer state changes only after accepted pointer/button input. This builds
on the current mouse capture and mapping implementation without replacing it.

Six deterministic tests of this exact wheel-policy module passed on the pinned
nightly earlier in this session, covering combined bounds, exact expiry,
intervening actions, invalid/oversized deltas, shorter new bounds and both axes.
The rebased shared-session and native-viewer wiring received independent source
review. The complete rebased portable suite, strict Clippy and actual Windows
build/runtime remain pending because the execution service is unavailable;
`fr-znp3` remains in progress.

## Windows installed-Tailscale machine discovery and connections (2026-11-14 UTC)

The native Windows client now lists a bounded local machine snapshot with
`fr-windows hosts` and accepts a stable ID or full certificate name for `view`
and `directory`. It selects through the authenticated installed Tailscale
service, retains the original ID/name tuple, and revalidates fresh metadata
before dialing and during renewal. Strict TLS and host admission remain the
remote acceptance boundary; discovery explicitly reports readiness as unprobed.

Automatic IPv6/IPv4 selection is available, with explicit `--ipv4` and `--ipv6`
choices. Optional `--address` arguments constrain the complete current address
set; partial, extra, duplicate and changed sets refuse instead of overriding
metadata. The same constraint is retained across fresh resolution and renewal.
iOS keeps its named route and unchanged C ABI. Usage and limits are documented
in [the Windows client guide](docs/windows-client.md).

Twenty regressions were added: nine Target cases, four portable CLI cases,
and seven selector/fresh-read/renewal cases. Independent source review checked
the real APIs, platform gates and FFI migration. Pinned formatting passed for
the five selector files. Integrated compilation, tests and strict Clippy remain
pending after executor timeouts; no native Windows or live-tailnet qualification
is claimed. The broader desktop-shell bead remains open.

## Windows mouse events retain their original capture and mapping (2026-11-14 UTC)

Windows mouse motion, buttons and wheel events now retain the same original
source identity and exclusive age bound as captured keys. Capture precedes
native coordinate conversion and draining earlier keys; GetMessageTime queue
age shortens that bound instead of stamping delayed input as fresh. The token
also retains the granted picture layout. Stale ordered transitions fence the
original control owner so its held-state cleanup runs.

A resize or window move immediately invalidates the local coordinate mapping.
Only successful presentation installs its replacement, and native messages from
before that presentation (including its ambiguous clock tick) cannot use it.
The viewer redraws the retained picture on a static desktop with its original
frame ID. Failed native resize now closes even a view-only session with a
PresentationFailed cause; authenticated host reports and cleanup/connection
failures retain precedence.

Six additional portable regressions cover capture-age consumption and source
replacement, resize mappings and drag release, and presentation-failure
completion. The no-grant API test also covers generic native capture. Independent
source review checked the real layout/source APIs and completion precedence;
execution, pinned formatting and strict Clippy of this slice remain pending
because the executor became unavailable. Existing push workflows cover the
portable tests and cross-compile the actual Windows viewer. No native runtime
qualification is claimed; `fr-znp3` remains in progress.

## Windows native input prepares before its final deadline check (2026-11-14 UTC)

Absolute mouse movement previously enumerated monitors, entered a native DPI
scope and normalized coordinates inside submission, after the input owner's
final authorization check. A blocked query could therefore consume the original
ticket lifetime before SendInput. The Windows sink now builds a single-use
PreparedMove during preparation. Submission consumes that value and calls
SendInput directly; cancellation, refusal and cleanup discard it without replay.

Three new production-owner regressions cover single-use/cancelled preparation,
click and scroll at just-before/exactly-at/after the original deadline, and
revocation during preparation with an earlier held key still released by
cleanup. Independent source review checked the actual InputSession API and
deadline semantics. The initial focused compile found a test-only must-use
result, which was corrected; its final rerun and native Clippy were interrupted
by loss of the execution environment. These new tests and native System wiring
remain unverified by execution here. Existing hosted checks cover the portable
sink and native fr-winsys library; full Windows host wiring still needs its
own feature-enabled build. The qualification bead remains open (`fr-6kx2`).

## Windows capture refuses obsolete desktop pixels (2026-11-14 UTC)

After Desktop Duplication access loss, a recreated interface no longer makes
retained pixels from the previous interface current. The capture owner requires
a complete image from the current interface and successful frame release before
encoding or reporting an unchanged desktop. Startup timeouts, pointer-only
updates, failed copies and failed releases cannot manufacture that evidence;
forced IDRs and cursor updates cannot bypass it. Access loss gets one immediate,
nonblocking retry on the same selected output, then an unavailable result ends
the original media owner through its existing cleanup path.

Six deterministic tests compile the exact production freshness policy and cover
startup, healthy static capture, recreation, release/copy failure and repeated
loss. All six passed, as did strict standalone Clippy and pinned formatting,
before the execution environment became unavailable. The baseline portable
fr-core/fr-wire/fr-media/fr-client suites also passed 1,755 tests. Native Windows
compilation and lock/UAC/display transition qualification remain pending; this
is source-policy evidence, not new hardware qualification (`fr-4f9m`).

## Serial reconnects and cleanup-confirmed host recovery (2026-11-13 UTC)

The capacity-one listener now rebinds its exact protected address set after
actual original transport destruction and before the existing cooldown. Bounded
kernel UDP input can retain a reconnect's Initial during that wait. Fresh IDs,
TLS, installed identity, policy and application admission still start only after
cooldown. Cancellation and ingress loss close the prebound sockets. This removes
the deliberate cooldown gap, not the interval while the old transport remains
retained or the separate multi-client limitation (`fr-w4yu`).

A serial `RetainedTransport` timeout still fences and ends its share. Persistent
`frd run` can now start a fresh share if the later original source, launch, audio,
ingress and approval cleanup all succeed. It uses the existing bounded host-fault
budget and backoff; `--once` preserves the original typed failure. A still-retained
transport, failed cleanup, or other listener fault cannot use this recovery
(`fr-kyxt`). See [persistent hosting](LINUX_SERIAL_HOSTING.md).

Four new serial namespace regressions cover immediate IPv4/IPv6 reconnects,
no early admission, cancellation and ingress loss during cooldown. Four host
namespace regressions exercise a deliberately retained real `HostSession`,
actual cleanup, fresh negotiated identities, and refusal on retained transport
or rule-removal failure. Two ordinary unit cases cover one-shot reporting and
the shared failure budget. These compose actual owners and UDP/TLS with the
existing explicit identity/interface/firewall fixtures, not physical GPU or
installed-tailnet qualification. Both namespace suites now run as independent
lanes of the existing Rust verification workflow.

At publication, exact pinned formatting, whitespace checks and independent
source reviews pass. UBS static-only scans of the five changed Rust files report
zero critical findings; its Cargo phases and AST rule pack were not evaluated.
Local `cargo check -p frd -p fr-transport --all-targets --locked` stopped with
SIGKILL in the unchanged pinned Asupersync compiler before checking these crates.
The local namespace cannot open netlink sockets. Compiler, Clippy and runtime
verdicts therefore remain pending hosted execution; neither bead is closed on
source evidence alone.

## Verification checkpoint for optional-channel expiry (2026-11-12)

Validation used `21393211` (the bead-only update after `8838a919`) plus the
exact output of the pinned `nightly-2026-08-31` formatter. The remaining four
Rust-file corrections are layout only. Workspace `cargo fmt --all --check`
and `git diff --check` pass.

The changed-source UBS scan completed with exit 0 across 25 Rust files:
zero critical findings, 763 warnings and 107 informational findings.
Warnings on changed lines were test assertions and fixture mutex checks.
The scan did not run Cargo phases in its targeted shadow workspace, and its
AST rule-pack mode was unavailable; this is limited static evidence.

The restored executor could not build the latest transport test binary.
`cargo test -p fr-transport --test media_attachment clipboard --locked`
exited 101 when the unchanged pinned Asupersync compiler process received
SIGKILL. A retry with one compiler frontend/backend/linker job and 256
codegen units also received SIGKILL, at 7,680,948 KiB maximum child RSS.
The all-workspace/all-target/all-feature metadata-only `cargo check --locked`
likewise exited 101 with SIGKILL, at 7,697,744 KiB maximum child RSS.
Both measured peaks are about 7.3 GiB. Kernel OOM counters were unavailable,
so the kill and memory measurements are evidence; its kernel cause is not.
These local attempts executed no selected clipboard tests. Latest strict Clippy and full
workspace runtime verification remain blocked by this dependency build.

The completed hosted results below retain their original revision scope.
At 10:52 UTC the newer hosted Rust, formatting and Observation jobs were
still queued or pending. The temporary formatter runner switch was restored
after both runner queues remained unassigned and local formatting became
available; the final series has no workflow-policy change. The non-Rust size
failure and live/loaded/platform acceptance requirements remain open.

## Clipboard send expiry preserves the original desktop (2026-11-12)

Completed Clipboard pairs now share the settled native-I/O send-expiry boundary
with Files and FilesDownload. The original stream pair retires with a distinct
`SendExpired` cause and reset/stop code `0x4652_4353`; control, input, media and
independent file lanes remain owned. Native queued/retransmittable stream work
and application accounting are withdrawn, and consumed identities stay retired.
Critical expiry, incomplete exchanges, authority loss and expiry inside an
active native operation retain their existing connection fences.

The bridge fences its original worker and preserves already produced native
publication receipts. Its item deadlines, Egress checks, privacy switches and
`permits_io` guard remain unchanged. Native send-by still derives from the
original item's permit; this change does not manufacture a shorter normal
bridge deadline or extend an expired item. Whichever original terminal check
wins first retains its actual cause. The native CLI reports
`clipboard_send_expired` when the original transport tombstone proves it.

Prepared-send refusal handling also preserves a retained send's terminal cause
when either per-record callback crosses the old deadline. A refused new record
cannot conceal the expired original lane or reacquire queue ownership.

**Added and independently source-reviewed, not executed:** six UDP/TLS transport
regressions cover queued and staged sends in both directions, sibling traffic,
first/final prepared-refusal races, consumed identities, and terminal/admission
negatives. Two bridge tests cover four direction/staging combinations with the
real bridge, worker and Egress code plus an explicitly recording native fixture.
They retain an earlier uncollected publication receipt, check the untouched
original item cap, fence subsequent native activity and preserve input/control.
These fixtures do not establish real OS clipboard or live-tailnet acceptance.

The
[focused workflow on 4812fb8](https://github.com/Dicklesworthstone/franken_remote/actions/runs/43196003850)
compiled the actual daemon library test binary, including the new clipboard
bridge tests, checked the daemon executable, and passed 206 selected test
executions with zero failures. Some filters overlap; this is an execution count,
not a count of distinct tests. Its selected tests include real X11 indicator
and microphone controls, but **do not execute the new clipboard expiry tests**.
Their runtime verdict and the latest full workspace gates remain pending.
The shared-clipboard and loaded-session beads remain open.

## File send expiry preserves the original desktop (2026-11-12)

Completed Files and FilesDownload attachments now retire independently when a
retained reliable send expires between settled native I/O operations. The native
reset removes only that pair's unsent and retransmission work, clears its
retained accounting and framing, and keeps consumed identities as tombstones.
The peer receives a distinct send-expiry cause. Critical input, video and the
other file direction retain their original owners. Expiry during held native
I/O, unsettled protected output and unfinished attachment failures retain the
connection fence.

File owners preserve actual publication receipts and source/disk cleanup.
A completion request admitted without a publication proof remains an unknown
effect; no timeout replays a selection or starts its successor. Native upload
and download reports distinguish this cause from receive expiry and cancellation.

**Executed on Linux:** the
[full Linux job on 8ee5be4](https://github.com/Dicklesworthstone/franken_remote/actions/runs/43194283238/job/131101107036)
passed all 10 file-expiry regression executions: six transport cases, three
real ATP/disk scenarios and the original ControlledViewer/input continuation
test. Nine tests were newly added; one existing expiry test was updated.
They cover queued and staged sends in both directions, sibling continuity,
consumed identities, terminal negatives, partial staging cleanup, earlier batch
receipts and publication uncertainty. All-target/all-feature compilation and
every native integration exercise lane in that job also passed.

The full run was not green. Strict Clippy found the additional reservation
boolean; `e4f980a` replaces the mutually exclusive flags with a typed
`Option<Expiry>` without suppressing the lint. The formatter returned exact
source-layout corrections. One existing file-expectation regression reported
`Cancelled` instead of `Expired`. Two concrete cause-loss paths are now fixed:
receive preflight retains its exact failure before returning, and a passed file
setup deadline is rechecked through its original owner before session cleanup.
Earlier input, session and critical-response failures keep their priority.
Two actual-clock regressions require the original expiry before and after
cleanup; the existing withheld-offer test keeps its deadlines and assertion and
now includes the concrete viewer error in diagnostics. The original failure log
did not expose that error, so it does not identify which path caused that run.
These new negotiation regressions remain unexecuted.

The existing non-Rust glue count also exceeds its 20,000-line allowance; the
same failure predates this series. Final pinned formatting passes locally.
The typed-cause fix and full workspace runtime verdict still require execution
beyond the dependency build barrier recorded above. No live-tailnet, hardware
or load-at-least-40 acceptance is claimed; `fr-jlgm` remains open.

## Clipboard publication boundary integration (2026-11-12)

The controlling-viewer suite now has a thirteenth regression that stages real
clipboard records under an outstanding input receipt obligation. It verifies OS
publication immediately before that obligation expires, with the original
500,000-microsecond cap, and refusal plus prepared-state cleanup at exact expiry
while the view and lease are still live. This new regression is source-reviewed
but has not been executed.

The [focused Observation indicator run on 15cb1cc](https://github.com/Dicklesworthstone/franken_remote/actions/runs/43147800628)
passed **206 selected test executions with zero failures**. This includes the
updated software/VideoToolbox rate preflight test, host approval and policy,
shared publishing, Linux native input responsiveness, and real X11 indicator and
microphone controls. The run also compiled the actual frd library test binary,
including the six new native clipboard callback regressions, and checked the
frd executable. Some filtered suites overlap; 206 is an execution count, not a
count of distinct tests.

That workflow does not select the new client clipboard publication suite or
the six native clipboard callback tests. Those remain unexecuted, as does the
real Mac adaptive-rate worker regression. The earlier 44 shared/core clipboard
tests and the five Rust plus 25 portable C capture-policy checks are separate
passing evidence recorded below. Separately, the [Linux verification job on a7a5778](https://github.com/Dicklesworthstone/franken_remote/actions/runs/43148535884/job/130952835434)
passed the all-target, all-feature compiler check and strict Clippy. Its tests
and the Mac jobs are still running at this checkpoint. Complete multi-platform
verification and live VideoToolbox qualification remain outstanding. The two
native-test wrapping corrections from the pinned GitHub formatter are included
without semantic changes.

## Native clipboard callbacks after policy waits (2026-11-12)

The native clipboard access guard now retains the original authorization cap
before a host policy wait, rechecks cancellation, and samples the runtime clock
again after the wait. That fresh sample must precede the retained authority
cap and the caller's original item deadline. The subsequent readiness check is
nonblocking and refusal-only; a newer policy deadline cannot revive the old
operation. Switch generations and original transport lifetime are still checked
before returning permission.

Controller callbacks use their independently owned nonblocking clock cursor,
including a final original-owner stop check. They cannot borrow another
consumer's sample or renew the item's deadline. Metadata reads still require
current authority, temporary view suspension remains resumable, and original
owner stop fences native access even if the transport object still exists.
Native submission/unknown-effect receipts and cleanup paths are unchanged.

**Added and independently reviewed, not executed:** six regressions using the
real runtime virtual clock, a channel-delayed original authority read, and the
accepted-controller fixture. They exercise authority versus item expiry,
just-before/exact boundaries, policy renewal during a wait, revocation, runtime
cancellation, independent cursors, same-owner resume and original-owner drop.
The execution environment remains unavailable, so these new runtime tests and
the full current workspace have not been qualified locally. Exact formatter
patches from the client slice's GitHub job are included; earlier core and
portable Mac policy test results remain the separate evidence recorded below.

## Viewer clipboard publication caps and nonblocking native clocks (2026-11-12)

The controlling viewer now bounds clipboard publication by the current visible
view, outstanding input receipt/control-response obligations and original lease
proof. The item lifetime remains a separate lease-bound cap, allowing an
incomplete transfer to survive a temporary view suspension. Core retains the
publication cap across its final sample, even when a newer view becomes ready.
The final readiness hook also catches receiver-epoch loss and owner progress
that overtook that sample without authorizing a replacement deadline.

Native clock callbacks and their failure fallback no longer wait on cursor or
policy mutexes after sampling local time. Contention fences only clipboard;
the existing input maintenance retires that optional owner while preserving the
original input grant. A narrow nonblocking native transport sampler exposes the
same behavior to the original native worker. Temporary suspension stays
resumable; immutable item expiry and consumed-publication replay rules remain.

**Added and independently source-reviewed, not executed:** twelve regressions
using actual accepted grants, wire records, clock models and receiver epochs,
plus both real mutex contention paths. Positive cases retain submitted and
unknown-effect receipts, same-owner resume, and an incomplete transfer's
original lease cap. Contention cases require the original input owner to accept
a key afterward. The executor outage prevented compiling or running this new
client suite. The preceding shared/core 44-test result is separate evidence.
GitHub source formatting passed for dc0cc77, including the shared clipboard and
Mac changes; full runtime/native qualification is still pending. The native
callback's host-side clock-after-policy check follows in its own implementation
slice rather than being inferred from this viewer projection change.

## Native Mac adaptive bitrate (2026-11-12)

The existing adaptive-capture option now drives VideoToolbox rate transactions
for both shared and exclusive native publishers. The original Mac capture owner
retains a pending target bounded by its configured ceiling and the software
policy's two-second host/worker-clock dwell. A new target does not create video
work on an unchanged source. Required capture work applies and reads back the
same VideoToolbox session's soft AverageBitRate, requests an independent frame,
and reports a changed setting only after the existing strict HEVC guard admits
the actual IDR under the unchanged decoder configuration. Failure ends that
capture owner. Ordinary nonadaptive startup keeps its previous contract.
Readback confirms a configured target, not measured output throughput.

**Passed component checks:** all five production Rust rate-policy tests on the
exact nightly, with strict Clippy including pedantic lints, and all 25 production
portable C capture-policy tests with -Wall -Wextra -Werror. The checks cover idle
deferral, exact dual-clock dwell, pending-target changes, native receipts and
all combinations of conditional capture, source verification and forced IDRs.
The owned Rust files were formatted before the executor became unavailable.

**Added, not executed:** a real Mac worker protocol regression for 8 Mbps
bootstrap, a 4 Mbps transition with independently admitted IDR, a deferred
2 Mbps request, and 8 Mbps restoration after both original clocks pass the
dwell, followed by orderly Stop. Apple SDK compilation, real VideoToolbox runs,
full-workspace checks and impaired-path measurements remain unqualified here;
the existing GitHub verification lanes run after direct main publication.
The source-format runner's five clipboard-test wrapping corrections are also
included exactly as emitted by the pinned formatter; no test semantics changed.

## Clipboard publication after policy waits (2026-11-12)

The shared clipboard owner now retains the original item and readiness cap,
then samples the clock after authorization has released its policy locks.
Before native publication it checks that immutable exclusive deadline, original
owner revocation and both privacy-switch generations. A final nonblocking
refusal check can tighten the decision after view loss; it cannot renew the
item, grant authority or wait for the policy mutex. The host check preserves
the original lease and native-owner identity. Temporary view loss remains
resumable, while contention refuses the clipboard lane without revoking input.
Preparation cleanup and submitted/unknown-effect receipts remain intact;
consumed commits are never replayed.

**Passed:** the exact pinned Cargo clipboard suite (30 tests) and suspension
suite (14 tests), including ten new publication regressions using the actual
shared authority mutex and deterministic thread handoffs. Cases cover exact
expiry, the still-valid positive path, item expiry, clock regression, revoke,
privacy-switch cycles, immutable caps after newer evidence, retained native
receipts, final view loss and lock contention.

**Validation limits:** strict core Clippy identified nine test-only empty-vector
assertion style denials. Those assertions were corrected, but the executor
became unavailable before a rerun. Full-workspace checks and native OS/session
qualification have not passed on this revision. The companion viewer clock
projection/readiness completion is tracked separately under the existing
clipboard work; this shared/host slice does not claim that completion.

## Input expiry after authority-lock contention (2026-11-11)

The shared `InputSession` now resamples its original host clock after policy
authorization has released every authority lock. It compares that sample with
the exact exclusive deadline returned by authorization, without acquiring
another policy lock or renewing the action. Input that expires while waiting
for policy access is refused before any native submission or speculative held
press. Clock regression fences the owner; revocation during the final sample is
also checked. Partial receipts, duplicate suppression and release-only cleanup
remain intact.

Six new deterministic cases hold the actual shared authority mutex on another
thread while advancing the clock. They cover exact-deadline refusal, a
still-valid wait, a partial repeat without replay, held-key cleanup, clock
regression and final revocation. Five regressions fail on the preceding source;
the still-valid positive control passes there.

**Passed:** the pinned Cargo suite for `fr-core`, `fr-wire`, `fr-media` and
`fr-client`: 1,689 tests, zero failures; all-target compiler checks for those
four crates and strict Clippy also passed. The focused preparation suite was
rerun after test-only lint corrections: 17 passed. Workspace formatting, docs
links and whitespace checks passed. UBS reported four pre-existing
identifier-comparison false positives and test-only warnings; review found no
new production issue, but this limited scan is not a native qualification.

**Blocked/not tested:** the full all-feature workspace check was killed while
compiling the unchanged pinned Asupersync dependency. The subsequent full test
attempt was interrupted under resource pressure before tests ran. Portable
policy tests do not qualify native OS timing or a complete remote session.
Full-workspace and native qualification remain outstanding under `fr-fr2q`.

## macOS input checks after native event preparation (2026-11-11)

The live Quartz input owner now constructs and retains each complete keyboard,
pointer, button and scroll event before its final original-deadline/revoke check.
It revalidates the selected native target after construction, then posts the
retained event without another pointer query, allocation or property mutation.
A stalled constructor can no longer spend an already-checked input lifetime.
Text event properties are prepared before that boundary as well.

Failed pointer queries refuse instead of substituting an origin coordinate.
Cancelled preparation preserves click counts and the last submitted position;
confirmed refusals preserve the original held set, while uncertain submissions
retain their release obligations. Cleanup remains release-only and reports
incomplete native release when required pointer evidence is unavailable.

**Passed:** 25 pinned-toolchain tests of the actual Rust owner and its policy
modules against an explicitly deterministic FFI test boundary, plus strict
Clippy. Ten new cases cover ordering, expiry, native target change, failure,
coordinates, click state and cleanup. Three primary regressions were also run
against the unchanged preceding source and failed there, including a stalled
constructor incorrectly returning `Submitted` after its deadline.

**Blocked/not tested:** the unchanged pinned Asupersync dependency was killed
with SIGKILL during full-workspace metadata compilation. These component checks
do not compile the full application or validate the Objective-C boundary.
Apple SDK compilation, real Quartz event tests and a live Mac session remain
required under `fr-udyh`; this is not a native capability qualification.
See [the Mac host input boundary](docs/macos-host.md#input-at-the-final-authority-boundary).

## Native Mac file selections on the original upload (2026-11-10)

The macOS client now accepts `--control --file-drop` and provides a local
**Send copied files** button plus copy-only file-URL drops onto the picture.
Copying files in Finder before taking control provides an explicit same-window
selection workflow while preserving the existing focus-loss stop. Drops require
the original viewer to remain focused, visible and eligible. Initial `--send`,
native selections and `--file-commands` share one original upload intake;
`--receive` remains an independent direction.

The AppKit boundary imports URL metadata only after an explicit local gesture,
with at most eight items and 32 KiB. It refuses file promises and multi-URL
items; the existing parser also refuses nonlocal URLs, malformed paths and
duplicates. The existing selection worker pins descriptors off the window
thread, preserving the gesture's original two-second lifetime before and after
filesystem work and at final admission. Native cancellation and admission retain
the actual transfer's cancellation handle under one lock, so a stop cannot miss
a just-admitted source. Progress and final receipts retain every selected batch.
Cleanup waits for the original worker; reconnect does not replay selections or
renew their consent. The toolbar and `--fit` checks reserve the button's space.

**Passed source/component checks:** pinned workspace formatting, whitespace
checks, all nine production sizing tests (including the new toolbar case), and
all seven shared file-URL parser/inbox tests. UBS reported no critical findings;
its new warnings are test assertion/lock heuristics, not new production warnings.
The standalone component tests use the real source modules without substitute
types. They do not compile the entire native client or exercise AppKit.

**Blocked/not tested:** full runtime compilation and tests remain constrained
by the unchanged pinned Asupersync build and execution-environment resources.
Apple SDK compilation, actual Finder/pasteboard delivery and a real Mac upload,
cancel and reconnect session remain unqualified. These remaining checks are
tracked in `fr-macos-file-selection-h9nw`; no native capability pass is claimed.

## Optional audio receive expiry preserves the desktop (2026-11-10)

The original completed audio-down and microphone-up attachments now retire
independently when their first-byte receive framing deadline expires. Both a
partial record and a complete record held by a blocked consumer keep their
original deadline. Retirement retains the original binding and stream identities
as terminal tombstones, reports the receive-expiry cause to the peer, and returns
receive credit without reopening the attachment. Input, video and the other
audio direction remain independently usable.

The host playback owner withdraws source demand and fences its exclusive ring;
the viewer drops its playback owner and pending acknowledgment. Their checks
cover expiry before service, during native output, and inside final transport
admission. Microphone owners report a distinct `ReceiveExpired` result only
when the original live connection proves that its completed optional pair
retired. An unfinished attachment, unexpected transport failure, retained-send
expiry, or microphone output still held by native transport retains its existing
connection-closing behavior. Retirement cannot grant new capture consent or
replay speech.

Regression coverage is added at the real UDP/TLS transport boundary and at the
host/viewer owners, including both audio directions, control and reply streams,
partial and blocked records, original-connection checks, surviving desktop
traffic, native-output fences and retained microphone output. These are source
tests, not a claim of physical microphone or speaker qualification.

**Verification at the initial checkpoint:** source review completed; runtime
test execution **blocked**. The pinned Asupersync dependency was killed during
code generation before the focused audio tests could compile. An all-target,
all-feature metadata check was then interrupted after dependency compilation
made the execution environment unavailable. Neither attempt is a passing gate.
The toolchain, dependency pin, features and assertions are unchanged. Full
runtime, native-device and two-machine qualification remain outstanding under
`fr-audio-receive-expiry-8t7n`.

## Original native Send deadlines and stop-before-cleanup (2026-11-10)

The Mac composer's explicit Send now freezes the original controlled owner's
checked client-clock deadline, in addition to the original native event time.
That bound survives native handoff, local queue contention, compositor gaps and
an encoded send waiting on transport. Later renewal cannot extend old text.
An expired or stale-context completion refuses without disabling otherwise
valid physical control; it never acquires a replacement context or retries.

Each opening uses a fresh native editor with an immutable opening context.
Callbacks from a cancelled or retired editor cannot enter a later draft, even
within the same input view. One window can attach only its original input
producer. Local text selection is handled one event at a time rather than
entering a native mouse-tracking loop that postpones the owner pump.

Close, emergency revoke, focus loss, native sleep/session loss and overflow
retire text before native cleanup. Rust prioritizes terminal batch evidence and
fences its original input and streaming owners before acknowledging AppKit/IME
cleanup. Ordinary availability updates cannot stand in for that acknowledgment.
The existing Change Display path still parks its original producer for orderly
close, and a valid locally refused Send still requires a fresh explicit click.

After execution access recovered, **16 exact-source queue/context regressions**
and **8 native text-policy regressions passed** under `nightly-2026-08-31`.
Both limited diagnostics passed strict Clippy with real production dependencies;
they do not compile or exercise the full session runtime or native AppKit FFI.
The Mac CI lane now checks Objective-C syntax against the real Apple SDK before
the broader Cargo build and explicitly runs text and receipt policy tests.
Full runtime compilation remains blocked locally by the unchanged Asupersync
compiler memory failure. Apple SDK compilation, native IME/lifecycle behavior
and Mac-to-Mac text delivery remain unqualified; `fr-c2u1` stays in progress.

## Native Mac Unicode composer and retained action effects (2026-11-10)

The controlling Mac viewer now offers optional `native-input-text` and requests
Text in its default control profile. A granted session exposes a bounded local
`NSTextView` editor with explicit Edit Text, Send and Cancel. AppKit owns real
local preedit/candidates; the complete value is handed to the existing host
Unicode path only on a fresh Send click. The editor accepts typed/input-method
text, with local paste/file import and rich attachments explicitly unavailable.
Physical control remains the default outside the editor; composer keys and
their remaining releases stay local, and emergency revoke stays active.

The producer captures the original source/view continuity token before native
editing. Actual suspension, generation changes and native lifecycle loss clear
old drafts and pending Sends; ordinary compositor gaps only disable sending.
One bounded pending value retains its original native timestamp and receives a
synchronous admission acknowledgement before another native pump. Neither
refusal nor loss retries it. The composer reserves its window space before
viewport mapping, including matching CLI fit preflight.

Completion preserves confirmed action-operation prefixes, partial/unknown
outcomes, zero-submission refusals and unavailable retained history on both
success and failure. Pointer reports are separately labeled as possibly repeated
and never increase confirmed action effects. A queued local commit does not
claim OS submission, application delivery or rollback.

Tracking: `fr-c2u1` remains in progress. The coherent source and regressions have
independent source review. The initial publication occurred during an execution
environment disconnect; the subsequent portable checks and remaining native
qualification limits are recorded above. The earlier indicator component checks
recorded below do not validate this composer or the full runtime.

## Original text context survives only uninterrupted input (2026-11-10)

Queued Unicode commits and local composition contexts now carry the original
source identity, full input view and a checked continuity revision. A real
suspension invalidates them even when the same view resumes between owner
publications. The dispatch owner retains that revision outside the producer
queue lock, so a contended publication cannot revive old text. Ordinary
compositor candidate gaps only hold dispatch and disable Send; they preserve a
valid draft and its original capture-time bound. Context refusals admit no wire
identity and leave physical input usable.

Eight boundary regressions and five view-generation regressions cover this
behavior. With the three subsequent queue-deadline cases, all 16 passed in the
exact-source queue/context diagnostic after execution access recovered. Its
strict Clippy check also passed. The two full-runtime deadline regressions and
native Mac integration remain separate qualification work.

Before the disconnect, the published indicator implementation at `731cf819`
was recovered and checked: 18 supervisor tests and 17 native policy/role/pipe/clock
tests passed, as did narrow strict Clippy, an Apple-target Rust Clippy check
against the installed target standard library, and the real Linux minimal-feature
helper check. Six formatter-only updates and one equivalent notification
match-arm consolidation preserve those verified source bytes here. No Apple SDK
link, AppKit execution or complete application build is claimed; the unchanged
Asupersync compiler memory limit remains separate.

## Mac Stop control accessibility and visibility (2026-11-10)

The required indicator added in `731cf819` now exposes its Stop action to
accessibility clients and requires completed drawing of each retained original
badge, with its expected content view and visible bounds. Native terminal state
is an atomic first-cause latch: callbacks during a visibility query cannot be
overwritten by a late Shown result. A full bounded event turn with a remaining
backlog ends readiness before a queued Stop can be deferred indefinitely.

The signed installer builds the helper separately with only
`macos-observation-indicator` and no default features; it does not pull the
input, clipboard, viewer or broker/runtime features into that artifact.
The existing Mac verification lane now builds and links that standalone
configuration before broader native checks. Two pre-existing session-monitor
test assertions were made compatible with the pinned strict Clippy lane;
their empty-output requirements remain unchanged.

These follow-up changes were independently source reviewed. The execution
environment is unavailable, so their final format/build/test and actual
AppKit/accessibility behavior are not claimed as verified. The existing
`fr-hjl0` qualification task stays open. The earlier queued-text view fence
`3b78949` is also on main; native text composition remains separate work.

## Required macOS host Stop sharing surface (2026-11-10)

The macOS CLI now requires a separate read-only `fr-observation-indicator`
before the original host starts listening. It covers view-only and control-capable
hosting, including idle waiting between peers. The signed installer builds and
installs it, and an absolute `--observation-indicator` override survives
LaunchAgent installation. The controlling session keeps its existing per-lease
input and clipboard indicators.

The native helper opens a **Screen sharing enabled — click to stop** panel on
every original display, bounded by the eight-display catalog. AppKit stays on
the main thread while an independent nonblocking IPC/watchdog worker enforces
the original opening and visibility deadlines. Each readiness request requires
a completed native visibility/event turn that began after that request arrived.
Cached UI evidence, a resumed machine, a late native return or a new display
cannot extend or revive the old owner.

An attributed local Stop ends the original host and drains its existing owners,
including if clicked during startup. It produces a successful cancellation so
launchd's failure-only restart does not undo that action. Native failure remains
a refusal. The supervisor preserves a final local-revoke datagram across command
EOF, while malformed framing remains a protocol fault. Uncertain child cleanup
retains its original capacity and cannot silently start replacement work.

Verification before the executor outage: **18 focused supervisor tests and strict
Clippy passed**; **17 native evidence-policy, role, pipe and clock tests passed**.
These include real subprocess/channel behavior but do not prove a native UI.
The files were subsequently recovered from recorded sources and patches, and
independently reviewed. Their final formatting/full-build pass is unverified.
The final Apple Silicon Rust Clippy rerun was interrupted; Objective-C SDK
compilation, native linking and AppKit/physical-input execution remain
**not tested**. The full host-runtime build remains blocked locally by the
unchanged pinned Asupersync compiler memory failure.

This does not qualify graphical screen-lock detection, supply macOS local
approval, or grant observation/input authority through the indicator itself.
The desktop user and OS remain trusted. Tracking `fr-hjl0` stays in progress
for the remaining native and full-runtime gates; `fr-6y4w` retains the separate
screen-lock gap. See [observation indication](docs/observation-indicator.md).

## Bounded committed Unicode input (2026-11-10)

The macOS per-lease input executor now prepares and submits complete Unicode
scalars, including supplementary UTF-16 pairs, under the original ticket and
view authority. Native event construction/readback must succeed before the
optional `native-input-text` capability is offered. Each lease repeats that
probe, and a failed text probe leaves physical keyboard control available.
Text refuses any remotely held key or button; expiry and uncertain submission
preserve the submitted prefix and never replay the remaining content.

The Linux foreground robot owner exposes this through `fr robot input
--text-file PATH`, using the existing connection, lease, request and observation
handles. One regular, non-symlink UTF-8 file supplies 1..4096 bytes without
normalization, clipboard fallback or text in process arguments. The original
owner retains the bounded request only while needed, clears local request
buffers on drop, and keeps only a fingerprint in its receipt history. The wire
extension is optional and independently checked before host approval, so
legacy physical-control peers continue to negotiate normally.

Validation: 312 `fr-wire` tests and 1,121 `fr-client`/`fr-media` tests passed
after the extension. A limited source diagnostic passed 154 tests for the CLI
parser, original-owner ledger, scalar policy, file handling, action lowering
and the host's selected-capability boundary. Its unavailable viewer-control
type cannot be constructed; it does not exercise runtime or IPC behavior.
The actual Mac Rust input modules also passed source type checks and strict
Clippy without replacement FFI implementations, native linking or execution.
Native event-tap and expired-agent regressions were added but need an actual
Mac. Full application compilation remains locally blocked by the unchanged
Asupersync compiler memory failure. Native-window IME capture and application
acceptance of Quartz Unicode events remain unqualified; a receipt describes
submission, not proof that the application displayed the text.
See [committed Unicode input](docs/macos-host.md#committed-unicode-input).
Tracking: `fr-h80k` (native execution qualification remains open).

## Session startup and display-sleep fences (2026-11-10)

The macOS library entry point now requires the same-user native session
monitor, matching the CLI. A terminal monitor result interrupts policy and
certificate startup work before that work can progress. The host rechecks the
original evidence after ingress qualification and after binding; a share that
ended during a wait retires its ingress before reporting Listening.

The native monitor also requires an online, awake main display and consumes
screen-sleep notifications. A sleeping display refuses startup, and a later
wake cannot resume the original owner. This extends display-power coverage;
the graphical screen-lock qualification gap described below remains open.

Validation: the updated native evidence policy passed all 11 source tests and
strict Clippy. The added runtime startup regression is present but remains
unexecuted locally because compiling the unchanged Asupersync dependency is
killed by the executor's memory limit. Apple SDK compilation and physical
display sleep/wake behavior still require native qualification.
Tracking: `fr-rc2-lock-logout-revokes-wmrt` (broader acceptance remains open).

## Native session lifecycle supervision

The shared session monitor now validates executable images without blocking on
FIFOs, and its host startup wait wakes independently of native progress. Status,
the original evidence deadline and the last clock sample share one serialization
point. Negative replies, expiry, clock regression and poisoned state become
terminal before the process supervisor resumes; later responses cannot revive
the original owner. This first batch is commit `7e68729` (`fr-m16y`).

The macOS host now requires a read-only native session helper before listening.
The signed installer builds and installs it, and an absolute `--session-monitor`
override survives LaunchAgent installation. The helper binds the real inherited
Security session, UID and console set, observes public console/login state, and
consumes switch-out, sleep, logout/power-off and protected-data loss as terminal
signals. An independent pipe thread can report Opening and expire old evidence
while AppKit initializes or stalls. Positive native reads keep their original
500 ms deadline on the sleep-inclusive clock. The existing host supervisor
revokes the original control/clipboard/file owners and stops capture when that
evidence ends. See [macOS lifecycle scope](docs/macos-host.md#console-session-lifecycle).

This does **not** qualify graphical screen-lock detection. Public console state
and protected-data availability are not an unlocked-screen guarantee. The CLI
reports `console_and_sleep_monitored` and states the limit. The implementation
bead is `fr-uvji`; the unresolved lock authority boundary is `fr-6y4w`.

Validation: 1,672 original-workspace tests passed across `fr-core`, `fr-wire`,
`fr-media` and `fr-client`. The first commit's exact staged monitor sources passed
18 focused evidence/process tests, including real subprocess/pipe faults and a
FIFO; the final platform-aware sources passed 22 with the added protocol cases.
Ten native lifecycle policy tests and four original responder tests passed.
Strict component Clippy/type checks also passed for the actual
`aarch64-apple-darwin` Rust branches, including the native helper binary and the
checked package launch path, using the exact pinned nightly. These focused
checks do not constitute a full `frd` or native Mac build. The unchanged pinned
Asupersync compiler was killed by SIGKILL on three bounded local attempts
(last observed peak RSS about 2.2 GiB), so full application gates and two
runtime-dependent monitor tests remain unexecuted locally. Apple SDK compilation,
physical switching/logout/sleep and live tailnet qualification remain open.
The retained [source validation record](results/session-lifecycle-source-validation.json)
names the exact source hashes, tested scopes and excluded gates.

## macOS host text clipboard and privacy receipts (2026-11-10)

The macOS host now connects default-on controlling clipboard negotiation to a
real per-lease `fr-input-agent --clipboard` role. The existing client pasteboard
backend is shared through `macos-clipboard`; it remains thread-confined, bounded
to complete UTF-8 and separate from the broker. Prepare/publish consumes the
original native bytes once, retains the local revision and origin fence, and
rounds the original monotonic deadline down rather than renewing it.

A follow-up timing correction applies to both Mac client and host. Privacy
clock sampling now occurs under the heartbeat mutex, preventing preempted
readers from misclassifying a newer UI pulse as clock rollback. The heartbeat
also retains Darwin's sleep-inclusive monotonic sample: the 250 ms privacy cap
expires across system sleep even when Rust's uptime clock pauses. A later UI
turn cannot renew expired evidence; clock failure/regression remains terminal.
The protocol's original item deadline and its clock domain are unchanged.
Deterministic injected-clock regressions cover serialization and these cases;
this is not a claim that the host's separate lock/logout gap is solved.

The child keeps its own visible Off panel on the main thread and native work on
one worker. A separate watchdog closes the original parent's content-free privacy
signal on Off, parent loss or a stale UI heartbeat, without waiting for native
clipboard work. Read shutdown fences future commands while the write side can
return an already attempted publication receipt. The broker poisons the owner
before exposing such a receipt; text and later operations stay blocked.
A publication stopped before its first possible IPC write is definitely not
submitted. A lost reply after a possible write remains uncertain, without retry.

`--input-agent` enables clipboard by default; explicit On/Off and their
LaunchAgent persistence use the real run parser. View-only stays excluded.
The existing bounded dispatcher is shared with Linux, preserving its final
X11 deadline/privacy checks. A test-module path error observed in first-batch CI
was corrected with an explicit nested path.

Validation: the source and regression additions received independent review.
The workspace execution service became unavailable before compilation, so no
new native or full-suite pass is claimed. Added regressions cover private
pasteboard publication/local supersession, socket dispatch, exact receipt
retention, command shutdown with receipt writes, parent loss, original deadline
projection and installed default/On/Off choices. The Mac-host workflow explicitly
runs the helper and broker lifecycle tests. Native Mac host/client copy/paste,
the physical Off surface, stalled native provider behavior and all-three-host-OS
acceptance remain open. Tracking: `fr-p2-shared-clipboard-wgd`, `fr-5a4w`.

## Native media scheduling and macOS repaint isolation (2026-11-09)

An admitted in-process presenter now lives on one Asupersync drained blocking
task. Configuration, decode/present, cursor updates, idle repaint and configured
presenter destruction run there. A single bounded command slot and one reply
preserve operation sequencing and retained-byte checks. The session continues
checking its original deadline, cancellation and live authority while waiting;
dropped or expired waits poison the owner and discard late responses. Reaping
retains the exact native task and reports pending until its destructor actually
returns, including after cancellation. A finished channel is not cleanup proof.

The macOS UI reads and publishes plain geometry/counters under a separate short
lock. Metal drawables, textures and commands stay on the media side. Resize and
expose work is coalesced and serviced during idle media turns, so a static
desktop still repaints. Changed geometry cannot enable control until a real
retained picture is submitted at that revision. Redraws do not count as new
frames or fresh visibility. Missing Metal resources refuse the operation instead
of acknowledging a black or unsubmitted frame. Local escape is handled before
geometry/readiness checks, even while repaint is blocked.

Eleven real local-worker regressions cover sequencing, validation, deadlines,
live-authority loss, cancellation and blocked configuration, decode, cursor,
refresh and destruction. They are source additions, not a passed test claim:
the pinned Asupersync build was killed by SIGKILL before application tests could
execute. Native macOS compilation and Metal/event execution remain pending.
AppKit creation and final window/renderer release still run on the main thread;
they remain native trust boundaries rather than hard-bounded OS calls.
Tracking: `fr-5a4w`, `fr-55k1`; broader macOS qualification remains open.

## macOS reconnect consent and local session control (2026-11-08)

The native macOS client consumes its initial control request once per invocation.
Automatic retries reopen viewing; a new, focused and fresh F8 press/release is
required before another control request. Original native event timestamps fence
stale gestures, and Ctrl+Alt+Shift+Escape stops the original session before any
ordinary input from the same batch is sent. Focus loss, incomplete event batches
and geometry changes fence input before dispatch. Host terminal reports survive
cleanup; a window close cannot turn failed cleanup or a host refusal into success.
Completion separately retains host cleanup/effect reports and local cleanup proof.

The macOS connection now binds its original authenticated local owner. `fr robot
session status`, `fr disconnect` and instance-qualified `fr robot session close`
reach that invocation without waiting for a new Tailscale lookup. A private
pathname endpoint and lifetime file lock serialize duplicate startup and stale
socket reclamation; same-user kernel peer credentials and exact instance/target
checks remain required. Optional automation and quality data remain unavailable.

`--fit` now bounds the macOS window in AppKit points, preserves aspect ratio and
never upscales its natural size. Input still maps the actual drawable picture.
The public command details and remaining unsupported options are in
[native client commands](docs/native-client-cli.md#macos-desktop-client).

Portable production policy regressions cover request consumption, fresh/expired
gestures, focus loss and emergency stop; sizing tests preserve Retina logical
sizes and bound extreme dimensions. All 12 policy/sizing tests and strict
component Clippy pass, as do 240 core tests, formatting and documentation checks.
Real pathname-owner, typed-completion and Session lifecycle regressions are
included but have not executed here: the
unchanged pinned Asupersync compiler was killed by SIGKILL even with one job,
debug/incremental output disabled and reduced compiler parallelism. Full
workspace gates remain unverified. Native AppKit event delivery and a live
macOS reconnect/close sequence still require Mac execution. The separate macOS
client verification job enables `macos-desktop`, which the existing default-feature
macOS workspace job did not compile. Tracking: `fr-5a4w`, `fr-55k1`; neither
broader platform bead is closed by source or portable evidence.

## macOS outbound identity (2026-11-08; live on the installed app)

`fr-tailnet` provides the ordinary private `PeerTarget`, `NativeClient`,
`TargetOwner` and `TargetLease` path on macOS. It reads the standalone
Tailscale app's loopback `LocalAPI` with the root-published port and
admin-readable token, as Tailscale's own CLI does; `fr hosts` uses it for
discovery. Resolving a destination reads status, numeric WhoIs and status
under one original deadline. All three must come from the same daemon run and
agree on node identity. Exact address ownership, key expiry, cancellation,
clock rollback and system-sleep ceilings are enforced. No public JSON
constructor can create a dialable target, and Linux's daemon UID/PID checks are
unchanged.

The owner chose this transport on 2026-11-08 over a signed CLI child per read
(`fr-tailnet-platform`, since removed). That CLI connects to the same loopback
port with the same token, so its signature check authenticated only our own
client binary, at 60-80 ms of CPU per read and three reads per renewal. A whole
`fr inspect` (three reads, process start included) now takes about 12 ms of
CPU. Neither transport can check which process accepted the connection: if
Tailscale is not listening, a local process that binds the port would receive
the token and could answer with false metadata. The macOS host (`frd run`
with `macos-host`) reads viewer admission and its certificate through the
same endpoint. Destination
identity does not confer observation or input authority or establish
desktop/codec readiness.

Live on the owner's Mac (macOS 27, Tailscale 1.104.1, `io.tailscale.ipn.macsys`):
`fr hosts`, `fr inspect` in both builds (`identity_checked`), and desktop
sessions to ts2. macOS tests cover the endpoint reader. Portable tests cover the
WhoIs/status binding, malformed and routed responses, and lookup/target expiry. See
[the identity and lifetime boundary](docs/native-discovery.md#macos-installed-app-identity-and-outbound-connections).
Tracking: `fr-1yxy`; the broader macOS client bead remains open.

## macOS open-source `tailscaled` socket (2026-11-09; live reads, host run blocked)

Without the standalone app, macOS reads the open-source daemon's root Unix
socket `/var/run/tailscaled.socket` (or `--socket PATH` for `frd run` and
`fr`), through the same transport as Linux. The kernel must report the server
as root (`getpeereid`). Its `LOCAL_PEERPID` is the daemon identity all reads
of one lookup must share. The selection order and what is trusted are in
[native discovery](docs/native-discovery.md#macos-installed-app-identity-and-outbound-connections).
macOS now runs the shared Linux socket tests (admission, node, certificate)
and a socket suite. That suite includes a test-user server the production
client refuses before writing a request, and the kernel-reported PID.

Live on 2026-11-09 on an M4 Mac mini (macOS 26.5, Homebrew `tailscaled` 1.102.1
as a root LaunchDaemon, no app), from a test binary built on another Mac:
status, own-address WhoIs, `installed()` discovery and status/WhoIs/status
resolution chose the socket by themselves; the certificate pair for the
node's `ts.net` name was fetched and verified against the system roots. Every
read reported the daemon's launchd PID. `frd run --socket
/var/run/tailscaled.socket` there stopped earlier, at the encoder's capture
probe: Screen Recording is not granted under ssh (TCC preflight attributed to
`sshd-keygen-wrapper`), and that probe precedes the host's `LocalAPI` and
certificate stage. So `frd`'s own host stage and viewer admission over the
socket are not yet live-tested.

## macOS desktop client (2026-11-08; live, partly qualified)

With `--features macos-desktop`, `fr connect` runs natively on macOS. The host
stream is decoded by VideoToolbox into IOSurfaces and drawn by Metal into a
`CAMetalLayer`, the remote cursor is a blended Metal quad, and AppKit input
reaches `frd`'s input path. `frd`'s `Worker` presents through an in-process
`LocalPresenter` rather than a child process. The tailnet lookups use the
loopback `LocalAPI` backend described above (status/WhoIs/status). Options
not implemented here are
refused by name ([details](docs/native-client-cli.md#macos-desktop-client)).

Live on 2026-11-08 from the owner's Mac (macOS 27, Tailscale 1.104.1) to ts2
(Linux, NVENC): the window showed ts2's live desktop with control granted,
and typed keys reached ts2. A build using the signed CLI for lookups also showed
the live desktop. With the current loopback lookups, sessions opened and ran for
about a minute until ts2, under heavy memory pressure, ended them; no frame was
captured from that build. Not yet confirmed live: pointer, wheel and click
delivery, and the window-close outcome.

## macOS installed-app discovery (2026-11-08)

The public `fr hosts [--json]` command has a macOS implementation through the
installed Tailscale app's loopback `LocalAPI` (above). The snapshot-only
`fr peers` alias shares that implementation and the Linux discovery path. One
original one-second deadline covers the read and projection. Status is capped
at 1 MiB and uses the same parser, candidate exclusions and output format as
Linux. A timeout, cancellation or invalid result cannot become an empty
successful snapshot.

This was the first platform slice for `fr-5a4w`; the desktop client above
builds on it. Capability probes and hosting still return `platform_unavailable`
on macOS. A custom `--socket` reads only that root `tailscaled` socket (below;
until 2026-11-09 it returned `localapi_endpoint_unsupported`). Discovery
returns no target lease, observation grant or input authority, and Linux's root
UID/PID checks remain intact.

Added regressions cover shared status projection, exclusions, malformed and
oversized JSON, cancellation/deadline/clock refusals, public command routing and
an actual macOS suspended-child signature refusal/reap. Existing test setup was
consolidated without removing any of the 141 inventoried test functions.
Targeted pinned formatting and the test-body preservation audit passed before
the local execution environment disconnected. The last complete source-budget check passed before
that loss at 499,923 Rust lines and 17,948 glue lines. Source was recovered from
the recorded edits and immutable GitHub originals; fresh formatting, full Cargo
checks/tests, the macOS native regression and installed-Tailscale execution
remain pending. The recovered blobs also total 499,923 Rust lines and 17,948 glue lines under
the unchanged counting rules. The interrupted parser check did not produce
passing evidence. Since then (2026-11-08) the workspace builds, lints and
tests on macOS 27, the signature regression passed there, and `fr hosts` read
a real `io.tailscale.ipn.macsys` installation.
The broader macOS bead remains open.

See [the command and trust boundary](docs/native-client-cli.md#macos-discovery).

## Supervised native process launch (2026-11-08)

Capture startup, monitor discovery, decoder startup and audio startup now submit
process creation through the pinned Asupersync drained blocking pool. A slow
fork/exec no longer runs inside the poll that services session renewal. One
original absolute work deadline covers queueing, process creation and the
following configuration exchange; this change does not extend startup ceilings.
The same original authority and cancellation context are checked before
submission, when the pool claims the launch, after process creation, and before
returning its result. Role and configuration refusals still precede launch.

Cleanup distinguishes a launch still owned by the pool from a positively
unstarted process or a caller-owned worker. Cancellation before the pool claims a queued launch prevents that launch
from creating a child. A child returned after cancellation or
expiry is killed and transferred to its original retirement owner; cleanup waits
within its own unchanged deadline for that handoff and the actual OS exit
receipt. A missing child handle is never treated as proof of exit.

New regressions hold the actual bounded pool, exercise queued expiry,
cancellation and abandonment, retain a real late child through cleanup, and
service observation renewals while a launch waits past the first sampled
authority expiry. Existing worker scenarios share identical fixture setup and
payloads to preserve the handwritten-source limit. This is a source change for
`fr-6zrg`, not proof of the cause of the remaining live startup failures. Pinned full-workspace formatting and documentation checks pass. The fixed
source-count check passes at 499,979 Rust lines and 17,756 glue lines. Full runtime execution of this revision remains pending on a capable
runner; the local shared filesystem is full and unchanged Asupersync
compilation previously exceeded this runner's 8 GiB memory limit.

## Native session recovery and abrupt client failure (2026-11-08)

The original viewer can report an abrupt failure to its host through a bounded,
content-free protected QUIC close on the existing authenticated socket. The
connector retains destination renewal while that attempt runs and preserves
the original application result. A normal explicit closing exchange keeps sole
socket custody; queued application output is discarded, and a close write never
claims peer acknowledgement, remote input release or native cleanup. This
addresses the silent-client-end mechanism in `fr-t4lj`; live two-machine
acceptance remains pending. See [transport close](PROTOCOL_TRANSPORT_CLOSE.md).

The native reconnect adapters also fence their original observer before
returning an initial UI or setup callback failure. They preserve that failure
and retain the same native worker for the existing explicit cleanup; the next
attempt remains forbidden until cleanup completes. Two TLS/bootstrap and actual
child-reaping regressions are included in the existing isolated-namespace CI
lane. Platform permissions and codec output remain labeled fixtures.

Reference recovery now fences the original controlled input owner before a
network turn can transmit the recovery request and receive the host's resets.
While an outstanding native decode or presentation operation drains, that same
owner tolerates only retirement of its exact old media routes. Old progress
cannot renew input evidence, and successor attachments remain owned by the
existing recovery handshake. The original connection, grant and absolute
recovery deadline stay in place. Unexpected media retirement outside this
explicit transition still ends the session with `InvalidRoutes`.
Unexpected application records during native draining also retain their exact
`UnclaimedRecord(kind)` cause. The same authenticated TLS regression exercises
this refusal in both healthy streaming and explicit reference recovery.

This fixes a source-identified race relevant to `fr-live-invalid-routes-45mn`:
previously the post-I/O controlled-viewer check required live old media routes
before the outer loop could detach them for recovery. It is not yet proof that
this race explains every failure in the reported live runs.

Added actual localhost TLS/UDP regressions cover protected client close
custody, cancellation, destination loss, deadlines, queued native data, and
connector supervision. Recovery regressions retain a real receiver budget
inside simulated native work across authenticated peer resets and check that
unsolicited resets still fail. Codec/native completion and authority fixtures
are explicitly simulated; these tests do not qualify hardware or a live tailnet.
Pinned formatting and the documentation gate pass locally. Full check/test
attempts, including a single-job metadata-only transport check, are blocked by
SIGKILL compiling the unchanged pinned Asupersync dependency under this runner's
shared memory limit. At client-close commit `18f6bd5`, GitHub's Linux
all-targets/all-features check and macOS all-targets check passed, along with all
13 new transport client-abort tests and both connector supervision tests. That
revision's full test run still failed one receiver-load headroom-probe assertion;
the native dial-certificate and clipboard-stall lanes repeated earlier failures.
Its strict Clippy run found one documentation-markup error, corrected in this
tree. The later recovery and callback regressions still await completed runtime
verification. Formatting at `58235bb` passed. Shared terminal and recovery
fixtures retain all scenarios within the existing handwritten-source limit.

## Native readiness, hardware selection and transfer pacing (2026-11-06)

Three public operations now reach their existing native owners:

- `fr doctor --probe-native --worker PATH --encoder software|nvenc|vaapi --x-display :N`
  performs one actual capture/HEVC IDR probe before runtime or tailnet access.
  Optional `--input-agent PATH` discovers the executor's operations without
  injecting input. Reports distinguish combined probe failures from unknown
  capture outcomes and leave decode, presentation and other untested lanes
  unqualified. Ordinary `fr doctor` retains its tailnet checks.
- `frd run --encoder auto` explicitly probes NVENC then VAAPI, at most once each,
  and retains the first admitted concrete backend for every share and restart.
  Software is never an automatic candidate. Timeouts, malformed replies,
  invalid geometry, launch/reap errors and local Stop prevent further probing
  or startup as appropriate. Installation preserves the explicit selection;
  the service probes on its own actual desktop before binding.
- `frd send --target NAME --instance ORIGINAL --pause N` and `--resume N` control
  the original host download sender through existing authenticated local V2 IPC.
  They preserve its original deadline and authority, distinguish intent from
  observed pacing, and never replay or reopen a transfer. V1 send/status/cancel
  clients remain accepted by current owners; current clients do not downgrade.

Verification is scoped: 15 host-file CLI/report/IPC component tests, seven
native-doctor parser/help tests and 11 production encoder-selection tests pass,
with strict component Clippy. Formatting and diff checks pass. Full `frd`/native
CLI Cargo verification remains blocked locally by the unchanged pinned
Asupersync compile exceeding this runner's memory; real socket/desktop tests
cannot run because AF_UNIX creation returns EPERM. Report/probe and source-owner
integration regressions are added for a capable runner. Hardware, sustained
performance and public end-to-end qualification remain pending. Beads `fr-tlef`,
`fr-jn0o` and `fr-kqow` retain those exact follow-ups.

The GitHub indicator runner exposed two microphone compilation defects at
`716add6`; direct-main correction `3490f07` adds immutable launch-configuration
cloning and completes the independent microphone flag in an approval fixture.
The isolated native microphone-process library/audio-worker check and strict
Clippy pass. Whole-graph CI for the correction and the new operations remains
pending; earlier core-only CI results do not qualify these later commits.

## Public Linux microphone slice (2026-11-06; qualification pending)

The native microphone libraries now have public application owners. On the
host, `frd run --microphone` independently enables the original controller's
AudioUp attachment, requires a responsive local Stop microphone window, and
retains the real PipeWire/Opus/UI cleanup receipts. On the client,
`fr connect --control --microphone --microphone-source NAME` (feature
`linux-microphone`) starts muted, binds a visible Talk/Mute panel to the original
viewer, and records only after a new attributed local Talk gesture and the
exact host acknowledgement. Playback audio enable is independent.

Each Talk has a distinct immutable local token, so a fast mute–retalk interval
cannot revive old capture even if a session poll misses the muted state. Local
withdrawal has its own terminal epoch cause; after real drain evidence it uses
the normal UserMute/AudioStop path, without relabeling native or authority
failures. The original selected source and microphone choice do not cross
viewing reconnects or a later F8 request. No default device is changed.

The sender/offer/IPC component tests, strict component Clippy, native UI
production build, C warnings-as-errors compilation and native integration-test
compilation pass. The full graph is blocked locally by memory exhaustion in
the unchanged pinned Asupersync dependency; AF_UNIX socket creation is denied
by this runner, preventing an Xvfb execution result. The indicator CI lane now
requires the actual native panel tests. These results do not qualify a physical
microphone, a user's desktop audio server, application audibility, a public
end-to-end session or live-tailnet microphone latency. Bead `fr-d8rs` retains
that application acceptance follow-up. See [client setup](NATIVE_MICROPHONE_CAPTURE.md)
and [host setup](NATIVE_MICROPHONE_UPLINK.md).

## Live state (2026-10-12)

**First live two-machine sessions.** Release build of this tree, no patches:
ts1 (`frd run --software-explicit`, real Tailscale certificate, real WhoIs
admission, nftables ingress, Xvfb) served trj (`fr connect`, Xvfb), both
Ubuntu 26.04 on the same tailnet.

- View-only: first dial on an idle Tailscale path in 1.7 s; client pixels within
  0.18% of the host's (10% fuzz) with the cursor overlay; tcpdump on tailscale0
  saw no datagram above 1200 bytes.
- Control: the host pointer, read by an independent X client, followed the
  controller exactly, including a click. A controller window that never gets
  keyboard focus ends `control_focus_lost` after 30 s (with and without a window
  manager).
- `fr hosts` reports direct, DERP-relayed and idle paths correctly; a host that
  never completes the handshake ends `host_not_answering`.
- Not exercised live yet: local approval, VAAPI, macOS or Windows hosts or
  clients.
- 2026-10-14, live audio (ts1 `frd run --audio`, trj `fr connect --audio`,
  debug builds at c70a82b, a private PulseAudio null sink at each end). This
  was done in a view-only session and in a controlled one. The host sink
  played 450 Hz, then 1050 Hz at a scheduled instant. The client sink's
  monitor showed the change about 0.1 s later (121 and 92 ms, across
  NTP-synced clocks). Every 20 ms block carried the tone, for 30 s before
  the switch and 2 s after it. Both runs completed `stopped` with
  `audio_active: true`. The host log counted each clean session as
  `refused: 1` / `failed: 1`. It now counts `admitted: 1`, `refused: 0`
  and `failed: 0` (fr-u0xk). Also new: a controller's window close runs its
  closing exchange, and the controller's completion reports the host's lease
  report (fr-rsru).
- 2026-10-14, live (ts2 `frd run --encoder nvenc --input-agent --clipboard
  --files`, ts1 `fr connect --control --clipboard --send`, debug builds):
  typed keys ("hello") and wheel (buttons 4 and 5) reached the host, observed
  by an independent X client; a 100,000-byte file was published with an
  identical sha256, and a second file with a taken name was refused. With
  `--clipboard` the session first ended within seconds
  (`Control(Clipboard(Transport(Unauthorized)))`, fixed in 47c0c60), then no
  text crossed (xclip owners have no selection timestamp; fixed in 41b1440).
  Now text crosses both ways, including the host's pre-existing clipboard at
  session start (`clipboard_received` 2). NVENC teardown takes 0.6-8.3 s on
  that host; a slow one used to leak the ingress rule until frd refused to
  start (`ResidualRuleLimit`), fixed in cf97a6b (fr-kiao keeps the open
  semantics question). The client on trj (load 100-250)
  ended `Media(Worker(Deadline))`, the overload fragility in fr-tqu7, so the
  client ran on the idle ts1.
- 2026-10-14, later, the same live check at ed45e10 (after the day's renewal,
  clipboard-projection and visibility-gap changes) passed again: keys, wheel,
  clipboard both ways, file send with a matching sha256, orderly `stopped`.
- 2026-10-14, evening, live at 12b8a18 (debug builds). ts1 (`frd run --encoder
  software`) hosted and trj controlled. The full check passed: keys, wheel,
  the host's pre-session copy and both later copies (`clipboard_received` 2,
  `clipboard_ended` null), a 100 KB file with a matching sha256, `stopped`.
  The same check against ts2 (NVENC) under load 55-190 from unrelated jobs
  ended twice ~20 s in. The host reported `Ingress(Closed)` and the client
  `transport_deadline_expired`; a third run ended `Ingress(Identity(Timeout))`
  before admission. One ingress revalidation over its 1 s bound ends the share
  (fr-ingress-revalidation-load-qelg). The run now reports that cause instead of
  `Closed`.
- Clipboard over delay (namespace, netem, xclip). A failed item with records
  still in flight used to close the whole session: a 40 KB copy at 120 ms RTT,
  `native_session_failed`, 3 of 3. Since 145df7c it retires only the
  clipboard. Item ceilings (release builds): 1 MB on a LAN; at 30 ms RTT
  500 KB (1 MB fails); at 120 ms RTT 100 KB (200 KB fails). Before 41c3233
  the cap was the host's input view window (130-390 ms at Begin), about 60 KB
  and 20 KB. It is now the lease's authorization deadline (~2.7 s). Bulk data
  still moves ~8 KiB per RTT beside video (fr-clipboard-wan-item-ceiling-6mlb,
  owner decision).
- 2026-10-15, release builds, `scripts/live/two_node.sh` with `AUDIO=0`: ts2
  (`frd run --encoder software`) hosted and the idle ts1 controlled, over a
  direct tailnet path. Every row passed twice: session open, view pixels,
  keys, wheel, clipboard both ways (with the host's pre-session copy), a
  100 KB file with a matching sha256, the controller's closing exchange (host
  lease report `session_ended`, cleanup `fenced`) and `stopped`. A host
  colour change reached the client's pixel in 102-167 ms. Audio was not
  tested: ts2 has no sound server. Before 68663b7 the closing exchange failed
  3 of 3 with a file lane open: the input fence cancelled the session's
  context, the file lane's stream reset then refused it, and the failed
  retirement closed the connection (`transport: Closed`). Earlier release
  runs with the busy trj as client ended `video_worker_late` or
  `video_reference_lost` (fr-tqu7).
- 2026-10-15 later, same pair and script. libx265 now encodes each picture on
  four wavefront threads (ecce132; one picture, so no added picture delay). A
  host colour change reached the client's pixel in 49-75 ms over two runs.
  Alternating runs of the previous build took 108-174 ms. With
  `ENCODER=nvenc` (NVENC now has the same 200 ms VBV, fe8fb70) every row
  passed and the change took 32-59 ms.

**What it took** (each with unit tests and a planted negative):
- Asupersync pinned to git `ee72b665e` (0.6.0). Published 0.5.0 sends each
  handshake CRYPTO segment as one datagram and cannot send a real
  four-certificate Let's Encrypt chain (`udp_send`). The namespace fixture now
  serves such a chain.
- A 1200-byte datagram policy on both ends (tailscale0 MTU 1280).
- Peer identity that ignores Tailscale path fields (`CurAddr`, `Relay`,
  `Active`); path changes had ended sessions with `identity_changed`.
- A bounded wait for the viewer window's focus, and named codes for every
  capture stop.

**NVENC** (GTX 1070): `hevc_nvenc` never opened. The fixes are `refs` left at
the encoder default, encoder SEI (types 39/40) stripped on the host, and SPS
short-term RPS tables and VUI HRD admitted under the same rules. Live, an
NVENC-encoded desktop from ts2 was admitted, decoded and presented. A 5-minute
NVENC session (ts2 to ts1) stayed within 0.13-0.19% of host pixels with an
active encoder session. `frd run --encoder nvenc|vaapi` probes the encoder at
startup and refuses `encoder_unavailable` before binding (15d4c21). VAAPI on
VAAPI hardware is untested.

**Client presenter.** A fixed 200 ms limit per native decode/present request
killed a healthy first 1080p decode on a busy client; each picture's own budget
now binds (ff14b5a). The historical 48-200 ms measurements on the loaded client
predate the current MIT-SHM presentation path, recycled BGRA buffers and
reactor-driven pipe reads. Those implementations already exist; the loaded
two-machine acceptance in `fr-live-client-present-cost-tqu7` remains open.

The Linux worker now prepares exactly four HEVC slice threads from checked
geometry, applies the unchanged seccomp filter to every thread with TSYNC, and
only then admits and parses the remote decoder record. Configuration produces
no picture. This adds no frame threading or picture queue and changes no
presentation deadline. Failed preparation/configuration is terminal; the
original single-thread constructor remains available inside confinement. See
[WORKER_SANDBOX.md](WORKER_SANDBOX.md#hevc-slice-threads).

**Native decoder measurements, 2026-10-30.** The actual production bridge,
installed FFmpeg 6.1.1 (`libavcodec` 60), and software x265 encoder generated 65
1920x1080 pictures per fixture at 30 fps/8 Mbit/s. After five warmups, 60 paired
decode-plus-BGRA measurements alternated single-thread/four-slice order on the
same encoded packets. Destination poisoning and canary checks were outside the
timed region; every output pixel matched between paths.

| Fixture | Single-thread p50 | Single-thread p95 | Four-slice p50 | Four-slice p95 |
|---|---:|---:|---:|---:|
| Moving desktop | 10.207 ms | 16.329 ms | 5.243 ms | 9.996 ms |
| High-noise frames | 10.795 ms | 14.471 ms | 7.083 ms | 11.403 ms |

These are local native elapsed times before confinement, not transport, X11
presentation, scanout or hardware measurements. Each fixture subsequently
passed untimed decoding inside the real unchanged filter, including mixed
reference-only outputs and the original constructor. All four prepared thread
indices refused file opens, sockets and `clone3` with `EPERM`. Small 192x128 and
cropped 1366x768 rows passed the same checks. Reproduce the native harness with
[`scripts/test_native_decoder.sh`](scripts/test_native_decoder.sh).

**Cropped-frame correctness.** Those tests exposed an existing FFmpeg 6.1 fast
conversion path that left six visible columns unwritten at width 1366 with a
tightly packed BGRA destination. That revision selected the supported general
converter with `SWS_ACCURATE_RND` for widths not divisible by eight, without
another frame buffer or copy. The November 6 correction below tightens this
rule to widths not divisible by sixteen. Different poisoned output
buffers, opaque-alpha checks and leading/trailing canaries verify the whole
visible image. Restoring only the old conversion flag makes the cropped
regression fail on the previously unwritten alpha bytes.

The actual native library, media-worker binary and reference-decoder tests
also compiled in a same-source `linux-media` workspace: all seven decoder tests
and strict Clippy passed, including the three new prepared-decoder tests.
All three real Xvfb worker integration tests subsequently passed against the
same `e67bdfc` production source: native and fitted pixel readback across
reference-only history, cursor recomposition from the last presented picture,
and malformed decode-only input refusal with worker termination and unchanged
visible pixels. All ten `reference_decode` test bodies have therefore run
successfully. These tests use the actual confined worker and a bounded private-
pipe test driver; they do not exercise a complete `frd` authority supervisor or
a tailnet session. The isolated manifest excludes unrelated unavailable runtime
dependencies; this is not full-workspace qualification. The five-minute
loaded-client acceptance and current QUIC idle-traffic defect (`fr-r6qp`) remain
outstanding.

**Native buffer safety, 2026-11-06 (UTC).** The original
`95d010e` production bridge overwrote the trailing guard of an exact 1368x768
BGRA buffer in the real FFmpeg 6.1.1 harness. Its fast-converter wrapper rounds
width to eight pixels, while the SSSE3 kernel writes sixteen-pixel groups.
Selecting accurate general conversion for every width not divisible by sixteen
fixes this overrun and preserves the existing 1366-wide tail correction. It
adds no output buffer or pixel copy. The encoder also unconditionally ends its
swscale frame operation before returning the borrowed BGRA source; a partial
frame-start failure can otherwise leave a retained reference to caller memory.
See [the native boundary details](WORKER_SANDBOX.md#hevc-slice-threads).

The final production patch passed `scripts/test_native_decoder.sh` against the
real bridge, FFmpeg 6.1.1 and software x265: all eight geometry/content cases,
including 1368x768, exact pixel comparisons, opaque alpha and leading/trailing
buffer guards. Configuration and allocation-limit refusal cases also passed.
Inside the unchanged sandbox, the child completed metadata-only configuration,
reference-only decoding and the legacy constructor, with `EPERM` on all four
slice-thread indices. This is native CPU and confinement evidence, not a
tailnet session or visible-latency acceptance.

The actual `fr-media-worker` binary also built and passed strict scoped Clippy
(`-D warnings`) with the pinned toolchain, `--locked --offline`,
`--no-default-features --features linux-media`. Pinned workspace formatting,
docs and whitespace checks passed. Full-workspace tests were not completed in
this session: earlier builds of unchanged Asupersync exceeded the shared 8 GiB
memory limit, and UBS is unavailable. No dependency or lockfile was changed.

**Parallel conversion experiment rejected.** A separate swscale thread pool
increased moving-desktop conversion cost. Reusing the existing HEVC workers
instead passed pixel, bounds and confinement checks, but did not establish an
improvement in complete decode-plus-conversion time. In the final experiment,
both paths used four HEVC threads and the same 65 encoded 1920x1080 pictures per
fixture; 60 measurements after five warmups alternated the paired order.

| Fixture | Scalar total p50 / p95 | Shared-worker total p50 / p95 | Conversion-only p50, scalar to shared |
|---|---:|---:|---:|
| Moving desktop | 8.126 / 11.942 ms | 8.166 / 13.174 ms | 0.873 to 0.529 ms |
| High-noise frames | 7.804 / 11.821 ms | 7.952 / 13.383 ms | 0.829 to 0.479 ms |

These are local native elapsed times on a loaded shared host, before
confinement. Timing drift also appeared in identical-scalar controls; the cause
of the total-time difference is unproven. Production retains scalar conversion,
with the verified buffer-safety corrections. No additional converter pool,
shared-worker conversion scheduler, latency improvement or completed
five-minute two-machine acceptance is claimed by this change.

**Explicit robot commands (2026-10-30):** the foreground input owner accepts
bounded modifier chords (`--chord MODIFIER,...,KEY`), with one to four USB
modifiers and a maximum of ten receipt-gated transitions. Original observation,
generation, lease and request deadlines govern the entire chord; partial or
unknown effects fence the owner without replay. The actual client ledger's 22
tests and 68 focused parser/encoder/owner-state tests passed, with strict Clippy.

One-shot `fr robot observe --native-once --screenshot PATH` exports the actual
retained, composited viewport as an exclusively created private PNG after the
original session's completion and local cleanup. The worker matches frame,
codec-configuration and recovery identities, with an independent 32 MiB pixel
ceiling; reference-only decoding cannot replace the retained presentation.
All 12 actual native decoder/worker tests, 130 media tests, four parent request
policy tests and 58 focused CLI/artifact tests passed. An independent PNG decoder
verified ten generated files and every pixel. These are component and native
worker checks: the full session regression was added but could not run because
the exact pinned Asupersync compiler process exceeded the local 8 GiB memory
limit. Full CLI linking, live screenshots/chords over a tailnet, screenshot
export from an existing foreground owner and the broader robot acceptance bead
remain unqualified or incomplete. See [the command contract](docs/native-client-cli.md).

**macOS**: prior workspace checks passed before the native discovery change.
`fr hosts`/`fr peers` now use the installed-app bridge described above; its native
build and execution remain pending. Desktop connections and all `frd` operations
still refuse with `platform_unavailable`.

**CI had never run a test.** Every Rust verification run, the
observation-indicator lane and every nightly verify lane died with "The runner
has received a shutdown signal". Compiling frd's library tests peaks at 18.9 GB
RSS, against 16 GB on the hosted runner, and `continue-on-error` reported the
killed Tests step as successful. Tests committed "pending CI execution" had
therefore never executed anywhere. A swap action (12186bf) lets every lane run.
About a dozen never-run or stale tests have since been corrected; each commit
names its cause. Since 9916648 every lane of the Rust verification run is green
(fmt, check, clippy, tests, examples and the X11/HEVC/docs/count lanes). Nine
workflows that pin historical source blobs fail by design and await an owner
decision.

**Idle host.** Measured on ts1 (release `frd run`, listening, nftables rule
installed, 60 s of process CPU time):
- Before b47a271: frd used 2.28% of a core, 3.38% with its reaped `ip`/`nft`
  children. Four 10 ms safety-net pulses woke it 100 times a second.
- After: 0.48% (1.58% with children); RSS 9.9 MB. Pulses are 250 ms while idle;
  an accepted peer keeps the 10 ms re-check.
The plan's target is under 0.1%. Most of what remains is the 500 ms ingress
refresh, which spawns `ip` and `nft`. Its cadence is an owner decision (bead
fr-idle-frd-cpu-icui).

**Regressions from the pin.** Six namespace e2e tests regressed when the pin
landed. Four (peer-finished reporting after departing inspections, sequential
viewers, a refused peer's sibling) were FrankenRemote's: a terminal desktop
polled the peer's listener once and dropped it, which only worked while 0.5.0's
timeouts elapsed instantly. That is fixed (a496ec9). The lossy impairment rows
still pass or fail run to run (bead fr-xfvs).

**Tailnet path in the namespace suite.** The runner gives the namespace
loopback MTU 1280 and drops IP fragments, as Tailscale does (6a38a0b). A
planted 1500-byte datagram limit passes on plain loopback and fails there.
Under it, on ts1, all 70 pass (3df6564, 2026-10-14), including the seven
`real_audio` tests now that ts1 has PulseAudio: the host playback tone reached
the client's monitor 104 ms after the change (54 ms at the host's monitor).
Two are new: a local stop of `frd run` now tells a connected view-only viewer
`host_stopping` within ~20 ms; it used to leave the viewer to time out with
`host_not_heard` after ~3 s. And `--observation-indicator` maps the real
"Stop sharing" window before listening; a core XTest click on it does not end
sharing, and closing it does.
At 2d216d1 it was 61 of 68, missing only those seven. One later run at
9cfb65d ended a lossy row with an unnamed `Transport(Handler)` (fr-xfvs).
Earlier, at b47a271, 58 of 67 passed: the lossy busy-screen row and the
cursor-close step (fixed in 5d25287) also failed.
That step ended a normal controller close as `host_control_failure` in about
two runs of three on Ubuntu 26.04. XFixes returned a NULL cursor image while
the cursor's owner disconnected, and the capture worker refused it as an
allocation failure.

**Controlled sessions recover a lost video reference on the same lease**
(fr-4vf5, 2026-10-13). A 68th namespace test stalls host capture for 400 ms
during continuous motion: the late encoded picture is dropped, the host sends a
recovery IDR on replaced channels, and the controller's input stays suspended
until the recovered picture is shown. It passes 10/10 on ts1, with one recovery
per run. A second recovery used to be impossible: a connection held only 7
channel pairs for its lifetime and each recovery takes 3, so a session with
clipboard lanes could not recover even once. Since 59a5037 it holds 31, and a
controlled session with clipboard recovers from two stalls in a row (namespace
e2e). The stale-view test, which hit the same path whenever its stalled
capture was an encoded picture, went from about 9 of 14 to 10/10. Three fixes
were needed: the client suspends on the host's failure notice (1c0a989), its
input lane follows the replaced configuration channel (302f321), and the host
authority lets an evidence-backed view restart its evidence under a held
lease. Lossy network rows are a separate limit (fr-xfvs).

**Lossy paths** (fr-xfvs, 2026-10-13). On a path with 1-5% loss the pinned
QUIC stack's congestion window sits at its two-packet floor, so a picture can
miss its 250 ms send horizon. The shared view-only publisher used to end the
viewer then, and with a sole viewer the whole share. It now fences that viewer,
sends one Failed notice and recovers it in place when the viewer asks (379364c).
The delivery allowance for reliable records is now seven probe timeouts instead
of three (fe2edbc), since one tail loss alone costs about three on this stack.
The host's terminal close code is now named on the client (c078b34).
- The busy-screen view-only test passes 6 of 6 (it failed 3 of 6, before and
  after the pin).
- In the view-only impairment matrix, 16 of 17 optional rows show every
  change; before, the 120 ms lossy rows at 5 Mbit/s and the 200 ms RTT row
  ended after one or two.
- Controlled sessions on lossy 40 and 120 ms rows still end within a few
  steps, now always with a named code. A scratch client printing its final
  error showed that the commonest end was the viewer's own unsent input
  record passing its send deadline (fr-oj9u), mislabelled
  `transport_deadline_expired`. Such an end is now named by the bound that
  expired it (`view_stale`, `control_renewal_failed`), and an expired, never
  sent pointer position is dropped instead of ending control: the host
  tolerates missing pointer datagrams and every click and scroll carries its
  own position. Keys, buttons, text and held-state records still end control.
  Over the eight lossy 40/120 ms rows (64 steps) the controller held 22-32
  steps before and 29-34 after, in three runs; `control_renewal_failed` is
  now the commonest end.
- 2026-10-14 (fr-6hbo): that end was the viewer stopping itself. Every
  control grant carries a clipboard authority projection bound to the
  original media receiver, even with clipboard off. A lost reference fences
  that receiver, and the projection check ended control before the grant
  could suspend for reference recovery. A lapsed projection now ends only the
  clipboard (7856d23), and no run since has ended `control_renewal_failed`.
  Steps held did not improve measurably: the 40/120 ms lossy rows (64 steps)
  held 35/35/32 before and a mean of 34.6 over 8 runs after (range 22-40;
  three runs are too few at this variance). The client now survives the
  reference loss, but the host ends the session instead:
  - `host_session_ended`, about 3 rows per run: a host record unacknowledged
    past the 2 s allowance, or lease expiry;
  - `host_control_failure`, about 1 per run.
  `video_reference_lost` and `video_startup_expired` remain. At 120 ms RTT
  even 1% loss ends most controlled sessions within 4-6 steps (1 of 16 rows
  held). A reference recovery that misses its deadline makes the host's
  recovery services refuse I/O, which closes the connection. Raising the
  client's 2 s recovery budget to 5 s changed nothing (A/B, 16 rows each).
  The suspects are the host's own recovery deadlines and the pinned stack's
  throughput under loss (fr-xfvs).

## Audited state (2026-09-27)

**Early implementation; not an installable remote desktop.** A second reality
check (2026-09-27) re-read the plan, AGENTS.md and README against the code and
re-ran the gates; the 2026-09-24 section below is kept as history.

**What composes end to end (namespace e2e: real processes, UDP/TLS/QUIC, X11,
codecs; fixture LocalAPI, test CA, synthetic firewall).** `frd run` on Linux X11
serves the native `fr` client view-only or, with `--input-agent`, with
keyboard, button, absolute-pointer and discrete-wheel control executed by a
per-lease `fr-input-agent` child (mandatory indicator, submission-time deadline
checks, lease expiry proven by freezing the client). Opt-in text clipboard
(both directions), viewer-to-host file sending (alone or with the clipboard),
host playback audio for view-only sessions (client Opus decode in the restricted
`fr-opus-worker`), cursor forwarding with one renderer, the
LeaseRevoked/Closed/CloseRequest exchange, a root ingress helper so `frd run`
can run unprivileged, and `--logind-session`, which ends sharing and the run
with a typed cause on lock/logout/switch/suspend. Each has planted negatives.

**Fixed since 2026-09-24:** control-session deaths from IDR fragments paced four
per turn (eb8f450, bd177a8); consent surfaces accepting controller-injected
XTest events (a7b29ce and follow-ups); lock/logout not ending sharing (plan 2.2)
when `--logind-session` is given (5a8e12d); operator strings that understated
control or advertised unserved HTTPS/h3 endpoints (e55e56e).

**Update 2026-09-28 (namespace evidence; see PRESENTATION_FRESHNESS.md).**
Real `tc netem` rows now measure the network limit. Control holds on a clean
link and at 40 ms RTT, and ends at 60 ms RTT or more, or with 1% loss.
View-only holds at 40 ms RTT with 1% or 5% loss and at 100 ms RTT, and ends at
200 ms RTT. Near 60 ms RTT, control ends because of the fixed 250 ms source-age
bound and the presented-report deadlines derived from it (a 1 s experiment held
60 ms RTT). The owner decision `fr-rc2-owner-decision-view-lapse-f7ts` is pending.

Fixed today:
- critical session records starving behind queued media under loss (faae064);
- sessions that ended without a host report now name their cause:
  `view_stale`, `transport_deadline_expired` or `host_not_heard` (83781b0);
- `frd install -- <frd run options>` (514af9b);
- a typed certificate-rejection mapping (11cdf3a).

A wrong-host certificate still ends in a dial timeout: the pinned stack surfaces
the rejection only as an opaque code, and the fix is upstream. Gates:
- namespace suite 60/0;
- frd lib 742/0;
- fr-transport 165/0 (load about 110-120).

**Update 2026-09-29 (namespace evidence).**
- A view-only session no longer freezes silently under loss (64ea6b7). A
  repaired picture that completed after its display budget was decoded but
  never shown, and on a static screen nothing replaced it. Diagnostic
  repetitions at 40 ms RTT with 5% loss: 12 of 61 froze unfixed, 0 of 24 fixed.
- Control capabilities are a meet (3f927de): `frd run --input-agent` probes its
  executor at startup. It refuses typed without keys, pointer or buttons, and
  offers control without the wheel where the executor cannot scroll by lines,
  where every grant used to fail. A wheel notch now reaches the host in the
  control e2e.
- A full matrix rerun (ec91b25) has the same shape as above, with every
  shorter row ending on a named cause.

**Update 2026-09-30 (f7ts, decided under the owner's standing delegation).**
- A late ACK no longer kills the connection (0d4b59a). An admitted reliable
  record has max(send-by, admission + three probe timeouts) to be
  acknowledged, clamped to 250 ms-2 s.
- The presented source-age bound follows the measured RTT: 250 ms plus three
  RTTs at the viewer and four at the host, capped at 1 s.
  - Control at 120 ms RTT without loss now holds in some namespace rows (2 of
    8, against 0 of 2 before).
  - Diagnostics showed the main remaining limit: a stale view ended control
    instead of suspending input (plan 11.3), even at 6 ms RTT with 1% loss.
- A stale view now suspends control input instead of ending it
  (fr-suspend-o37f, PRESENTATION_READINESS.md). The host releases held input
  at the lapse, and the same lease resumes on fresh evidence.
  - Namespace A/B, control matrix at load 21-45: 3 runs with suspension
    against 2 on 33c2bd0 without it.
    - 6 ms RTT with 5% loss held in 5 of 6 rows (0 of 3 without).
    - 120 ms RTT without loss held 2 of 2 (0 of 1 without).
    - No row ended with `view_stale` (5 rows did without).
    - An untyped `native_session_failed` still aborted 1 of 3 runs (2 of 2
      without).
  - The busy-screen view-only must-hold row (40 ms RTT, 1% loss) failed 3 of
    10 runs on this build. In the same session the baseline failed 1 of 5
    (2 of 11 before). The slice's only view-only change drops an
    already-expired observation instead of ending the session, so no
    mechanism links them. That is not proof of no regression at this sample
    size.
- An expired decoder restart is named (`video_startup_expired`).
- Harness fixes:
  - the stale-view test no longer SIGSTOPs other suites' capture workers;
  - control-e2e pointer polling no longer misreads a 120 ms RTT path as lost.

**Update 2026-10-01.**
- Controlled sessions negotiate reference recovery (fr-4vf5,
  RECOVERY_HOST.md). The loss suspends input. The host then advances the
  view's recovery generation, so actions decided on older pictures are
  refused, and the same lease resumes on the recovered picture.
  - Unit-tested in fr-core and fr-client; the client test drives a real
    receive pipeline through a lost reference. Planted negatives fail.
  - In frd, only negotiation is tested for control. The host and viewer
    recovery paths are covered by the existing observation recovery
    fixtures, not a controlled one.
  - Not yet exercised end to end: no namespace row recovered a reference
    (see Open).
- Two control ends are named: `input_dispatch_stalled` and
  `control_renewal_failed` (46a5891). The control matrix now reaches all its
  rows instead of aborting on an untyped end.
- Namespace on this build: full suite 63/63; the stale-view suspend e2e
  passed 2 of 2.

**Open, in priority order:**
- Control over lossy paths. At 40 and 120 ms RTT with loss, the matrix never
  reaches the new controlled recovery. Host diagnostics, 13 of 20 lossy ends:
  the host's own sender could not finish sending a picture within the 250 ms
  reference horizon (`OriginalExpired`, a 3 KB picture once). It ends the
  session in its first serving round.
  - Likely cause: Asupersync 0.5.0's NewReno sits at its 2-datagram floor
    under loss, and a lost tail packet waits for a probe timeout.
  - The client reports it as `host_control_failure` (reason `host_failure`),
    or as `transport_deadline_expired`. Before the labels commit it read
    `host_control_revoked` / `local_revoke`, a mislabel.
  - Most traced expiries are a prepared packet that missed its send-by while
    waiting for transport admission, which closes the transport. A host wait
    for the viewer's recovery request would rescue only the others. The
    options (upstream tail-loss probing and pacing, egress handling of an
    unwritten expired packet, a bounded host wait, the 250 ms horizon cap)
    and the evidence are in bead fr-xfvs.
  - Separately, under CPU load, captured input waited behind a slow
    visibility confirmation (145-161 ms) past its 100 ms bound and ended
    control (`input_dispatch_stalled`). Since fr-1r40 an aged event is
    dropped and counted instead, and a release the host needs is still sent.
    Not yet re-run in the namespace matrix.
  - See PRESENTATION_FRESHNESS.md.
- A live two-machine run: no client on another tailnet node has ever connected
  (owner decision needed: a second machine, or an auth key for an isolated test
  node). Every row above is namespace evidence.
- Transport interoperability: the workspace pins Asupersync `=0.5.0`, which
  cannot decode QUIC frame types 0x07/0x18/0x19; the 2026-09-20 0.6.0 "GO" has no
  retained logs.
- One peer at a time: the 1 controller + 2 viewers hub, late join and `Busy`
  exist only as library code (serial listener).
- Audio during control, local approval and a host indicator for view-only and
  audio shares in `frd run`, IPv6 binding.
- Hardware HEVC (no GPU on the development host), Wayland, macOS, Windows,
  browser and mobile: not implemented.
- Size: about 312,800 handwritten Rust lines (240,000 planned maximum, 500,000
  hard stop); about 9.5k lines are unreachable (owner decision pending).

**Gates (2026-09-27):** CI rust-verification at b6aca68 had only the clippy lane
red (fixed); plain `cargo test --workspace` 2,548 passed / 4 failed (wall-clock
tests that pass alone); fr-native Linux features 463/0; namespace suite 55/1
(one X `BadWindow` at load 86).

## Audited state (2026-09-24)

**Early implementation; not an installable remote desktop.** This section
replaces the September 8 summary. The per-slice entries under "History" below
are retained verbatim as the record of what each slice claimed when it landed.
Some of them state things later found false; the corrections are listed here.
The README's "What runs today" table gives the per-command evidence level.

**What composes end to end.** `frd run --software-explicit` (composition in
`crates/frd/src/host_run.rs`, CLI in `crates/frd/src/bin/frd.rs`) binds the
tailnet address, installs the firewall ingress rule, admits peers through
Tailscale WhoIs, launches the capture worker on admission, serves software HEVC
to native viewers, renews authority and cleans up in a fixed order. This is
verified by the namespace end-to-end suite (fixture LocalAPI, test CA, synthetic
firewall; real UDP/TLS/QUIC, processes and media), including sequential viewers,
departing inspections that do not count as host failures, and silent-viewer
expiry. On a real tailnet host (root, with the owner's consent to issue its first
Tailscale certificate) it obtained the certificate, installed and verified the
`tailscale0` firewall rule, listened and stopped cleanly; no live client has
connected yet (no second machine).

**Fixed during the 2026-09-23/24 audit:**
- Admission refused every real peer because it required `MachineAuthorized`.
- Installed tailscaled's `"Peer": null` status was rejected as malformed.
- Hosted sessions died about 3 s in: the accepted QUIC socket read one datagram
  per turn, so ACK latency grew until reliable records expired (61e56f8).
- The headless Xvfb display had no X authority cookie.
- `fr` refused the distribution CA bundle (limit of 64 roots).
- CI lanes now run independently and the format gate is green again. Fixes for
  the test/examples lanes (missing `libpulse0`) and two flaky tests are pushed;
  CI confirmation is pending.

**Withdrawn as fabricated** (no evidence existed):
- `frd status` fixture output and `fr doctor` "granted/passed" rows (these now
  probe live, or say `not_tested`);
- `fr robot`/`fr disconnect` success envelopes (now typed refusals);
- GNOME/KDE/Hyprland and Windows GPU qualification rows (docs and the runtime
  tables `QUALIFIED_ROWS`/`QUALIFIED_WINDOWS_ROWS`, now empty);
- Windows/macOS worker sandboxes and Landlock claims.
The Linux client decoder's seccomp sandbox is real and tested.

**Open, in priority order:**
- A live two-machine run: a client on another tailnet node connecting to `frd run`.
- Remote control: the client needs `--control`; the host needs an input-agent
  subprocess, because `frd` cannot link the XTest sink
  (`fr-rc-client-control-cli-ykt`).
- Hardware HEVC selection in `frd run`, and the ADR 0004 software ceilings.
- Local approval in `frd run`, which needs the session-agent process.
- Wayland, macOS, Windows, browser and mobile paths (not implemented).
- Size discipline: resolved by the owner on 2026-09-24. The hard stop was raised
  to 500,000, and the clearly fake modules were deleted (2fc9711, about 15k lines).
  The count is about 264,500: over the 240,000 planned maximum, within the stop.

## History (entries before 2026-09-23, retained verbatim)

Updated September 8, 2026. **Early Rust implementation, not an installable remote desktop.** The comprehensive plan remains the design authority; this file records implementation and evidence, not additional product scope. No application or live-transport/hardware phase gate is declared complete by the tests below.

## Native-owner cancellation and returned input results

The native owner now checks parent runtime cancellation at each final native
submission and projects actual receipts into the existing `InputResult` codec
using the originating request's immutable binding. Late results, partial/unknown
text effects, receipt eviction and cancelled waits retain explicit semantics.
[INPUT_AGENT_RESULTS.md](INPUT_AGENT_RESULTS.md) records the implementation,
real XKB/XTest/Asupersync integration tests, reproduction commands and exact
local verification scope. This extends the canonical owner/watchdog rather
than adding another actor or runtime. Broader transport and application gates
remain open; the local test evidence is not a fresh remote CI result.

## Input result receipts

Sources `7b6fcdcf37d32e2cdf23e0f948fe933c1245ab66` and
`3577eb306f79839eb73251a5b41108f1540811e2` implement the host-to-viewer
`InputResult` kind (0x0048) in [fr-wire](crates/fr-wire/src/input_result.rs).
The allocation-free codec checks the attached channel/session/lease, separates
pointer and reliable-action sequence spaces, enforces negotiated record limits,
and rejects contradictory stage/outcome/count/reason combinations. Conversion
from a completed core receipt preserves the confirmed native-operation prefix
and an uncertain next operation; it never promotes submission to observation.
The fixed result carries no typed text, key identity, coordinates or ticket.
[PROTOCOL_INPUT.md](PROTOCOL_INPUT.md#inputresult-0x0048) defines its exact bytes.

Ten independently constructed byte fixtures and ten receipt tests cover the
outcomes, stages, malformed records, truncation, bindings, extensions and bounds.
The existing sixteen wire-to-submission fault tests now encode and decode their
actual core receipts. The single-byte mutation sweep is bounded adversarial
testing, not a coverage-guided fuzz campaign or independent-peer interoperability.
Those fault tests use an explicit recording sink, not live desktop input.

### Receipt revision verification

Exact source `3307d194b57819edf29f40fbd149c46eff6a1e2e` (including the receipt
changes and the separately landed watchdog) passed the following gates on
September 8. Builds ran serially on RCH worker `hz3`, Linux x86_64, with
`nightly-2026-08-31`, FFmpeg package `7:8.0.1-3ubuntu2` and Xvfb
`2:21.1.22-1ubuntu1`. Each build/test command below used
`RCH_REQUIRE_REMOTE=1 RCH_WORKER=hz3 rch exec --source-content-receipt --`.
All four returned remote exit 0 and receipts matching every one of the detached
checkout's 148 tracked files; before/after tracked-file hashes were unchanged.

| Command after the RCH prefix | Result |
|---|---|
| `cargo check --workspace --all-targets --all-features --locked -j 4` | Passed |
| `cargo clippy --workspace --all-targets --all-features --locked -j 4 -- -D warnings` | Passed |
| `cargo test --workspace --all-features --locked -j 4` | 236 passed, zero failed/ignored/filtered |
| `cargo test --workspace --all-features --locked --examples -j 4` | Examples compiled; two corpus-reader tests passed, zero failed/ignored/filtered |

These are the Rust commands from `scripts/verify.sh fast`, invoked separately
through RCH. Local `RCH_CARGO_WRAPPER_BYPASS=1 cargo fmt --all --check` and
`./scripts/verify.sh docs` also passed. The native tests exercised Xvfb and
software HEVC; this is not physical-GPU qualification, a live Tailscale session,
or independent wire interoperability. Full logs and RCH source receipts are
retained under `/tmp/fr-input-result-validation-3307d19-canonical/` on the
dispatcher, with their build IDs and receipt hashes recorded in the framing bead.

UBS scanned all five changed Rust files and **exited 1**, with 26 critical and
282 warning findings. Self-review classified the critical findings as 25 generic
binary `decode` sites misidentified as JWT handling and one existing explicit
test panic; warnings inventory test assertions/unwraps, fixture copies and
bounds-checked slices. No source suppression was added. `UBS_SKIP_RUST_BUILD=1`
disabled its duplicate local Cargo subprocesses because the explicit RCH gates
above supply those checks. The scan remains nonzero, not a clean full-lane pass;
its complete output is `/tmp/fr-input-result-ubs.txt`. Independent review has
been requested but has not returned.

An earlier mutable-checkout run returned four successful Cargo exits but changed
source during execution; it is indeterminate as a combined revision-bound gate.
It remains retained negative evidence, superseded by the detached run above.

`HeldState`, other missing message classes, the fuzz harness/campaign, and actual
session response transport remain open. Evicted receipts, obsolete pointer state
and missing process replies cannot be converted into fabricated zero-effect
results. `fr-fr-wire-framing-i0u` remains in progress with its original acceptance
criteria. The `fr-rmk` correction makes KeyTransition follow PROTOCOL.md §2's
explicit HID page/usage pair. The codec emits and checks page 0x0007, preserves
the core's admitted usage subset, and charges the complete 117-byte record.
The original implicit-page `key.hex` remains unchanged as rejection evidence;
`key_page_usage.hex` specifies the normative bytes. The
[draft compatibility rationale](PROTOCOL_INPUT.md#keytransition-draft-compatibility-correction-fr-rmk)
keeps application version 0 and rejects the nonconforming legacy layout without
adding a compatibility decoder. This repair does not complete the framing bead
or establish native input or live wire qualification.

## Earlier input framing, final submission, and native X11 effects

This subsection retains the verification and integration limits of its named
earlier revisions. Later XKB keyboard work, the `08c6f72` independent authority
monitor and the `3307d19` Asupersync watchdog are separate changes; their presence
does not establish a qualified input-agent process or host/client lifecycle.

The September 8 input implementation adds the missing path from bounded action
records to final authority checks and a real platform sink, without enabling an
unqualified network service. [PROTOCOL_INPUT.md](PROTOCOL_INPUT.md) specifies the
byte layout and [NATIVE_INPUT.md](NATIVE_INPUT.md) documents the native boundary.

| Source commit | Implemented behavior |
|---|---|
| `d9f8ea7bc3a2154637f998f5395cf30d091b24e5` | Seven allocation-free FRD0 input codecs: keys, buttons, absolute pointer state, cumulative relative checkpoints, scroll, committed UTF-8 and mode changes. Full session/lease/ticket/view bindings; authenticated direction/channel checks; independent exact byte fixtures. |
| `3b8f7e9742191677979a3729c88afcdc30834a30` | One lease-owned final submission path joins the authority and replay ledger. Each native call checks the actual clock after platform preflight. Pointer barriers, mode-ticket fencing, scalar-bounded text, confirmed-prefix/unknown-effect receipts and release-only cleanup are implemented. |
| `eca3253c0a6c2810b75d0c5233c277d58d6efa49` | Opt-in Linux X11/XTest pointer/button adapter. Actual wire records drive native motion/drag/release through the same submission owner, with X11 state queries verifying effects. Other native input capabilities explicitly refuse. |
| `8f501c0691b20e3b8b7da436af5c0ccb614e1479` | Fix native HEVC allocation admission to account for FFmpeg's aligned rows without loosening the Rust coded/crop/profile/DPB guard. The crop regression retains its original assertions and includes additional awkward dimensions. |

This builds on the existing supervised native-media and HEVC-guard work already
present at `812f35c398f35ae1ba78d9ccddace278e9180195`; those earlier changes are not
attributed to the input implementation. Historical evidence below remains scoped
to its named source revision, not the current workspace. Concurrent compound-repeat
and reversible-preparation cleanup support at `10b51d7` is preserved; it is not
included in the verification counts below.

### Input verification and remaining integration

The pinned nightly passed 157 core/wire/media tests, including seven new input
codec tests and 16 byte-codec-to-submission fault tests, plus strict Clippy for
those crates, formatting and documentation checks. Four live Xvfb input tests
and one local-display-selector unit test also passed by compiling the exact
first-party sources directly with the pinned compiler. Native-library Cargo
Clippy and test Clippy with the workspace's existing lint settings passed.
The direct native tests are real X11 API effects with synthetic local authority
grants, not Tailscale admission or physical-device qualification.

The `8f501c0` native sources additionally passed all nine native unit tests
(including the display-selector test above) against matching Debian FFmpeg
7.1.5 headers/runtime, plus the 12-frame X11 capture -> HEVC -> wire delivery ->
decode -> presentation/readback example. The codec crop test exercises five
awkward geometries, including 1366x768. A fresh negative-control build changing
only the C bridge back to its pre-fix `eca3253` source failed at 1366x768 frame 0
with `Allocation`; the otherwise identical fixed build passed. These direct
pinned-compiler executions used the actual first-party source and native APIs,
not a substitute codec or runtime. They are not a full Cargo-workspace test run.
The C compiler reports existing misleading-indentation warnings in the compact
bridge; they were retained, not hidden or relabeled as a warning-free C build.

A local full Cargo build was killed while compiling Asupersync under the 4-GiB
execution limit, including a serialized-backend retry and the later all-target
metadata-only check. Full-workspace Clippy could not run after that check failed.
The full GitHub native run
on `3b8f7e9` ([run 34274407094](https://github.com/Dicklesworthstone/franken_remote/actions/runs/34274407094))
passed formatting, compilation and strict Clippy before exposing the cropped-frame
allocation failure on FFmpeg 6.1. The `8f501c0` full native-workspace rerun
([run 34276085651](https://github.com/Dicklesworthstone/franken_remote/actions/runs/34276085651))
completed successfully on Ubuntu 24.04 x86_64 with FFmpeg 6.1.1-3ubuntu5
and the pinned `nightly-2026-08-31`. `./scripts/verify.sh fast` passed formatting,
workspace all-target/all-feature compilation, strict Clippy (`-D warnings`),
workspace tests/doctests and example tests, with zero failed or ignored tests.
`./scripts/verify.sh docs` also passed. The cropped-frame regression now passes
on both tested FFmpeg versions. This complete Cargo-workspace result validates
exact source `8f501c0`, not later concurrent source changes. The earlier failed
run remains negative evidence rather than being relabeled as passing.

`InputSession` does not start a watchdog. The interactive agent must independently
service lease expiry and local revoke, connect focus/lock/suspend/display lifecycle,
and arbitrate the single global controller before this becomes an unattended
control path. The X11 connection belongs in that input process, never the broker
or media worker. Xlib can block or terminate; entered native calls cannot be
rolled back and failed-process key release remains uncertain. Keyboard mapping,
repeat ownership, committed text, relative motion and scroll remain unsupported
in this native adapter even though their bounded wire/core paths exist.

The next critical join is qualified Tailscale identity/transport plus the real
input-agent watchdog and client lifecycle. HeldState encoding, session response
transport and native keyboard/text qualification also remain open. InputResult
encoding is now implemented as described above. No installable desktop,
live QUIC qualification, hardware latency result or Phase 1 completion is claimed.
The existing beads `fr-fr-wire-framing-i0u`, `fr-p1-input-pipeline-ay1`,
`fr-p1-session-agent-iq3` and `fr-p2-host-linux-4e4` are only partially implemented.
Their original acceptance criteria remain in force. Those earlier input passes
did not close or reassign beads because `br` was unavailable in that environment;
the receipt pass above claimed the framing bead without closing it.

## Encoded-media path now implemented

The source at `b2a2888e84601b9cb79be7fd248c54cc9aced67d` adds a complete bounded **encoded picture -> binary records -> fragment repair -> reference-ordered picture** path. It includes the new `fr-wire` crate and `fr-media::delivery::{SendCache, ReceivePipeline, MediaBudget}`. No runtime, foreign codec or external serialization dependency was added. The objects execute production packetization/reassembly policy; they are not mock codec implementations.

Sender ownership, reliable IDR startup, immutable progress announcements, out-of-order fragments, duplicate/conflict handling, bounded repair requests, reference retention, failed-decode fencing and generation replacement are implemented. Decoder-held compressed buffers continue consuming their reservation after leaving the receive queue. A repair of an old reference can unblock recent pictures without making the old picture fresh for display. Entire loss of the final picture is detectable from the reliable progress announcement.

See [PROTOCOL_MEDIA.md](PROTOCOL_MEDIA.md) for executable record layouts, [MEDIA_DELIVERY.md](MEDIA_DELIVERY.md) for integration and the reproduction command, and [CHANGELOG.md](CHANGELOG.md) for incremental commits. The rest of the session wire protocol, transport scheduling/credits, host identity adapters and native codec/capture workers remain incomplete.

### Earlier encoded-media source verification

Source `b2a2888e84601b9cb79be7fd248c54cc9aced67d` also passed [GitHub Rust verification run 34237484950](https://github.com/Dicklesworthstone/franken_remote/actions/runs/34237484950). On Linux x86_64 with the pinned `nightly-2026-08-31`, `./scripts/verify.sh fast` passed formatting, all-target/all-feature compilation, strict Clippy with `-D warnings`, normal tests/doctests and example tests. `./scripts/verify.sh docs` also passed. There are **111 Rust tests**, zero failed or ignored: the previous 76 plus 10 wire tests, 13 receiver tests, 10 sender/receiver integration tests and 2 corpus-reader example tests. The new packet impairment tests run the real byte codecs and delivery owners with injected clocks; they are not Asupersync lab execution or a physical network benchmark.

### Real HEVC preservation and independent software decode

The opt-in `scripts/verify_hevc_delivery.py` lane passed locally on the same Rust sources with FFmpeg/ffprobe `7.1.5-0+deb13u1`. Both synthetic corpora were Main 8-bit 4:2:0, 640x360, generated at 30 fps with periodic IDRs. Delivery uses a virtual impairment clock, not a measured realtime throughput/latency claim.

| Corpus | Encoded bytes | Original records | Dropped video packets | Duplicate deliveries | Repaired packets | Independently decoded frames |
|---|---:|---:|---:|---:|---:|---:|
| 90 frames / 3 IDRs | 405,338 | 514 | 87 | 328 | 87 | 90, identical hashes |
| 240 frames / 8 IDRs | 1,083,917 | 1,364 | 222 | 893 | 222 | 240, identical hashes |

The whole source/delivered 90-frame corpus has SHA-256 `19c68ed85668520d38e0be513f191a0ed19e3a86cfc7f9f559572ae4c37b9683` on both sides. Both independent 90-frame decoded-hash files have SHA-256 `19b2bf195ee511db3132b80b6fda82d6745516bec586d43bda69a6d33caa2f4e`. Each lane checks all access-unit bytes and all decoded frame hashes, and requires nonzero drops/repairs. Decoder stderr was empty in both local corpus runs. Exact commands, versions and generated artifacts are retained by the lane in a fresh user-selected directory, never committed as screen recordings.

This adds **real HEVC byte-preservation and independent software-decode evidence**. It does not add a shipping FFmpeg wrapper, hardware encoder/decoder, capture source, browser presentation, live Tailscale/QUIC/H3 endpoint, physical latency measurement, or OS input injector. The Rust fixture diagnostic models decode completion after byte comparison; the separate FFmpeg decoder checks the delivered stream afterwards. Full parameter-set/DPB security validation before a production decoder is still required.

The next critical integration work is the qualified Asupersync transport composition and one real capture/HEVC/presentation path, joined to current authority and delivery owners. Repository task `fr-5nb` records the prior QUIC qualification defects; it needs fixes/requalification, not a second shipping QUIC implementation. `fr-fr-wire-framing-i0u` is only partially implemented because non-media message classes and its fuzz campaign remain outstanding. No Beads task is declared closed by this delivery pass; `br` was unavailable in this execution environment.

## Earlier authority/media-contract slices

| Component | Present behavior | Boundary still requiring integration |
|---|---|---|
| `fr-core::ids`, `limits`, `time` | Distinct identities/generations, checked clock arithmetic, downward-only negotiated limits, checked geometry/allocation arithmetic | Qualified randomness, real host clocks, transport decoding, resource ownership across processes |
| `fr-core::authority` | Separate observation/readiness/control; exclusive deadlines; issue-time challenge renewal; submission-time authorization; refusal/closure cleanup; suspend fencing; clock-regression refusal | Tailnet identity and approval adapters, global controller arbitration, geometry/mapping checks, OS injection and actual held-key cleanup |
| `fr-core::input_sequence` | Lease-scoped monotonic consumed floor, bounded receipt ring, duplicate suppression after receipt eviction, single pending action, explicit uncertain/partial outcomes, no counter wrap | Attach to the authenticated ordered input stream and final submission gate; never reconstruct a ledger to resume an old lease |
| `fr-media` contracts | Checked declared configuration/geometry, capability/admission helpers, opaque surface/codec interfaces, frame metadata, copy accounting, test-only fake backend | Real capture, hardware encode/decode, HEVC syntax/reference validation, FFI ownership, device qualification |
| Diagnostic formatting | Access-unit `Debug` excludes compressed screen bytes; authority-owner `Debug` excludes live challenge/ticket/lease material; authority owner is not cloneable | Review every subsequent logging/trace boundary; metadata/IDs remain sensitive deployment information even where they are not bearer credentials |
| Repository verification | Pinned-nightly formatting/check/Clippy/test lane, feature-gated media contract tests, documentation link checks, explicit blocked full/release states | Native builder and hardware matrix, UBS, fuzzing, independent transport interoperability and end-to-end application evidence |

The core and media crates add no new external runtime or codec dependency in this slice. Neither the declared workspace Asupersync dependency nor a fake media backend demonstrates an integrated asynchronous transport or HEVC encoder.

## Earlier authority/input source evidence

The source commit [e7d57d5a1a284ec0b8da6374d3eda8613167c1e1](https://github.com/Dicklesworthstone/franken_remote/commit/e7d57d5a1a284ec0b8da6374d3eda8613167c1e1) passed [Rust verification run 34231571592](https://github.com/Dicklesworthstone/franken_remote/actions/runs/34231571592), job `102078753267`, on Ubuntu 24.04.4 x86_64 with `nightly-2026-08-31` (`rustc 1.100.0-nightly`, `908501772`, 2026-08-30).

| Gate | Result |
|---|---|
| `cargo fmt --all --check` | Passed |
| `cargo check --workspace --all-targets --all-features --locked` | Passed |
| `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` | Passed |
| `cargo test --workspace --all-features --locked` | Passed: 47 core unit tests, 17 media unit tests, 9 media contract tests, 3 compile-fail doctests; 76 total, zero failed or ignored |
| `./scripts/verify.sh docs` | Passed at the source evidence commit |

The nine media contract tests use an explicitly test-only fake backend. The authority/replay tests inject host-clock values into the real policy implementation. Neither category is a live Asupersync lab run, a GPU benchmark, or proof that an OS accepted/released input. Compile-fail doctests check identifier separation and that a live authority owner cannot be cloned.

UBS, Miri, fuzz-engine campaigns, macOS/Windows native builds, browser/mobile execution, live Tailscale/QUIC/H3, hardware capture/HEVC, and installer/rollback qualification were **not run in this implementation pass**. The `full` lane requires UBS and returns a blocked result when it is absent. The `release` lane intentionally remains blocked.

## September 8 implementation changes

| Commit | Change |
|---|---|
| `0de2ca6` | Replace the pre-implementation verification refusal with actual workspace gates; add the missing `fr-media` lockfile entry |
| `2f9e05f` | Fix refused-session credentials, challenge carry-over, issue-time renewal deadlines, exact-deadline submission, stale-view ticket reuse, observation/control coupling, suspend and clock-regression fencing |
| `53607b5` | Resolve pinned-nightly formatting and existing lint failures without weakening checks |
| `2ceb847` | Redact compressed desktop payloads from diagnostic formatting and add bounded-output regressions |
| `752d106` | Add bounded lease-scoped replay accounting and an authority-expiry integration regression |
| `f5f40f9` | Redact authority credentials and remove `Clone` from live authority owners |
| `e7d57d5` | Assert must-use admission outcomes, format the new regressions, and obtain the complete passing source run above |

The review-only source-maintenance workflow emits a diff and Git blob objects for inspection; it does not commit, update refs, create branches, or open PRs. Machine-applicable repairs were reviewed and committed separately. Repository-owned verification commands remain the source of truth, not the existence of a green historical job.

## Authority/input integration obligations

Keep one serialized authority owner per remote session under the OS share-session owner. The containing owner must authenticate the principal, enforce optional local approval, reserve the single global controller slot, and perform held-key cleanup before handoff. Sample the host clock immediately before submission; a previously accepted packet or cached timestamp is not current authorization. On actual suspend/wake, call the explicit suspend fence even when the selected clock has not advanced.

For each reliable input action, consume its sequence before any possible external effect, then recheck live observation/control, the ticket, geometry, and viewport mapping immediately before injection. Record whether submission happened, partially happened, was refused/expired/cancelled before submission, or has an unknown effect. A failed or uncertain predecessor fences dependent actions. Receipt eviction never authorizes re-execution. Release-only cleanup belongs to the local input owner, not a retry of the remote action.

The input framing and final submission owner above now join those policies to an explicit X11 pointer/button path. The authenticated host/client session, independently progressing input watchdog and native lifecycle integration remain unfinished while the Phase 0 transport/media experiments continue. Do not turn policy tests or Xvfb effects into a claim of a working remote desktop.

The earlier implementation passes did not rewrite Beads task records because `br` was unavailable. Their broader original acceptance criteria remain intact; the receipt pass above records its claim and blocking protocol defect explicitly. The source and revision-scoped evidence identify what actually landed.

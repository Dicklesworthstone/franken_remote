# Implementation status

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

Local execution remains unavailable. Hosted verification for the preceding file
checkpoint has started; this new clipboard slice has no compiler, Clippy or
runtime pass at this checkpoint. The shared-clipboard and loaded-session beads
remain open for their existing acceptance work.

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

**Added and independently source-reviewed, not executed:** transport regressions
for queued and native-staged sends in both file directions, control/sibling
continuity, consumed identities and terminal negatives; real ATP/disk scenarios
for partial staging cleanup, earlier batch receipts, and an expired publication
reply; and an original ControlledViewer regression that sends input under the
same lease after file retirement. CLI projection checks preserve earlier and
unknown publication evidence.

The exact pinned toolchain and locked dependencies were prepared, but the local
execution environment became unavailable before compiler or test output.
Compilation, strict Clippy, full workspace tests, the new runtime regressions
and the loaded namespace acceptance remain unverified at this checkpoint.
Existing GitHub verification runs follow the direct main commit. This is not a
live-tailnet or hardware qualification; `fr-jlgm` remains open.

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

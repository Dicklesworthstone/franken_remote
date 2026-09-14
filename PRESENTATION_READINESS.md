# Presentation-backed host readiness

The `presented-state` version 1 capability connects the existing viewer visibility
tracker, source-provenance verifier and finite host view gate to the continuous
session services. A configured viewer can now acquire control without a caller
manually marking the host view ready. This is a working library integration, not
a completed desktop application or a physical-scanout attestation.

## Using the integrated path

Select `fr_wire::presented::CAPABILITY` and `fr_wire::presented::VERSION` on both
endpoint offers, alongside the existing decoder, media-delivery, input-attachment
and native-control-grant capabilities. The selected control-record bound must
admit `fr_wire::presented::BYTES`. This extension never installs implicit channels,
changes an observation-only session into a control session, or bypasses approval.

Enable the existing session clock exchange before control acquisition. Build the
original configured `Stream` and negotiated viewer startup as usual. Preserve the
actual first decoder completion using `ViewerSession::into_streaming_presented`.
Run the host with `StreamingHost::serve_accepting_control` and the viewer with
`StreamingViewer::serve_requesting_control`; both continue servicing their original
workers, receiver, packet cache, connection and observation authority.

The viewer callback confirms only the coordinate mapping it actually installed,
using `PendingViewerControl::confirm_mapping`. It calls `visible(frame)` only
when its qualified platform path has independently confirmed presentation of that
exact frame. A decoder completion or compositor-submission receipt is not such
confirmation. The same requirement applies to `ControlledViewer::visible` after
the grant. The existing pure observation callback has no independent visibility
acknowledgement and therefore does not fabricate positive reports.

The host callback can query `PendingHostControl::view_ready()` while a request is
pending. This query neither reserves the Seat nor creates authority. A separate,
explicit local decision calls `approve` for the current qualified target. The
broker rechecks readiness and admission, reserves the original shared Seat, and
initializes the native owner on its existing independent path. Immediately poll
the returned Driver in its authority task, including during startup and cleanup.
Returning a target, receiving a report, or seeing decoder progress is not consent.

## Evidence and lifetime

The host learns source stamps only from its own admitted capture/packetizer owner,
not from a peer-supplied timestamp or the act of sending a heartbeat. Each report
must use the original connection's registered, reliable, critical session-control
route. It binds the host boot, OS session, remote session, display, geometry,
codec configuration, recovery and viewport generations. Equal numeric routes on
a different connection cannot borrow that authority.

The host retains 64 source-stamp records, not another queue of encoded frames or
surfaces. A positive report must match that history. Readiness expires at the
original host source-observation time plus the protocol's 250 ms maximum source
age; neither receipt time nor repeated reports slide that deadline. Enabling this
mode retires legacy unbounded readiness. Existing native input monitors check
finite readiness immediately before OS submission, even without another packet
or watchdog service turn. A report cannot create a lease or resurrect an expired
one.

The viewer retains one fixed-size pending report and bounds positive-report
cadence to 50 ms. Its send deadline starts from the actual visibility/source-age
sample, not the later network service call. Backpressure preserves exact bytes
and the original deadline; the final transport guard checks expiry again.
Explicit loss of source/visibility evidence preempts positive-report throttling
with one bounded negative report, which later positive evidence cannot replace
while it is pending. Repeated absence emits no readiness heartbeat.

A short interval between compositor submission and independent visibility is
handled separately: the reporter drops unsent positive evidence and preserves
the preceding host deadline. It cannot certify the candidate or extend readiness.
Existing input logic pauses submissions during this gap. Unknown/stale source
state invalidates readiness; terminal platform lifecycle events retain the
existing immediate stop behavior. Old source reports cannot reopen a stale gate.

Qualified unchanged-source observations keep a genuinely static view current
without encoding or decoding dummy frames. The reporter and source verifier
survive the observation-to-control handoff, so report sequence, source history,
original worker identity and replay protections are not reset during promotion.

`presentation_reports()` exposes positive source-matched host reports or reports
admitted to the viewer's transport, respectively. These counts are not proof of
remote receipt, physical visibility, scanout, or exactly-once OS execution.

## Executed verification

Pinned nightly: `nightly-2026-08-31`, rustc `1.100.0-nightly` / `908501772`.
First-party libraries and tests were rebuilt against compiler-matched dependency
artifacts retained by Actions run `34786108173`, artifact `10326771799`, from
source commit `13a40a186d96d499ab4ba60c9463fb4795c480cf`. The dependency artifacts
were used only for local verification and are not vendored into the repository.
Newer native relative-input changes on main were preserved, not rewritten.

The daemon suite passed 187 tests, with 16 pre-existing native/namespace cases
still gated and not counted as passes. A further 23 selected suites passed 189
tests with no failures or skips: all media integration suites, all transport
integration suites, core bounded-view-readiness tests and wire presentation-state
tests. Strict Clippy for the changed libraries and unit-test targets passed.

The integrated control test uses real UDP/TLS, negotiated channels, the original
session/grant/input owners and two supervised processes. Its codec replies,
source verification, platform visibility and counted native input sink are
explicit fixtures. It disables manual host readiness, obtains a real protocol
grant after presentation and local consent, returns key-press/release receipts,
and renews the same control owner beyond the initial lease. During the steady
idle interval there are no additional encoded or decoded pictures. Companion
cases prove that valid presentation cannot supply consent and decoder submission
without visibility cannot report readiness or start native input.

Other cases exercise original-deadline expiry without more network traffic,
negative reports and replay, source-history mismatch, foreign bindings/routes,
connection substitution, report sampling-to-service delay, and real critical-send
queue backpressure. The prior lost-visibility test's clock-sampling race was fixed
without changing production deadlines: an expiry between the loop's clock check
and its drive is accepted only after asserting that the original deadline passed.

Repository commands for normal builders include:

```sh
cargo test -p frd --lib session_startup::running::streaming --locked -- --test-threads=4
cargo test -p fr-media --test presented_provenance --locked
cargo test -p fr-core --test bounded_view_readiness --locked
cargo test -p fr-wire --test presented_state --locked
./scripts/verify.sh fast
./scripts/verify.sh docs
```

This evidence is an artifact-assisted first-party rebuild, not a cold complete
native workspace build, newly completed CI, independent wire interoperability,
or physical/two-machine HEVC/display qualification. Desktop consent/visibility
UI, controlled convenience bootstrap, listener integration and platform
qualification retain their separate acceptance requirements. The broader broker
and input beads are advanced, not declared complete.

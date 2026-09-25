# Bounded native X11 line scrolling

This joins the existing client Scroll action, FRD0 scroll record, final input
submission owner and X11 input sink. It adds no wire record, transport, runtime,
codec, crate dependency or delayed work queue. Asupersync QUIC remains primary.
It advances `fr-p1-input-pipeline-ay1`; the broader input/application milestone
remains open.

## Units and supported native subset

The protocol already specifies signed 16.16 distances. `65536` is one line;
`1` is not one wheel step. The discrete X11 path accepts whole-line values only,
with at most **32 combined horizontal and vertical steps per action**. Positive
horizontal values map right and positive vertical values map down. The mapping
uses logical X buttons 4/up, 5/down, 6/left and 7/right; the adapter inverts the
current physical-button map before each press.

One nominal line produces one wheel click. Application preferences determine the
resulting document movement; neither the receipt nor this profile claims an
exact number of rendered text lines. Fractional lines, pixel scrolling and
oversized discrete actions are explicitly refused. They are not rounded,
converted, truncated, split across newly authorized actions or accumulated into
future events. Other native backends retain the existing atomic Scroll operation
and can still consume fractional or pixel distances unchanged.

`Capability::LineScroll` is advertised only after the existing XTest 2.1-or-later
major-version-2, single-X-screen qualification and a valid map for all four wheel
directions. Multiple RandR monitors inside one X screen are not excluded by the
screen-count check. No Xwayland or alternate-display fallback is selected on
permission failure. The X11 adapter still does not advertise PixelScroll.

## One action, individually authorized native effects

The core `LineScroll` planner validates the entire expansion before changing the
pointer barrier or submitting its positioning operation. A complete discrete
action submits the original absolute pointer position, then horizontal steps,
then vertical steps. Each step has one press and one release. Zero distance
performs only the existing positioning operation, not a dummy wheel event.

`InputSink::line_scroll_requires_pairs()` selects the discrete realization; its
default is false for the existing atomic backend contract. A wheel transition is
an explicit `Operation::Wheel`, not an invisible loop inside a single platform
call. All transitions go through `Attempt::one`, which completes native
preparation before sampling the original authority/ticket immediately before
submission. The maximum is **65 OS API submissions**, not 65 application effects.

All steps retain the original reliable action sequence, ticket, view generations
and deadline. Backpressure or partial effects never reissue the action with a
new identity. A duplicate reliable action returns its retained receipt rather
than scrolling again. The existing pointer barrier, source-freshness checks,
input-mode checks and release-before-handoff discipline remain in force.

## Partial effects and cleanup

Both the core and native sink record a possible wheel press before entering the
native API. A definite NotSubmitted result restores the previous core state;
an unknown result or unwind retains possible ownership and the confirmed prefix.
A confirmed release clears that ownership. This is distinct from rollback:
a completed wheel press can already have scrolled an application.

If the ticket expires or the user revokes control between press and release,
the release is not submitted as another authorized remote operation. The action
terminates with its actual submitted prefix. The existing local release-only
cleanup then releases the recorded wheel without producing any future scroll
step or granting authority. Uncertain cleanup remains tracked for local retry.
Native Drop also attempts release of the original physical code.

Mapping is checked before each new press, but release uses the recorded physical
code rather than resolving a potentially changed map. A code already held by
this owner's ordinary buttons is refused. Pre-existing vertical wheel presses
are refused and are not claimed or released by remote cleanup. Core X11 exposes
held masks only for buttons 1 through 5; equivalent ownership attribution for
horizontal buttons 6 and 7 is not claimed. Simultaneous local input, grabs,
confinement, map changes and X-server failure remain platform trust limitations.

## Receipt compatibility

A successful atomic Scroll still reports two API submissions (position, scroll).
A successful discrete Scroll reports `1 + 2 * steps`. The client accepts either
complete realization when that exact bounded whole-line request permits both;
it does not accept an intermediate count as a completed action. Partial and
unknown receipts retain the actual prefix and stop further actions as before.
No receipt means “exactly once application execution.”

The GUI event queue submits positioned line-scroll actions to this backend.
The X11 wheel-event collector and the command-profile integration are described
below. High-resolution smooth scrolling and live-tailnet desktop qualification
remain separate work.

## Verification and reproduction

The increment adds **23 tests**: ten pure final-submission/planner tests, four
client receipt tests and nine real Xvfb/XTest scrolling integrations. The native
cases pass actual encoded client actions through production parsing and final
input authority, then return encoded native receipts to the client. An
independent Xlib connection observes wheel press/release events, their logical
directions and the action's pointer position. Grants, clock samples and
presentation are explicitly local fixtures; the X server and input effects are
real.

Coverage includes signed fixed-point limits, direction order, zero steps,
maximum work, fractions, oversized values, replay, remapping, missing mappings,
expiry/revoke between native effects, partial receipts, unknown presses/releases,
native unwind, release-only retry, Drop cleanup and a pre-existing vertical
wheel press belonging to another local owner. A fractional/oversized request
cannot even reposition the pointer before refusing.

The final local scoped workspace passes **464 tests at each of one, four and
eight test threads**, with no failures or ignored tests. Strict Clippy,
formatting and generated API-documentation checks are separate retained logs.
Two deliberately broken source copies fail unchanged regressions: removing
per-wheel final authorization permits work beyond expiry; accepting the full
minimum-to-maximum count range treats an incomplete wheel sequence as complete.

Local validation uses pinned nightly-2026-08-31 and an external five-crate Cargo
workspace. The modified native/core/client files match the published preimages
from `01bedf8`; comparison through `c1914fc` showed no concurrent edits to these
files. Unrelated test/dependency sources are the recovered `3a99e73` snapshot,
with the current finite-readiness authority source restored. This is a scoped
rebuild, **not a complete current-main dependency build or CI pass**. The seven
relative-input integration tests added after that snapshot were not rerun in this
local slice; the clean publication CI below subsequently ran those cases too.
No production dependency, feature or repository Cargo manifest is altered
by the external build arrangement.

In a normal complete checkout with the pinned toolchain and native SDKs:

```sh
cargo test -p fr-core --test scroll_submission --locked
cargo test -p fr-client --test scroll_receipts --locked
cargo test -p fr-native --features linux-input --test scroll_x11 --locked -- --test-threads=4
cargo clippy -p fr-core -p fr-client -p fr-native --all-targets --features fr-native/linux-input --locked -- -D warnings
```

## Publication verification — 14 September 2026

The recovered implementation is published on `main` in `21e6e7e`, followed by
the native regression tests and integration contract in `d79c3c5`. All ten
published source objects are the exact objects exported after successful
verification; no reconstructed or unverified replacement source was published.

Fresh local verification repeated all 464 scoped tests at one, four and eight
test threads and separately passed strict Clippy, formatting and generated API
documentation checks. Both deliberately broken copies failed their unchanged
regressions. The local snapshot limitations above remain explicit.

Separately, clean GitHub Actions run `34844573653` completed successfully on
`767c141b`. That run checked every published preimage before applying the reviewed
candidate, built the complete pinned dependency graph, and passed the full
workspace `fast` and documentation lanes. It also repeated the core scrolling,
client receipt and native input suites at one and eight test threads, including
the previously published relative-input tests, keyboard tests and concurrent
XTest-cache test. The verifier checked all resulting source hashes again and
exported them only after every test step succeeded.

The finalized workflow verifies committed sources read-only. It no longer stages
a candidate patch, receives a write token or creates Git objects. A fresh run
against the final documentation/workflow commit is separate evidence; the
completed candidate run does not imply that later checks have already passed.

Physical devices/displays, live Tailscale ingress, Wayland, macOS and Windows
qualification remain separate acceptance work. The former saved scrolling patch
is superseded by these published commits and should not be reapplied.


## Command integration — 25 September 2026

The `fr connect NODE --control --experimental-native --display only` profile now
requests `Capability::LineScroll`, and `frd run --software-explicit --input-agent
PATH` permits that same native operation. The original control grant, fresh-view
checks, action sequence, ticket deadline, native preparation and final submission
checks still apply. Observation-only clients never gain input permission.
Use matching current host/client builds: a host without the requested operation
refuses the request rather than silently changing its scope or retrying it.

The X11 viewer maps button-press notches 4/5/6/7 to up/down/left/right using the
existing `LINE = 65536` fixed-point unit. Its former `+/-1` values were fractional
lines that the discrete native backend correctly refused. Wheel release edges
produce no second scroll; missing negotiated line-scroll capability ignores the
wheel without consuming subsequent key actions. Pixel scrolling, relative input
and committed text remain excluded from this command profile.

Verification for this integration: a new actual-X11 capture regression fails on
the original unit conversion (`-1` instead of `-65536`) and passes on the fixed
source. All 23 tests in the selected native input/capture build pass under Xvfb,
as do all 11 existing native scroll tests (including independent Xlib observation,
expiry, revoke, replay and remapping). Strict pedantic Clippy for the capture test
build and pinned rustfmt pass. Two additional local contract checks compile the
exact host/client capability function bodies, verify equality across all eight
operation bits, and confirm that the set fits the real X11 sink's capability
probe. Those two checks do not compile the enclosing CLI or daemon modules.

This is scoped evidence: first-party libraries were rebuilt from the retained
`2fc9711` snapshot using the pinned compiler and matching unchanged external CI
libraries. The capture source and its original tests were hash-identical to
current main before the fix; the two command-profile files were separately
reconciled to their exact current-main hashes before editing. The complete
current-main workspace and the two-machine/namespace CLI scenario have NOT been
rerun for this change. No physical wheel, smooth-trackpad, GPU or live-tailnet
qualification is claimed.

In a full checkout, the capture regression can be repeated with:

```sh
xvfb-run -a env FR_NATIVE_INPUT_CAPTURE_REQUIRED=1 cargo test -p fr-native --features linux-input,linux-viewer-input --lib viewer_input::tests --locked -- --test-threads=1
```

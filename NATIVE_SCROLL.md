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

The existing GUI event queue can already submit positioned line-scroll actions.
This implementation supplies their native backend. It does not install a GUI,
provide a platform wheel-event collector, implement high-resolution smooth
scrolling or qualify a live tailnet desktop session.

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
slice. No production dependency, feature or repository Cargo manifest is altered
by the external build arrangement.

In a normal complete checkout with the pinned toolchain and native SDKs:

```sh
cargo test -p fr-core --test scroll_submission --locked
cargo test -p fr-client --test scroll_receipts --locked
cargo test -p fr-native --features linux-input --test scroll_x11 --locked -- --test-threads=4
cargo clippy -p fr-core -p fr-client -p fr-native --all-targets --features fr-native/linux-input --locked -- -D warnings
```

Full current-workspace CI, physical devices/displays, live Tailscale ingress,
Wayland, macOS and Windows qualification remain separate acceptance work.

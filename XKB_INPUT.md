# Native physical keyboard input

This extends the existing `fr_native::input::X11Pointer` input sink under
`linux-input`; it is not a second input protocol or a competing platform adapter.
Asupersync QUIC remains the primary transport. No transport selection, runtime,
codec, tailnet grant, or authenticated-listener behavior changes here.

## Executable path

`fr_wire::input` record -> `InputSession` sequence and authority checks -> XKB
preparation -> final host-clock check -> XTest submission -> staged receipt.
The sink advertises Keys/Repeat only when its XKB extension probe succeeds.
Physical keys are mapped from USB HID positions to exact XKB key names. Unknown
positions refuse; characters, keysyms and guessed evdev offsets are never used
as a physical-key mapping. Text, scroll and relative motion are still unsupported
by this native adapter and do not silently use clipboard paste or layout guesses.

X11 server-generated repeat is disabled per remotely held key and its original
setting is restored after release. Explicit repeats are decomposed by the shared
input owner into release and press with a fresh authority check before EACH
native operation. Expiry between them reports the confirmed release as partial;
it never retries the old repeat with a fresh ticket. A cancellation/unwind guard
restores reversible platform preparation when final authorization fails.

The native keyboard tracks the exact submitted keycode, including across keymap
changes. A pre-existing key press is refused rather than claimed as remote input.
Release cleanup is local-only, and closing the native owner performs best-effort
release/restoration before closing its display. Fixed per-key storage is bounded;
no credentials or input content enter diagnostics or the native ABI helper.

## Verification

Nine `keyboard_x11` integration tests exercise the actual FRD0 input codec, shared
InputSession, native XKB/XTest calls and an independent Xlib client observing its
window, keyboard state and repeat settings on fresh local Xvfb servers. Six cover
keyboard behavior: press/release, duplicate suppression, explicit repeat without
a second server-generated stream, ticket expiry after preparation, expiry between
repeat release/press, refusal of pre-existing input, physical-keycode retention
through keymap changes, and local revoke during preparation. Three additional
cases exercise swapped button mappings for all five buttons, simultaneous key/
drag cleanup on normal Drop, and refusal to claim/release another local drag.

All nine passed locally, alongside the four unchanged pointer integration tests,
with the repository-pinned nightly and the actual Cargo-built first-party
libraries. Strict native-library Cargo Clippy and pinned Clippy checking of both
integration test sources also passed. The reproduction command is:

```sh
cargo test -p fr-native --features linux-input --test keyboard_x11 --locked -- --test-threads=1
```

Requirements: Xvfb, X11 development headers, the installed libXtst.so.6 ABI,
a C compiler and ar. `linux-input` alone does not build or link FFmpeg. The
independent observer is test-only; no fabricated native backend is used.

## Integration limits

InputSession cleanup and native preparation cleanup are separate evidence. Before
controller handoff, retire authority, require core held-state cleanup to finish,
and require `X11Pointer::cleanup_native()` to return true. A failed restoration
is retained and blocks new presses; zero core-held keys alone does not certify
that preparation was restored. `cleanup_keyboard()` covers only the keyboard;
Drop is best effort, not an acknowledged handoff.

X11 cannot perfectly distinguish a simultaneous physical local press of the same
key from an injected press. Per-key repeat settings are X-server-global while
held. Process termination or X-server failure can leave effects/restoration
uncertain. Xlib may block or terminate on connection loss; independent process
supervision and the idle watchdog must be supplied by the session-agent runtime.
This work does not claim those lifecycles, OS login/lock handling, Wayland access,
physical-device qualification, live QUIC/Tailscale admission, or a complete remote
workstation. Submission means OS API submission, not arbitrary application success.

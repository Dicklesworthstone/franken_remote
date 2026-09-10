# Selected display to native media startup

`frd::display_selection` implements the initial full-display choice on an
already approved native session, using the existing `display-selection` version
1 wire capability. Asupersync QUIC remains the sole primary transport. This is
an application integration, not a new listener, identity policy or input grant.

## Running session and selection ownership

`HostSession::select_display` publishes the locally enumerated, approved
`DisplayCatalog` on the original session-control stream. The catalog holds at
most eight display entries, with opaque handles, signed desktop origins,
explicit dimensions/scale/rotation and geometry generations. OS enumeration and
local disclosure policy remain outside the network callback and must not block
the authority owner. No peer string becomes an OS display selector or path.

`ViewerSession::select_display` receives that exact catalog and requires an
explicit local choice. Even a one-entry catalog is not implicitly selected. The
viewer sends the existing `SelectDisplay` record; the host checks its catalog
revision, handle, geometry and complete full-display rectangle. Unknown outputs,
stale revisions, crops and implicit scope expansion refuse instead of clamping.

Only one initial selection exchange may be claimed per connection. Both sides
transfer their non-cloneable `SelectedDisplay` owner into the selected view's
lifetime. It retains the original QUIC connection, full session identity,
selection limits and chosen metadata. A different connection with identical
numeric route IDs cannot use it or receive its pending catalog.

Keep driving the canonical host/viewer session while display selection waits.
Display dispatch consumes only its own records; observation renewal and clock
messages remain with their existing dispatchers. Merely putting SelectDisplay
in the transport queue is not a host acknowledgement. Subsequent attachment
bindings and decoder configuration must match the viewer's retained choice.

A local topology refresh calls `SelectedDisplay::revalidate` with a current
catalog. Changes to other outputs do not retarget the selected view; removal or
changed metadata of the selected output invalidates it. The local provider must
never reuse a handle/generation for a replacement output. Dropping or invalidating
the owner ends its original observation lifetime. Existing input owners retain
their separate final-submission checks and cleanup responsibilities; this owner
does not claim to have completed OS key/button cleanup.

This initial profile shares one full display per session. In-place resize,
seamless monitor switching and multiple concurrent selected displays are not
implemented here; a changed selected view requires a new checked session.

## The choice constrains the actual decoder

`SelectedDisplay::binding` supplies the selected display and exact initial view
generations for configuration, recovery and video attachment. Completed
attachments still pass through `NegotiatedMedia`; no route is manufactured from
an untrusted list of stream numbers.

`SelectedDisplay::decoder_setup` additionally constrains the visible dimensions
of native startup. Matching opaque IDs alone is insufficient: an otherwise valid
HEVC stream with a different visible width or height refuses before worker launch.
The host also refuses a capture configuration of different selected dimensions.
Coded padding remains permitted under the existing HEVC admission rules. Color,
parameter-set, resource and reference validation are not bypassed by selection.

The native integration now executes:

```text
actual X11 geometry -> approved catalog over QUIC -> explicit viewer choice
  -> selected configuration/recovery/video attachment bindings
  -> source-owned HEVC IDR and network decoder configuration
  -> selected-size and HEVC validation -> supervised decoder configuration
  -> DecoderConfigured -> reliable IDR -> decode and pixel readback
  -> dependent P picture through video datagrams, progress and selective repair
```

Configuration acceptance, decoding, visible presentation, local consent and
control remain different states. This exchange cannot enable input, revive an
expired admission, choose a different OS user or widen the approved desktop scope.

## Bounds and failures

One fixed pending-record buffer is retained during the exchange, with the
existing bounded catalog representation. Backpressure preserves exact bytes and
the original exclusive deadline; polling, unrelated traffic and repeated choice
do not extend it. The timeout must be positive and at most 60 seconds, and live
observation/admission checks continue to apply independently throughout that wait.

Premature or replayed selection records, invalid bindings, malformed records,
stream termination, clock regression and cancellation terminate the affected
selection. An unfinished owner cannot be dropped and recreated to reset the
connection's selection allowance. No media, input, platform or codec call occurs
inside the synchronous display-record dispatcher.

## Executed evidence

Local verification uses pinned nightly-2026-08-31 and Asupersync 0.4.10. The
seven new display integration tests use actual localhost UDP/TLS with synthetic
admission and display metadata. They cover explicit choice, real credit pressure,
fixed expiry, stale/cropped/replayed selections, foreign connections, abandoned
owners, invalid setup and selected-versus-unselected topology changes.

One additional canonical-session test keeps observation renewing past the
original grant while the viewer waits on its catalog, then completes selection
without replacing either session. All 31 daemon unit tests pass.

All 14 native decoder-startup tests pass. Their two new cases reject otherwise
valid wrong-sized HEVC before native launch, reject wrong-sized host capture and
verify selected-owner drop revokes observation. The normal startup and whole-final-
picture-loss tests now obtain all three media bindings from the network-selected
X11 display. They retain actual software HEVC, supervised child processes,
independent Xvfb servers, pixel readback and dependent-frame continuation/repair.
Only initial admission/session-control and the local catalog provider remain test
fixtures; these are not live-tailnet, GPU, physical scanout or WAN measurements.

A separate negative-control source copy removes only the viewer's selected-size
check. The unchanged test then fails at `display mismatch must precede native
launch`; the production source and regression assertions remain unchanged.

After reconciling the concurrently published negotiated input broker, the local
selection passes 574 tests: 363 core/wire/media/client Cargo tests, 31 daemon unit
tests, seven display tests, 14 native decoder tests, 55 native input tests, 20
attachment tests, 16 native-QUIC tests and 68 existing daemon integration tests.
There are zero failures or ignored cases in those production selections. These
local runtime tests rebuild first-party code against matching retained Asupersync
libraries, not a cold dependency graph. Clean CI results are recorded separately.
The seven display and fourteen native startup tests each also passed eight
repetitions across one, two, four and eight threads; the session-startup selection
passed three four-thread repetitions.

## Publication and clean CI

The selection/session/decoder constraint implementation landed in `dc756baa`,
followed by the native selected-display integration in `8d5c8b4e`. Their eleven
exact source objects passed clean pinned workspace formatting, compilation,
strict Clippy, tests, example tests and documentation checks in
[run 34536786406](https://github.com/Dicklesworthstone/franken_remote/actions/runs/34536786406),
against `f0af4057`. That run also executed the focused display, session and native
startup tests, exporting matching immutable source objects only after success.
Those exact objects were published directly to the current main branch.

The first candidate run, 34536443028, stopped at a preimage mismatch before
compilation because concurrent broker work changed `running.rs`. It is not a
passing build. The revision reconciles that source change without removing the
new broker methods or weakening any selection, authority or test requirement.
The verification workflow now checks committed source read-only, with no staged
patch replay, source export, write token or branch mutation. A later combined
checkout's CI status is separate from the successful exact-source candidate.

Reproduce on a provisioned Linux checkout:

```sh
cargo test -p frd --test display_selection --locked
cargo test -p frd --lib session_startup --locked
cargo test -p fr-native --all-features --test decoder_startup --locked
cargo test -p fr-native --all-features --test input_quic --locked
./scripts/verify.sh fast
./scripts/verify.sh docs
```

Protected Tailscale ingress, production OS display enumeration, hotplug event
wiring, desktop GUI selection and the enclosing installable application remain
separate work. No upstream transport change or alternate stack was introduced.
UBS and Beads tooling were unavailable; no issue or full phase gate was closed.

Related: [NATIVE_MEDIA_ATTACHMENT.md](NATIVE_MEDIA_ATTACHMENT.md),
[DECODER_STARTUP.md](DECODER_STARTUP.md), [SESSION_DRIVERS.md](SESSION_DRIVERS.md),
[BROKER_CONTROL_GRANT.md](BROKER_CONTROL_GRANT.md), [PROTOCOL.md](PROTOCOL.md).

# Window-coordinate input and local zoom

The shared client mapper and persistent `ControlledViewer` now translate local
window events into the existing host-desktop pointer, button and scroll records.
There is no new wire format, input executor, native queue, host grant or runtime.
This is the coordinate/input integration for a thin platform shell, not a complete
native windowing implementation or physical-display qualification.

## Coordinate contract

`LocalPoint` uses signed 1/256 physical-window-pixel coordinates. Integer physical
coordinates use `pixels`; fractional coordinates use `subpixels`. Toolkit logical
coordinates use `logical` with that window's actual rational DPI scale. Scaling
uses checked wide arithmetic and floor division, including for negative values;
NaN, infinity and guessed DPI do not enter this representation. The platform must
supply the correct coordinate convention and retire the layout on DPI changes.

`SurfaceRect` describes the physical window area available for the image, including
any toolbar offset. `Viewport::configure` aspect-fits a source subrectangle into
that area and returns the exact integer source/destination placement the renderer
must use. Odd letterbox remainders and rounding therefore have one definition,
shared by rendering and hit testing. Events use half-open destination bounds;
letterboxes, toolbars and outside points refuse rather than clamp onto a host edge.
Coordinates map by floor of the exact linear position in the source rectangle.
No renderer-only rotation or additional transform may be applied behind this API.

Source rectangles are signed host-desktop pixel coordinates and must lie wholly
inside the granted image. Negative monitor origins and large checked extents are
supported. Local zoom/pan samples already-received pixels; it never enlarges host
capture, selects another monitor, changes a host viewport generation or grants
another capability. When sampling a decoded texture, subtract the granted desktop
origin from the returned source rectangle. A host crop or geometry change still
requires the existing new-view/acknowledgement lifecycle, not a local reconfigure.

## Layout and owner lifetimes

The local transform owner retains one layout. Every configure attempt first
retires the old layout, including an invalid source rectangle. Call
`invalidate_viewport` immediately when a resize/DPI/zoom event arrives, before
validating new dimensions or waiting for the renderer. Thus a failure constructing
new dimensions cannot leave a stale transform active accidentally.

Each returned `Layout` has an opaque retained identity. The platform stamps an
event with `Layout::at` WHEN SAMPLED, not later when an event queue is drained.
`confirm_viewport` acknowledges that exact placement after the renderer applies
it. Equal rectangle values, a late callback, an old queued event, or a different
mapper do not substitute for the current identity. An old token keeps its identity
allocation alive, preventing address reuse from making it current again.

The mapper is also bound to its original `InputClient` object lifetime, not just
numeric session/lease/view identifiers. Reconstructing an input owner with equal
numbers cannot reuse the previous owner's mapper. Local layout confirmation is
not a host mapping acknowledgement, decoder completion, visible-frame proof,
source observation, ticket renewal or permission to transmit input.

## Running viewer integration

`PresentedInput::viewport` exposes only grant-bound local coordinate metadata.
The running viewer owns its mapper from construction through terminal closure.
The platform uses:

- `configure_viewport` and the returned layout for image placement;
- `confirm_viewport` for that exact renderer layout acknowledgement;
- `pointer_in_view` or `action_in_view` for stamped coordinate events.

Buttons and scrolling use `PositionedAction`. They reach the same existing
encoders, reliable action identities, atomic pointer barriers, native final
checks and result interpretation as direct desktop-coordinate input. The direct
coordinate API remains available for already-mapped callers; it is not a second
UI event queue or an alternative authority path.

The existing single pending send slot is checked before accepting another UI
event. Outside, obsolete and unconfirmed events consume no input identity or
receipt reservation. Already-encoded input is different: its original desktop
coordinates, credential, bytes and exclusive deadline survive local re-layout.
It is not re-encoded using the new zoom, renewed after backpressure, or silently
replayed. New events stamped with the retired layout refuse once the slot is free.

Layout changes do not invent button releases at a clamped edge. If a drag's release
occurs outside the image, the coordinate action refuses. The shell must supply its
actual held-key/button snapshot to the existing release-only reconciliation path,
or invoke the independent lifecycle stop when focus/visibility is lost. Snapshot
reconciliation stays usable while only the local coordinate layout is invalid.
It does not generate a pointer move or consume an unrelated action identity.

The independent `ViewerControl` fence, original media-receiver lifetime and source
freshness remain authoritative. Confirming a layout while a submitted picture
awaits its visibility callback cannot enable input. A closed receiver or stopped
session cannot be reopened by configuring or confirming another layout. Platform
callbacks must still provide genuine visibility and lifecycle evidence.

## Verification and boundaries

The first mapper increment is `03f9bd7`. Nine new core/client regressions cover
aspect-fit rounding and all exclusive edges, fractional DPI, negative monitor
origins, local zoom without host-generation changes, stale/equal/foreign layouts,
arithmetic extrema, mapping/readiness separation, rejected-event sequence
preservation and the actual wire-to-core submission path with a counted sink.

The following integration adds one input-owner lifetime regression and five
running-session tests. These negotiate real startup and four channel roles over
localhost UDP/TLS, then run the existing controlled viewer, controlled host and
canonical native owner. They verify actual decoded operation coordinates, button
press/release and scroll receipts, pending-slot priority, unchanged queued bytes
and deadlines across zoom, stale callbacks, outside-image drag reconciliation,
and independent visibility/receiver/stop fencing. Native effects are recorded by
a counted test sink; the image/layout/visibility evidence is explicitly synthetic.
There is no new X11, HEVC decoding, native GUI, live-tailnet or scanout claim.

The regrant test failed before lifetime binding was added: an equal-numeric
replacement encoded pointer sequence zero using the old mapper. It passes with
actual owner identity retained. No test deadline or authority assertion was
relaxed to obtain that result.

Local verification passes 394 core/wire/media/client tests through an isolated
four-crate Cargo workspace, plus 55 daemon unit tests built from current first-party
sources with the pinned compiler and matching retained Asupersync dependencies.
All five new runtime cases also pass with one, four and eight test threads. Strict
all-target/all-feature Clippy passes for the pure workspace; strict first-party
and daemon-unit Clippy, repository formatting and documentation checks also pass.
The attempted fresh full-workspace Cargo build stops at missing offline `rustls`
source. This local evidence is not a cold full-dependency build; any subsequent
committed-source CI result must be recorded separately against its exact commit.

The platform shell still owns real event acquisition, DPI notifications, renderer
layout application, visibility/focus and held-state sampling. No GUI library,
dependency, Asupersync pin, wire format, host approval rule or permission policy
was changed. Broader client/shell beads are not closed by this integration.

Related: [SESSION_DRIVERS.md](SESSION_DRIVERS.md),
[CONTROLLED_HOST_SESSION.md](CONTROLLED_HOST_SESSION.md),
[CONTROL_LEASE_RENEWAL.md](CONTROL_LEASE_RENEWAL.md),
[NATIVE_INPUT_ATTACHMENT.md](NATIVE_INPUT_ATTACHMENT.md),
[PROTOCOL.md](PROTOCOL.md).

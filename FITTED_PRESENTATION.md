# Explicit fitted native presentation

The Linux development client can display a larger remote desktop in a smaller
local X11 window. Scaling-capable hosts can encode a smaller picture for that
window while the original remote display and input coordinates stay unchanged:

```bash
fr connect NODE_ID --view-only --experimental-native --display choose \
  --worker /opt/fr/fr-media-worker --trust-roots /opt/fr/ca-roots.pem \
  --fit 960x540
```

`--fit WIDTHxHEIGHT` is an explicit maximum window size in **physical pixels**.
Both dimensions must be even, at least 16, and within the existing media limits.
Each actual window dimension is capped at the selected remote display dimension
and rounded down to even pixels. Without `--fit`, the local screen's usable
size (the X root, narrowed to `_NET_WORKAREA`) is the default box, so a display
larger than the screen is fitted the same way instead of refused, and one that
fits keeps its exact 1:1 window.
An explicit fit box of at least 64 pixels per axis uses the existing optional
`scaled-picture` negotiation. A host that can scale capture selects a canonical
even picture inside that box. Hosts without that capability and sessions whose
capability list has no room keep their native pictures; smaller local fit boxes
also retain native pictures. This does not grant control or auxiliary access,
and the experimental transport opt-in still applies.

The worker derives one centered image rectangle from the **original display** and
the original UI-owned window. Rendering, input and cursor positions all use that
rectangle. The decoded picture dimensions select samples only. For example,
5120x2880 fitted into 1582x888 occupies 1578x888 at x=2, even when the canonical
HEVC picture is 1582x888. Treating the encoded crop as the input rectangle would
shift clicks and the remote cursor. A 1000x750 window can likewise contain its
1000x562 picture with black bars, without treating those bars as a geometry error.

The worker uses integer nearest-neighbour CPU sampling and opaque black bars.
An existing shared source may supply a larger or smaller canonical picture;
sampling in either direction remains bounded by the original target and display.
This is an explicit quality/copy cost, not GPU or zero-copy evidence. At most one
reusable target-sized BGRA frame is retained, with actual allocation
capacity observable through `FittedFrame::retained_bytes`. It is allocated once,
never used as a queue, and borrowed only until the synchronous presentation call
finishes. When the picture already fills the target 1:1, no second frame or
full-frame sampling copy is needed; cursor coordinates still use the original
display. A changed source size is refused instead of using an obsolete layout.

`Configuration::with_fitted_window` preserves this local size policy across
reconnection, but every attempt still creates a new native window and decoder.
A checked runtime resize fences the original session and requires completed
window, decoder and input cleanup before a fresh attempt advertises the new fit
box. Old input, control consent and auxiliary selections are not replayed. Hidden
or lost windows and adverse cleanup retain their existing refusal behavior.
Zoom/pan and high-quality resampling are not implemented by this option.

For control-capable library applications, `check_fitted_layout` requires the full
original remote desktop bounds and exactly the renderer's integer fitted
rectangle. The viewport maps only that image; letterbox coordinates are outside
it. Negative remote origins, rational DPI metadata and odd letterbox remainders
do not become guessed coordinate transforms. Matching a layout neither confirms
it nor supplies visibility, fresh source evidence or input authority. All those
original checks remain necessary.

## Private worker protocol and evidence

The desktop's mapped path uses `ConfigureMappedPresentation` /
`MappedPresentationReady` (22 / 282). The bounded body carries the validated
decoder configuration, locally selected X11 target and original display width
and height. The exact echo binds all of them to the original worker epoch and
request sequence. Incompatible workers and native/fitted-only acknowledgements
refuse. Existing `ConfigureFittedPresentation` / `FittedPresentationReady`
(15 / 273) retain their legacy native-picture fitting contract, and the original
native configure kinds still require an exact native-size target. These are
private pipe kinds; the remote session wire protocol is unchanged.

Coverage includes bounded framing and incompatible modes, canonical scaled
picture validation, odd original displays, independent pixel-sampling oracles,
cursor alignment, one-buffer retention, source geometry changes, and matching
input layouts. Reconnect tests require the old owner to retire before the next
fit box can be advertised. A supervised child fixture checks the actual
launch/echo/deadline/reaping path; it never decodes HEVC. The pre-existing X11
tests exercise legacy scaling and borrowed drawable lifetime separately.

These source and regression checks do not qualify real HEVC scaled-picture
startup on X11, physical scanout, live tailnets, or end-to-end performance.
`fr-foub.1` advances the native client work in plan sections 11, 14, 15 and 16;
live scaled-host-to-Linux presentation and pointer alignment remain separate
qualification work.

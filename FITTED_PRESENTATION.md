# Explicit fitted native presentation

The Linux development client can display a larger remote desktop in a smaller,
fixed local X11 window without changing the remote display or decoder geometry:

```bash
fr connect NODE_ID --view-only --experimental-native --display choose \
  --worker /opt/fr/fr-media-worker --trust-roots /opt/fr/ca-roots.pem \
  --fit 960x540
```

`--fit WIDTHxHEIGHT` is an explicit maximum window size in **physical pixels**.
Both dimensions must be even, at least 16, and within the existing media limits.
Each actual window dimension is capped at the selected remote image dimension;
there is no upscaling. The window still has to fit the local X11 screen. Omitting
`--fit` keeps strict native-pixel presentation, including size-mismatch refusal.
This does not enable the command's still-unavailable control UI, clipboard,
audio or files, and does not change its experimental transport opt-in.

The worker scales the complete decoded image with integer nearest-neighbour CPU
sampling and centered opaque black letterbox bars. This is an explicit quality
and copy cost, not a GPU/zero-copy claim or a negotiated encoder resolution
change. One reusable target-sized BGRA frame is retained, with actual allocation
capacity observable through `FittedFrame::retained_bytes`. It is allocated once,
never used as a queue, and borrowed only until the synchronous presentation call
finishes. A changed source size is refused instead of using an obsolete layout.

`Configuration::with_fitted_window` preserves this local size policy across
reconnection, but every attempt still creates a new native window and decoder.
Resize, hidden/lost window and native event failure keep the existing stop and
cleanup behavior. Runtime resizing, zoom/pan and high-quality resampling are not
implemented by this option.

For control-capable library applications, `check_fitted_layout` requires the full
original remote desktop bounds and exactly the renderer's integer fitted
rectangle. The viewport maps only that image; letterbox coordinates are outside
it. Negative remote origins, rational DPI metadata and odd letterbox remainders
do not become guessed coordinate transforms. Matching a layout neither confirms
it nor supplies visibility, fresh source evidence or input authority. All those
original checks remain necessary.

## Private worker protocol and evidence

`ConfigureFittedPresentation` / `FittedPresentationReady` are separate private
pipe kinds (15 / 273). They carry the original validated decoder configuration
and locally selected destination. Old workers and native-only acknowledgements
refuse; the original configure kinds still require an exact native-size target.
These kinds are not additions to the remote session wire protocol.

Coverage includes bounded framing and incompatible modes, exact integer pixel
mapping, one-buffer retention, source geometry changes, CLI refusal, reconnect
ownership and matching input layouts. A supervised child fixture checks the real
launch/echo/deadline/reaping path; it never decodes HEVC. An independent X11 test
reads back every pixel from the actual production scaler and borrowed drawable,
including bars and cleanup that must not destroy the UI-owned window.

These results do not qualify real HEVC fit-mode startup, hardware acceleration,
physical scanout, live tailnets, or an end-to-end controlled desktop. They advance
the native client/rendering work in plan sections 14/15 and `fr-p1-fr-client-bis`
without closing the broader platform or input qualification gates.

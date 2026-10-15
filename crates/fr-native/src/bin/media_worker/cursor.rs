//! Cursor-only maintenance of the worker's already presented picture.
//! No codec progress, video receipt or new freshness evidence is produced.
use fr_native::{BgraFrame, CursorOverlay, NativeError, OverlayPlacement, X11Surface};

pub(super) fn repaint(
    surface: &mut X11Surface,
    overlay: &mut CursorOverlay,
    frame: &mut BgraFrame,
    placement: OverlayPlacement,
) -> Result<(), NativeError> {
    let damage = overlay.recomposite_regions(frame, placement)?;
    // Do not skip an empty list: the original drawable can have ended or
    // acquired exposure damage even when this cursor update changes no pixel.
    surface.present_regions(frame, damage.regions())
}

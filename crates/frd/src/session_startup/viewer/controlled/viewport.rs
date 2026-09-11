//! Local coordinate events use the existing single send slot and authority path.
use super::{ControlledViewer, Encoded, Error};
use fr_client::input::viewport::{Layout, Located, PositionedAction, SurfaceRect};
use fr_core::input::InputBounds;

impl ControlledViewer {
    /// Retire the old event transform immediately on resize, DPI change or pan.
    /// Already encoded bytes retain their original desktop coordinates/deadline.
    /// Release-only held-state reconciliation remains available during this gap.
    pub fn invalidate_viewport(&mut self) {
        self.viewport.invalidate();
    }
    /// Select and aspect-fit pixels within the original granted image. Use the
    /// returned source/destination rectangles for rendering; local zoom is NOT a
    /// host crop request. This does not manufacture host mapping or visibility.
    pub fn configure_viewport(
        &mut self,
        source: InputBounds,
        area: SurfaceRect,
    ) -> Result<Layout, Error> {
        self.check()?;
        self.viewport
            .configure(source, area)
            .map_err(Error::Viewport)
    }
    /// The renderer acknowledges THIS local placement, not decoded/presented
    /// pixels. A retired layout or closed session cannot be made current again.
    pub fn confirm_viewport(&mut self, layout: &Layout) -> Result<(), Error> {
        self.check()?;
        self.viewport
            .confirm_layout(layout)
            .map_err(Error::Viewport)
    }
    /// Pass an event stamped with its layout when sampled, not when delivered.
    /// A busy slot, stale layout or outside-image point consumes no input ID.
    pub fn pointer_in_view(&mut self, event: &Located) -> Result<Encoded, Error> {
        self.slot()?;
        let point = self.viewport.map(event).map_err(Error::Viewport)?;
        self.pointer(point)
    }
    /// Buttons and scrolling retain the existing reliable action positions and
    /// atomic pointer barriers. Mapping adds no queue or alternate send path.
    pub fn action_in_view(
        &mut self,
        event: &Located,
        action: PositionedAction,
    ) -> Result<Encoded, Error> {
        self.slot()?;
        let point = self.viewport.map(event).map_err(Error::Viewport)?;
        self.action(action.at(point))
    }
}

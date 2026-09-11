//! Checked local window-to-desktop transforms. Local zoom selects pixels already
//! in the granted image; it never requests a larger host capture or a new lease.
//! The renderer must use the returned placement and acknowledge that exact layout.
use crate::input::{self, Action, ClientInstant, Encoded, InputClient};
use fr_core::input::{DesktopPoint, InputBounds, InputView, PointerButton, ScrollUnit};
use fr_wire::input_result::ResultBinding;
use std::sync::Arc;

/// 1/256 of a physical window pixel. Fixed-point coordinates exclude NaN/infinity
/// and keep fractional DPI conversion separate from the image transform.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct LocalPoint {
    x: i64,
    y: i64,
}
impl LocalPoint {
    pub const fn pixels(x: i32, y: i32) -> Self {
        Self {
            x: x as i64 * 256,
            y: y as i64 * 256,
        }
    }
    pub const fn subpixels(x: i64, y: i64) -> Self {
        Self { x, y }
    }
    /// Convert toolkit logical 1/256-pixels using its actual rational DPI scale.
    /// Round down once: a negative point just outside an edge stays outside.
    pub fn logical(x: i64, y: i64, numerator: u32, denominator: u32) -> Result<Self, Error> {
        if numerator == 0 || denominator == 0 {
            return Err(Error::InvalidScale);
        }
        let convert = |value| {
            i64::try_from(
                (i128::from(value) * i128::from(numerator)).div_euclid(i128::from(denominator)),
            )
            .map_err(|_| Error::Overflow)
        };
        Ok(Self {
            x: convert(x)?,
            y: convert(y)?,
        })
    }
}

/// Half-open rectangle in physical window pixels, including toolbar offsets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SurfaceRect(InputBounds);
impl SurfaceRect {
    pub fn new(x: i32, y: i32, width: u32, height: u32) -> Result<Self, Error> {
        InputBounds::new(DesktopPoint { x, y }, width, height)
            .map(Self)
            .ok_or(Error::InvalidArea)
    }
    pub const fn origin(self) -> DesktopPoint {
        self.0.origin()
    }
    pub const fn width(self) -> u32 {
        self.0.width()
    }
    pub const fn height(self) -> u32 {
        self.0.height()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidArea,
    InvalidScale,
    OutsideGrantedDisplay,
    OutsideImage,
    Unconfirmed,
    Obsolete,
    WrongInput,
    Overflow,
    Stopped,
    Input(input::Error),
}
impl From<input::Error> for Error {
    fn from(value: input::Error) -> Self {
        Self::Input(value)
    }
}

/// Immutable placement plus a retained identity. Clones may cross the platform
/// event boundary, but equal geometry from another layout is not the same token.
/// Keeping an old token alive prevents allocation-address reuse from reviving it.
#[derive(Clone)]
pub struct Layout {
    identity: Arc<()>,
    source: InputBounds,
    destination: SurfaceRect,
}
impl Layout {
    /// The renderer samples this subrectangle of the ALREADY received image.
    /// Subtract the granted desktop origin when addressing its decoded texture.
    pub const fn source(&self) -> InputBounds {
        self.source
    }
    pub const fn destination(&self) -> SurfaceRect {
        self.destination
    }
    /// Capture the layout identity at event sampling, not at eventual dispatch.
    pub fn at(&self, point: LocalPoint) -> Located {
        Located {
            layout: self.clone(),
            point,
        }
    }
}
/// A platform event point tied to the layout that existed when it was sampled.
/// Intentionally no Debug implementation: input coordinates are not diagnostics.
#[derive(Clone)]
pub struct Located {
    layout: Layout,
    point: LocalPoint,
}
#[derive(Clone, Copy)]
pub enum PositionedAction {
    Button {
        button: PointerButton,
        pressed: bool,
    },
    Scroll {
        x: i32,
        y: i32,
        unit: ScrollUnit,
    },
}
impl PositionedAction {
    pub fn at(self, position: DesktopPoint) -> Action<'static> {
        match self {
            Self::Button { button, pressed } => Action::Button {
                button,
                pressed,
                position,
            },
            Self::Scroll { x, y, unit } => Action::Scroll {
                position,
                x,
                y,
                unit,
            },
        }
    }
}

/// One grant's local transform owner. Stores one layout, never an event queue.
/// Confirmation means the renderer applied this placement, NOT visible pixels,
/// host mapping acknowledgement, fresh source evidence or input authorization.
pub struct Viewport {
    binding: ResultBinding,
    view: InputView,
    bounds: InputBounds,
    layout: Option<Layout>,
    confirmed: bool,
    stopped: bool,
}
impl Viewport {
    /// Aspect-fit an already available source subrectangle into a window area.
    /// Integer rounding and centering are returned to the renderer, so rendering
    /// and input use the SAME rectangle, including any odd letterbox remainder.
    /// Every attempt retires the previous layout, including an invalid resize.
    pub fn configure(&mut self, source: InputBounds, area: SurfaceRect) -> Result<Layout, Error> {
        if self.stopped {
            return Err(Error::Stopped);
        }
        self.layout = None;
        self.confirmed = false;
        if !contains(self.bounds, source) {
            return Err(Error::OutsideGrantedDisplay);
        }
        let sw = u64::from(source.width());
        let sh = u64::from(source.height());
        let aw = u64::from(area.width());
        let ah = u64::from(area.height());
        let (w, h) = if aw * sh <= ah * sw {
            (aw, aw * sh / sw)
        } else {
            (ah * sw / sh, ah)
        };
        if w == 0 || h == 0 {
            return Err(Error::InvalidArea);
        }
        let x = i64::from(area.origin().x)
            + i64::try_from((aw - w) / 2).map_err(|_| Error::Overflow)?;
        let y = i64::from(area.origin().y)
            + i64::try_from((ah - h) / 2).map_err(|_| Error::Overflow)?;
        let destination = SurfaceRect::new(
            i32::try_from(x).map_err(|_| Error::Overflow)?,
            i32::try_from(y).map_err(|_| Error::Overflow)?,
            u32::try_from(w).map_err(|_| Error::Overflow)?,
            u32::try_from(h).map_err(|_| Error::Overflow)?,
        )?;
        let layout = Layout {
            identity: Arc::new(()),
            source,
            destination,
        };
        self.layout = Some(layout.clone());
        Ok(layout)
    }
    /// Immediately retire the old transform on resize, DPI change or local pan,
    /// before awaiting a new render layout. Held inputs are not synthetically
    /// released here: use the existing held-state or lifecycle cleanup owner.
    pub fn invalidate(&mut self) {
        self.layout = None;
        self.confirmed = false;
    }
    pub fn stop(&mut self) {
        self.invalidate();
        self.stopped = true;
    }
    fn current(&self, layout: &Layout) -> Result<(), Error> {
        if self.stopped {
            return Err(Error::Stopped);
        }
        if !self
            .layout
            .as_ref()
            .is_some_and(|l| Arc::ptr_eq(&l.identity, &layout.identity))
        {
            return Err(Error::Obsolete);
        }
        Ok(())
    }
    pub fn confirm_layout(&mut self, layout: &Layout) -> Result<(), Error> {
        self.current(layout)?;
        self.confirmed = true;
        Ok(())
    }
    /// Half-open hit testing; toolbar/letterbox/outside events are refused, never
    /// clamped onto a remote edge. All products fit i128 for the public bounds.
    pub fn map(&self, event: &Located) -> Result<DesktopPoint, Error> {
        self.current(&event.layout)?;
        if !self.confirmed {
            return Err(Error::Unconfirmed);
        }
        let layout = &event.layout;
        let destination = layout.destination;
        let offset_x = i128::from(event.point.x) - i128::from(destination.origin().x) * 256;
        let offset_y = i128::from(event.point.y) - i128::from(destination.origin().y) * 256;
        let width = i128::from(destination.width()) * 256;
        let height = i128::from(destination.height()) * 256;
        if offset_x < 0 || offset_y < 0 || offset_x >= width || offset_y >= height {
            return Err(Error::OutsideImage);
        }
        let position = DesktopPoint {
            x: i32::try_from(
                i128::from(layout.source.origin().x)
                    + offset_x * i128::from(layout.source.width()) / width,
            )
            .map_err(|_| Error::Overflow)?,
            y: i32::try_from(
                i128::from(layout.source.origin().y)
                    + offset_y * i128::from(layout.source.height()) / height,
            )
            .map_err(|_| Error::Overflow)?,
        };
        if !self.bounds.contains(position) {
            return Err(Error::OutsideGrantedDisplay);
        }
        Ok(position)
    }
    fn check_input(&self, input: &InputClient) -> Result<(), Error> {
        if self.binding != input.binding
            || self.view != input.credentials.view
            || self.bounds != input.bounds
        {
            return Err(Error::WrongInput);
        }
        Ok(())
    }
}
fn contains(outer: InputBounds, inner: InputBounds) -> bool {
    let x = i64::from(inner.origin().x) - i64::from(outer.origin().x);
    let y = i64::from(inner.origin().y) - i64::from(outer.origin().y);
    x >= 0
        && y >= 0
        && x + i64::from(inner.width()) <= i64::from(outer.width())
        && y + i64::from(inner.height()) <= i64::from(outer.height())
}
impl InputClient {
    /// Coordinate metadata only. Creating a transform neither confirms the host
    /// mapping nor changes this input owner's authority or freshness.
    pub fn viewport(&self) -> Viewport {
        Viewport {
            binding: self.binding,
            view: self.credentials.view,
            bounds: self.bounds,
            layout: None,
            confirmed: false,
            stopped: false,
        }
    }
    pub fn pointer_on(
        &mut self,
        viewport: &Viewport,
        event: &Located,
        out: &mut [u8],
        now: ClientInstant,
    ) -> Result<Encoded, Error> {
        self.ready(now)?;
        viewport.check_input(self)?;
        let position = viewport.map(event)?;
        Ok(self.pointer(position, out, now)?)
    }
    pub fn action_on(
        &mut self,
        viewport: &Viewport,
        event: &Located,
        action: PositionedAction,
        out: &mut [u8],
        now: ClientInstant,
    ) -> Result<Encoded, Error> {
        self.ready(now)?;
        viewport.check_input(self)?;
        let position = viewport.map(event)?;
        Ok(self.action(action.at(position), out, now)?)
    }
}

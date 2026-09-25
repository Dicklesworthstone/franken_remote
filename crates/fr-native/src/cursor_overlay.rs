//! The single client-rendered remote cursor, composited into the presented
//! BGRA image of the confined presenter. The host capture excludes the
//! pointer, so this is the only cursor drawn for the remote desktop. No
//! decode, freshness, visibility or input claim follows from compositing.
//!
//! One bounded shape copy (≤ 256×256 RGBA8) and one bounded save-under copy
//! are retained; a move restores the saved pixels instead of retaining a
//! second full frame. Straight-alpha blend, clipped to the picture area.
#![forbid(unsafe_code)]
use super::{BgraFrame, NativeError};
use fr_media::worker::{overlay::Update, presentation::Fit};

/// Mapping from decoded picture pixels into the presented image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// The picture is presented 1:1 at the origin.
    Identity,
    /// Explicit local downscale (`--fit`): the picture occupies `fit`.
    Fitted {
        fit: Fit,
        source_width: u32,
        source_height: u32,
    },
}

struct Shape {
    width: u32,
    height: u32,
    hotspot_x: i64,
    hotspot_y: i64,
    rgba: Vec<u8>,
}
#[derive(Clone, Copy, PartialEq, Eq)]
struct Rect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    // Offset of the clipped rectangle inside the cursor image.
    sx: u32,
    sy: u32,
}

/// Content-free Debug: no cursor or desktop pixels reach diagnostics.
#[derive(Default)]
pub struct CursorOverlay {
    shape: Option<Shape>,
    position: Option<(i32, i32)>,
    saved: Option<Rect>,
    under: Vec<u8>,
}
impl std::fmt::Debug for CursorOverlay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CursorOverlay")
            .field("shape", &self.shape.as_ref().map(|s| (s.width, s.height)))
            .field("visible", &self.position.is_some())
            .field("retained_bytes", &self.retained_bytes())
            .finish_non_exhaustive()
    }
}
impl CursorOverlay {
    pub fn new() -> Self {
        Self::default()
    }
    /// Owned shape and save-under allocations, both bounded by the cursor limit.
    pub fn retained_bytes(&self) -> usize {
        self.shape.as_ref().map_or(0, |s| s.rgba.capacity()) + self.under.capacity()
    }
    /// Whether a cursor would currently be composited.
    pub fn is_shown(&self) -> bool {
        self.shape.is_some() && self.position.is_some()
    }
    /// Install one validated update. Only a new shape allocates (bounded).
    pub fn apply(&mut self, update: &Update<'_>) -> Result<(), NativeError> {
        match *update {
            Update::Hidden => self.position = None,
            Update::Move { x, y } => {
                if self.shape.is_none() {
                    return Err(NativeError::InvalidConfiguration);
                }
                self.position = Some((x, y));
            }
            Update::Shape {
                x,
                y,
                width,
                height,
                hotspot_x,
                hotspot_y,
                rgba,
            } => {
                fr_media::cursor::check_geometry(width, height, hotspot_x, hotspot_y, rgba.len())
                    .map_err(|_| NativeError::InvalidConfiguration)?;
                let mut owned = self.shape.take().map(|s| s.rgba).unwrap_or_default();
                owned.clear();
                owned
                    .try_reserve_exact(rgba.len())
                    .map_err(|_| NativeError::Allocation)?;
                owned.extend_from_slice(rgba);
                self.shape = Some(Shape {
                    width: u32::from(width),
                    height: u32::from(height),
                    hotspot_x: i64::from(hotspot_x),
                    hotspot_y: i64::from(hotspot_y),
                    rgba: owned,
                });
                self.position = Some((x, y));
            }
        }
        Ok(())
    }
    /// A freshly rendered picture: saved pixels belong to the old one.
    pub fn composite_fresh(
        &mut self,
        frame: &mut BgraFrame,
        placement: Placement,
    ) -> Result<(), NativeError> {
        self.saved = None;
        self.composite(frame, placement)
    }
    /// The same retained picture: restore what the previous cursor covered,
    /// then composite the current state (or nothing, when hidden).
    pub fn recomposite(
        &mut self,
        frame: &mut BgraFrame,
        placement: Placement,
    ) -> Result<(), NativeError> {
        if let Some(rect) = self.saved.take() {
            let stride = frame_stride(frame)?;
            let row = rect.width as usize * 4;
            for line in 0..rect.height as usize {
                let dst = (rect.y as usize + line) * stride + rect.x as usize * 4;
                frame.bytes[dst..dst + row]
                    .copy_from_slice(&self.under[line * row..(line + 1) * row]);
            }
        }
        self.composite(frame, placement)
    }
    fn composite(
        &mut self,
        frame: &mut BgraFrame,
        placement: Placement,
    ) -> Result<(), NativeError> {
        let (Some(shape), Some((x, y))) = (&self.shape, self.position) else {
            return Ok(());
        };
        let Some(rect) = clip(shape, x, y, frame, placement)? else {
            return Ok(());
        };
        let stride = frame_stride(frame)?;
        let row = rect.width as usize * 4;
        self.under.clear();
        self.under
            .try_reserve_exact(row * rect.height as usize)
            .map_err(|_| NativeError::Allocation)?;
        for line in 0..rect.height as usize {
            let dst = (rect.y as usize + line) * stride + rect.x as usize * 4;
            self.under.extend_from_slice(&frame.bytes[dst..dst + row]);
            let src = ((rect.sy as usize + line) * shape.width as usize + rect.sx as usize) * 4;
            let source = &shape.rgba[src..src + row];
            for (d, s) in frame.bytes[dst..dst + row]
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(source.as_chunks::<4>().0)
            {
                // RGBA straight alpha over BGRA; destination alpha unchanged.
                let a = u32::from(s[3]);
                for (channel, value) in [(0, s[2]), (1, s[1]), (2, s[0])] {
                    d[channel] = blend(value, d[channel], a);
                }
            }
        }
        self.saved = Some(rect);
        Ok(())
    }
}
fn blend(source: u8, destination: u8, alpha: u32) -> u8 {
    let mixed = (u32::from(source) * alpha + u32::from(destination) * (255 - alpha) + 127) / 255;
    u8::try_from(mixed).expect("convex combination of bytes")
}
fn frame_stride(frame: &BgraFrame) -> Result<usize, NativeError> {
    usize::try_from(frame.width)
        .ok()
        .and_then(|w| w.checked_mul(4))
        .ok_or(NativeError::InvalidConfiguration)
}
/// Map the hotspot into the presented image and clip the cursor rectangle to
/// the picture area (never into letterbox bars). All arithmetic is i64.
fn clip(
    shape: &Shape,
    x: i32,
    y: i32,
    frame: &BgraFrame,
    placement: Placement,
) -> Result<Option<Rect>, NativeError> {
    let (x, y) = (i64::from(x), i64::from(y));
    let (area_x, area_y, area_w, area_h, mx, my) = match placement {
        Placement::Identity => (0, 0, frame.width, frame.height, x, y),
        Placement::Fitted {
            fit,
            source_width,
            source_height,
        } => {
            if source_width == 0 || source_height == 0 {
                return Err(NativeError::InvalidConfiguration);
            }
            let mx = i64::from(fit.x) + x * i64::from(fit.width) / i64::from(source_width);
            let my = i64::from(fit.y) + y * i64::from(fit.height) / i64::from(source_height);
            (fit.x, fit.y, fit.width, fit.height, mx, my)
        }
    };
    let right = i64::from(area_x) + i64::from(area_w);
    let bottom = i64::from(area_y) + i64::from(area_h);
    if right > i64::from(frame.width) || bottom > i64::from(frame.height) {
        return Err(NativeError::GeometryChanged);
    }
    let left = mx - shape.hotspot_x;
    let top = my - shape.hotspot_y;
    let x0 = left.max(i64::from(area_x));
    let y0 = top.max(i64::from(area_y));
    let x1 = (left + i64::from(shape.width)).min(right);
    let y1 = (top + i64::from(shape.height)).min(bottom);
    if x0 >= x1 || y0 >= y1 {
        return Ok(None);
    }
    let int = |v: i64| u32::try_from(v).map_err(|_| NativeError::InvalidConfiguration);
    Ok(Some(Rect {
        x: int(x0)?,
        y: int(y0)?,
        width: int(x1 - x0)?,
        height: int(y1 - y0)?,
        sx: int(x0 - left)?,
        sy: int(y0 - top)?,
    }))
}

#[cfg(test)]
#[path = "cursor_overlay/tests.rs"]
mod tests;

//! Checked placement of the confirmed host cursor over the decoded picture.
//! This is rendering state only: repainting it never acknowledges a new
//! picture, renews input authority, or predicts a host pointer position.
use crate::geometry::Rect;

pub const MAX_DIMENSION: u16 = 256;

/// Dimensions and hotspot of one admitted RGBA8 shape. No pixels in Debug.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Shape {
    pub width: u16,
    pub height: u16,
    pub hotspot_x: u16,
    pub hotspot_y: u16,
}

impl Shape {
    pub fn bytes(self) -> Option<usize> {
        if self.width == 0
            || self.height == 0
            || self.width > MAX_DIMENSION
            || self.height > MAX_DIMENSION
            || self.hotspot_x >= self.width
            || self.hotspot_y >= self.height
        {
            return None;
        }
        usize::from(self.width)
            .checked_mul(usize::from(self.height))?
            .checked_mul(4)
    }
}

/// One latest state, copied without native work while the shared owner is
/// briefly locked. Attempt and frame floor belong to its original decoder.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct State {
    pub attempt: u64,
    pub generation: u64,
    pub shape_generation: u64,
    pub frame_after: u64,
    /// Original selected display pixels, which can exceed encoded pixels.
    pub source: (u32, u32),
    pub visible: bool,
    pub x: i32,
    pub y: i32,
    pub shape: Shape,
}

impl State {
    pub fn shape_key(self) -> (u64, u64) {
        (self.attempt, self.shape_generation)
    }

    /// A cursor cannot migrate to another connection or appear on the
    /// preceding decoder's retained picture after a configuration change.
    pub fn visible_on(self, attempt: u64, frame_sequence: u64) -> bool {
        self.visible
            && attempt != 0
            && self.attempt == attempt
            && frame_sequence > self.frame_after
            && self.source.0 != 0
            && self.source.1 != 0
            && self.shape.bytes().is_some()
    }
}

/// Window-space destination and the picture clip. The bitmap is never
/// cropped by allocating another image; the GPU clips the bounded quad.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    pub destination: [f32; 4],
    pub clip: [f32; 4],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Geometry,
    Allocation,
}

/// Direct2D takes premultiplied BGRA8; the wire carries straight RGBA8.
/// Validate the complete original geometry before allocating anything.
pub fn premultiplied(shape: Shape, rgba: &[u8]) -> Result<Vec<u8>, Error> {
    let bytes = shape.bytes().ok_or(Error::Geometry)?;
    if bytes != rgba.len() {
        return Err(Error::Geometry);
    }
    let mut bgra = Vec::new();
    bgra.try_reserve_exact(bytes)
        .map_err(|_| Error::Allocation)?;
    for pixel in rgba.as_chunks::<4>().0 {
        let alpha = u16::from(pixel[3]);
        let multiply = |channel: u8| {
            // Rounded product is at most 255, including fully opaque pixels.
            u8::try_from((u16::from(channel) * alpha + 127) / 255).unwrap_or(255)
        };
        bgra.extend_from_slice(&[
            multiply(pixel[2]),
            multiply(pixel[1]),
            multiply(pixel[0]),
            pixel[3],
        ]);
    }
    Ok(bgra)
}

/// Map the confirmed hotspot and shape into exactly the video's fit. A
/// partly off-picture cursor is clipped at the picture, not the whole window.
/// Extreme coordinates cannot overflow or allocate, and invisible quads are
/// rejected before crossing into a native API.
#[allow(
    clippy::cast_possible_truncation,
    reason = "finite bounded drawing coordinates"
)]
pub fn placement(state: State, dest: Rect) -> Option<Placement> {
    state.shape.bytes()?;
    let picture = state.source;
    if picture.0 == 0 || picture.1 == 0 || dest.left >= dest.right || dest.top >= dest.bottom {
        return None;
    }
    let left = i64::from(state.x) - i64::from(state.shape.hotspot_x);
    let top = i64::from(state.y) - i64::from(state.shape.hotspot_y);
    let right = left + i64::from(state.shape.width);
    let bottom = top + i64::from(state.shape.height);
    if right <= 0 || bottom <= 0 || left >= i64::from(picture.0) || top >= i64::from(picture.1) {
        return None;
    }
    // The intersection above bounds these values to the u32 picture range
    // plus 256 pixels. f64 represents every integer in that range exactly.
    let scale_x = (f64::from(dest.right) - f64::from(dest.left)) / f64::from(picture.0);
    let scale_y = (f64::from(dest.bottom) - f64::from(dest.top)) / f64::from(picture.1);
    #[allow(
        clippy::cast_precision_loss,
        reason = "checked coordinate range below 2^53"
    )]
    let x = |value: i64| (f64::from(dest.left) + value as f64 * scale_x) as f32;
    #[allow(
        clippy::cast_precision_loss,
        reason = "checked coordinate range below 2^53"
    )]
    let y = |value: i64| (f64::from(dest.top) + value as f64 * scale_y) as f32;
    #[allow(
        clippy::cast_precision_loss,
        reason = "window coordinates converted to native drawing units"
    )]
    let clip = [
        dest.left as f32,
        dest.top as f32,
        dest.right as f32,
        dest.bottom as f32,
    ];
    Some(Placement {
        destination: [x(left), y(top), x(right), y(bottom)],
        clip,
    })
}

/// A native cursor is hidden only over this picture, and only while the
/// original controlled session actually displays a separate remote overlay.
/// Hosts whose video already contains their cursor never enter this case.
pub fn hide_local(controlling: bool, overlay_drawn: bool, dest: Rect, point: (i32, i32)) -> bool {
    controlling
        && overlay_drawn
        && point.0 >= dest.left
        && point.1 >= dest.top
        && point.0 < dest.right
        && point.1 < dest.bottom
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::fit;

    fn state() -> State {
        State {
            attempt: 17,
            generation: 3,
            shape_generation: 2,
            frame_after: 8,
            source: (100, 80),
            visible: true,
            x: 12,
            y: 15,
            shape: Shape {
                width: 4,
                height: 6,
                hotspot_x: 1,
                hotspot_y: 2,
            },
        }
    }

    #[test]
    fn transparent_colours_cannot_create_halos_and_opaque_channels_are_bgra() {
        let shape = Shape {
            width: 4,
            height: 1,
            ..Shape::default()
        };
        let pixels = [
            255, 16, 8, 0, 200, 100, 50, 128, 1, 2, 3, 255, 255, 255, 255, 1,
        ];
        assert_eq!(
            premultiplied(shape, &pixels).unwrap(),
            [0, 0, 0, 0, 25, 50, 100, 128, 3, 2, 1, 255, 1, 1, 1, 1]
        );
    }

    #[test]
    fn hostile_shapes_refuse_before_allocating_or_accepting_a_partial_image() {
        let valid = state().shape;
        for shape in [
            Shape { width: 0, ..valid },
            Shape {
                height: MAX_DIMENSION + 1,
                ..valid
            },
            Shape {
                hotspot_x: valid.width,
                ..valid
            },
            Shape {
                hotspot_y: valid.height,
                ..valid
            },
        ] {
            assert_eq!(premultiplied(shape, &[]), Err(Error::Geometry));
        }
        assert_eq!(premultiplied(valid, &[0; 95]), Err(Error::Geometry));
        assert_eq!(premultiplied(valid, &[0; 97]), Err(Error::Geometry));
        assert_eq!(premultiplied(valid, &[0; 96]).unwrap().len(), 96);
    }

    #[test]
    fn hotspot_follows_the_same_native_pixel_and_scaled_picture_mapping() {
        let state = state();
        let one_to_one = fit((100, 80), (200, 160)).unwrap();
        assert_eq!(
            placement(state, one_to_one),
            Some(Placement {
                destination: [61.0, 53.0, 65.0, 59.0],
                clip: [50.0, 40.0, 150.0, 120.0],
            })
        );
        let scaled = fit((100, 80), (50, 50)).unwrap();
        assert_eq!(
            placement(state, scaled),
            Some(Placement {
                destination: [5.5, 11.5, 7.5, 14.5],
                clip: [0.0, 5.0, 50.0, 45.0],
            })
        );
    }

    #[test]
    fn edge_clipping_preserves_partial_shape_and_never_draws_into_letterboxes() {
        let dest = fit((100, 80), (200, 160)).unwrap();
        let edge = State {
            x: 0,
            y: 0,
            ..state()
        };
        let placed = placement(edge, dest).unwrap();
        assert_eq!(placed.destination, [49.0, 38.0, 53.0, 44.0]);
        assert_eq!(placed.clip, [50.0, 40.0, 150.0, 120.0]);
        for (x, y) in [
            (i32::MIN, 0),
            (i32::MAX, 0),
            (0, i32::MIN),
            (0, i32::MAX),
            (-4, 0),
        ] {
            assert!(placement(State { x, y, ..state() }, dest).is_none());
        }
        assert!(
            placement(
                State {
                    source: (0, 80),
                    ..edge
                },
                dest
            )
            .is_none()
        );
    }

    #[test]
    fn host_encoding_resize_keeps_the_cursor_in_original_display_coordinates() {
        let dest = fit((100, 80), (200, 160)).unwrap();
        let remote = State {
            x: 120,
            y: 140,
            source: (200, 160),
            ..state()
        };
        // These source coordinates exceed the encoded image's width/height.
        // They must still land at the corresponding point in that picture.
        assert_eq!(
            placement(remote, dest),
            Some(Placement {
                destination: [109.5, 109.0, 111.5, 112.0],
                clip: [50.0, 40.0, 150.0, 120.0],
            })
        );
    }

    #[test]
    fn reconnect_and_decoder_reconfiguration_cannot_reuse_a_cursor_on_old_pixels() {
        let state = state();
        assert!(state.visible_on(17, 9));
        assert!(!state.visible_on(17, 8));
        assert!(!state.visible_on(18, 9));
        assert!(
            !State {
                attempt: 0,
                ..state
            }
            .visible_on(0, 9)
        );
        assert!(
            !State {
                visible: false,
                ..state
            }
            .visible_on(17, 9)
        );
        let moved = State {
            generation: 4,
            x: 30,
            ..state
        };
        assert_eq!(moved.shape_key(), state.shape_key());
        assert_ne!(
            State {
                attempt: 18,
                ..state
            }
            .shape_key(),
            state.shape_key()
        );
    }

    #[test]
    fn a_control_overlay_hides_only_the_local_pointer_over_the_original_picture() {
        let dest = fit((100, 80), (200, 160)).unwrap();
        assert!(hide_local(true, true, dest, (50, 40)));
        assert!(!hide_local(false, true, dest, (50, 40)));
        assert!(!hide_local(true, false, dest, (50, 40)));
        for point in [(49, 40), (150, 40), (50, 39), (50, 120)] {
            assert!(!hide_local(true, true, dest, point));
        }
    }
}

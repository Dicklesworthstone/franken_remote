//! Bounded native cursor pixels in the same scale as the presented picture.
//! Native cursors use integral dimensions/hotspots. Scaling never changes an
//! input coordinate or moves the local pointer.
use crate::{
    cursor::{Error, Shape},
    geometry::Rect,
};

/// Top-down premultiplied BGRA and a top-down, DWORD-aligned 1bpp AND mask.
/// All bytes, including mask padding, are initialized before entering GDI.
pub struct Image {
    pub shape: Shape,
    pub bgra: Vec<u8>,
    pub mask: Vec<u8>,
}

fn zeroes(bytes: usize) -> Result<Vec<u8>, Error> {
    let mut result = Vec::new();
    result
        .try_reserve_exact(bytes)
        .map_err(|_| Error::Allocation)?;
    result.resize(bytes, 0);
    Ok(result)
}

/// Validate the complete source before allocating. The video never upscales;
/// the cursor obeys the same rule and remains within its original 256px cap.
pub fn prepare(shape: Shape, rgba: &[u8], source: (u32, u32), dest: Rect) -> Result<Image, Error> {
    if shape.bytes() != Some(rgba.len()) || source.0 == 0 || source.1 == 0 {
        return Err(Error::Geometry);
    }
    let width = dest
        .right
        .checked_sub(dest.left)
        .filter(|value| *value > 0)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or(Error::Geometry)?;
    let height = dest
        .bottom
        .checked_sub(dest.top)
        .filter(|value| *value > 0)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or(Error::Geometry)?;
    if width > source.0 || height > source.1 {
        return Err(Error::Geometry);
    }
    let scaled = |value: u16, target: u32, original: u32| {
        (u64::from(value) * u64::from(target) + u64::from(original) / 2) / u64::from(original)
    };
    let w =
        u16::try_from(scaled(shape.width, width, source.0).max(1)).map_err(|_| Error::Geometry)?;
    let h = u16::try_from(scaled(shape.height, height, source.1).max(1))
        .map_err(|_| Error::Geometry)?;
    let output = Shape {
        width: w,
        height: h,
        hotspot_x: u16::try_from(scaled(shape.hotspot_x, width, source.0))
            .map_err(|_| Error::Geometry)?
            .min(w - 1),
        hotspot_y: u16::try_from(scaled(shape.hotspot_y, height, source.1))
            .map_err(|_| Error::Geometry)?
            .min(h - 1),
    };
    let mut bgra = zeroes(output.bytes().ok_or(Error::Geometry)?)?;
    let mask_stride = usize::from(w).div_ceil(32) * 4;
    let mut mask = zeroes(mask_stride * usize::from(h))?;
    for y in 0..usize::from(h) {
        let sy = ((2 * y + 1) * usize::from(shape.height) / (2 * usize::from(h)))
            .min(usize::from(shape.height) - 1);
        for x in 0..usize::from(w) {
            let sx = ((2 * x + 1) * usize::from(shape.width) / (2 * usize::from(w)))
                .min(usize::from(shape.width) - 1);
            let input = (sy * usize::from(shape.width) + sx) * 4;
            let output = (y * usize::from(w) + x) * 4;
            let pixel = &rgba[input..input + 4];
            let alpha = u16::from(pixel[3]);
            let premultiply =
                |channel: u8| u8::try_from((u16::from(channel) * alpha + 127) / 255).unwrap_or(255);
            bgra[output..output + 4].copy_from_slice(&[
                premultiply(pixel[2]),
                premultiply(pixel[1]),
                premultiply(pixel[0]),
                pixel[3],
            ]);
            if alpha == 0 {
                mask[y * mask_stride + x / 8] |= 0x80 >> (x % 8);
            }
        }
    }
    Ok(Image {
        shape: output,
        bgra,
        mask,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(width: i32, height: i32) -> Rect {
        Rect {
            left: 10,
            top: 20,
            right: 10 + width,
            bottom: 20 + height,
        }
    }

    #[test]
    fn native_pixels_preserve_alpha_hotspot_and_mask_padding() {
        let shape = Shape {
            width: 3,
            height: 1,
            hotspot_x: 2,
            hotspot_y: 0,
        };
        let image = prepare(
            shape,
            &[255, 0, 255, 255, 255, 255, 255, 128, 50, 90, 130, 0],
            (800, 600),
            rect(800, 600),
        )
        .unwrap();
        assert_eq!(image.shape, shape);
        assert_eq!(
            image.bgra,
            [255, 0, 255, 255, 128, 128, 128, 128, 0, 0, 0, 0]
        );
        assert_eq!(image.mask, [0x20, 0, 0, 0]);
    }

    #[test]
    fn scale_uses_original_display_including_host_encoded_resize() {
        let shape = Shape {
            width: 4,
            height: 4,
            hotspot_x: 3,
            hotspot_y: 3,
        };
        let rgba: Vec<_> = (0..16_u8).flat_map(|value| [value, 0, 0, 255]).collect();
        let image = prepare(shape, &rgba, (200, 160), rect(100, 80)).unwrap();
        assert_eq!(
            image.shape,
            Shape {
                width: 2,
                height: 2,
                hotspot_x: 1,
                hotspot_y: 1
            }
        );
        assert_eq!(
            image.bgra,
            [0, 0, 5, 255, 0, 0, 7, 255, 0, 0, 13, 255, 0, 0, 15, 255]
        );
        assert_eq!(image.mask, [0; 8]);
    }

    #[test]
    fn tiny_fit_retains_one_pixel_and_a_valid_hotspot() {
        let shape = Shape {
            width: 2,
            height: 2,
            hotspot_x: 1,
            hotspot_y: 1,
        };
        let image = prepare(shape, &[0; 16], (10_000, 10_000), rect(1, 1)).unwrap();
        assert_eq!(
            image.shape,
            Shape {
                width: 1,
                height: 1,
                hotspot_x: 0,
                hotspot_y: 0
            }
        );
        assert_eq!(image.mask, [0x80, 0, 0, 0]);
    }

    #[test]
    fn native_preparation_refuses_invalid_and_upscaled_geometry() {
        let shape = Shape {
            width: 1,
            height: 1,
            hotspot_x: 0,
            hotspot_y: 0,
        };
        for (source, dest, rgba) in [
            ((0, 20), rect(10, 10), &[0; 4][..]),
            ((20, 20), rect(0, 10), &[0; 4][..]),
            ((20, 20), rect(10, 10), &[0; 3][..]),
            ((20, 20), rect(21, 10), &[0; 4][..]),
            (
                (20, 20),
                Rect {
                    left: i32::MIN,
                    top: 0,
                    right: i32::MAX,
                    bottom: 10,
                },
                &[0; 4][..],
            ),
        ] {
            assert!(matches!(
                prepare(shape, rgba, source, dest),
                Err(Error::Geometry)
            ));
        }
    }
}

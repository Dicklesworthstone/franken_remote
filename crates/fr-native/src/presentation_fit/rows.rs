//! CPU fitting kernels for exact half-size and nearest-neighbour presentation.
//! Only bounded CPU slices cross this boundary; there is no FFI or allocation.
#![forbid(unsafe_code)]

/// Each target pixel covers exactly four source pixels. Average them in the
/// decoded BGRA representation so a one-pixel stroke survives either phase.
pub(super) fn halve_row(upper: &[[u8; 4]], lower: &[[u8; 4]], target: &mut [[u8; 4]]) {
    let upper = upper.as_chunks::<2>().0;
    let lower = lower.as_chunks::<2>().0;
    debug_assert_eq!(upper.len(), target.len());
    debug_assert_eq!(lower.len(), target.len());
    for ((top, bottom), pixel) in upper.iter().zip(lower).zip(target) {
        for (channel, value) in pixel.iter_mut().enumerate() {
            let sum = u16::from(top[0][channel])
                + u16::from(top[1][channel])
                + u16::from(bottom[0][channel])
                + u16::from(bottom[1][channel]);
            *value = u8::try_from((sum + 2) / 4).expect("mean of four bytes fits one byte");
        }
    }
}

pub(super) fn scale_row(source: &[[u8; 4]], target: &mut [[u8; 4]]) {
    let width = target.len();
    // Both buffers were bounded before allocation. Shared-source pictures may
    // be smaller than the original display-space rectangle they must fill.
    assert!(width != 0 && !source.is_empty());
    if width == source.len() {
        target.copy_from_slice(source);
        return;
    }
    let whole = source.len() / width;
    let remainder = source.len() % width;
    if remainder == 0 {
        // Integer downscales (notably 4K -> 1080p) select the first pixel of
        // each exact group. No carry or per-pixel bounds check is needed.
        for (group, pixel) in source.chunks_exact(whole).zip(target) {
            *pixel = group[0];
        }
        return;
    }
    let mut index = 0;
    let mut phase = 0;
    for pixel in target {
        *pixel = source[index];
        index += whole;
        // Bresenham-style carry: before pixel x, index=floor(x*S/W) and
        // phase=(x*S)%W. Subtract before adding to avoid intermediate overflow.
        if phase >= width - remainder {
            phase -= width - remainder;
            index += 1;
        } else {
            phase += remainder;
        }
    }
}

#[cfg(test)]
#[path = "rows/tests.rs"]
mod tests;

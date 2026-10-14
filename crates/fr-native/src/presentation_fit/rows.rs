//! Exact `floor(x * source_width / target_width)` without per-pixel division.
//! Only bounded CPU slices cross this boundary; there is no FFI or allocation.
#![forbid(unsafe_code)]

pub(super) fn scale_row(source: &[[u8; 4]], target: &mut [[u8; 4]]) {
    let width = target.len();
    // The caller has already validated a nonempty, downwards-only Fit.
    assert!(width != 0 && width <= source.len());
    if width == source.len() {
        target.copy_from_slice(source);
        return;
    }
    let whole = source.len() / width;
    let remainder = source.len() % width;
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

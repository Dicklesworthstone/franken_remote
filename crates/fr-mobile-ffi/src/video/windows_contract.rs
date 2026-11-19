//! Bounds and ownership checks used by the actual Media Foundation decoder.
//!
//! Keep these checks independent of COM so hostile sizes and delayed output
//! can also be exercised on the portable test runner.

/// Bound native capability enumeration as well as the buffers it describes.
#[cfg(windows)]
pub(crate) const MAX_OUTPUT_TYPES: u32 = 32;
#[cfg(windows)]
pub(crate) const MAX_TRANSFORMS: u32 = 32;

/// A decoder may pad NV12 rows to 512 bytes and surface heights to 64 lines.
/// Larger padding requires a different, explicitly budgeted backend contract.
const ROW_ALIGNMENT: u32 = 512;
const HEIGHT_ALIGNMENT: u32 = 64;
/// HEVC's admitted DPB ceiling, also applied to a returned texture array.
const MAX_SURFACES: u32 = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Allocation {
    Decoder,
    Caller { bytes: u32, alignment_mask: u32 },
}

/// One configured picture, including a bounded allowance for native padding.
#[derive(Debug, Clone, Copy)]
pub(crate) struct OutputBounds {
    coded: (u32, u32),
    visible: (u32, u32),
    max_stride: u32,
    max_height: u32,
    max_bytes: u32,
}

impl OutputBounds {
    /// The caller has already admitted the coded/crop geometry against the
    /// negotiated protocol limits. Recheck arithmetic before native use.
    pub(crate) fn new(coded: (u32, u32), visible: (u32, u32)) -> Option<Self> {
        if coded.0 == 0
            || coded.1 == 0
            || visible.0 == 0
            || visible.1 == 0
            || visible.0 > coded.0
            || visible.1 > coded.1
            || !coded.0.is_multiple_of(2)
            || !coded.1.is_multiple_of(2)
            || !visible.0.is_multiple_of(2)
            || !visible.1.is_multiple_of(2)
        {
            return None;
        }
        let max_stride = align(coded.0, ROW_ALIGNMENT)?;
        let max_height = align(coded.1, HEIGHT_ALIGNMENT)?;
        let max_bytes = nv12_bytes(max_stride, max_height)?;
        Some(Self {
            coded,
            visible,
            max_stride,
            max_height,
            max_bytes,
        })
    }

    /// MF may describe either the coded image or its visible crop. Other
    /// dimensions are a configuration change, not a new allocation request.
    /// Padding belongs to the stride/texture, not to a new visible geometry.
    pub(crate) fn layout(&self, width: u32, height: u32, stride: Option<u32>) -> Option<u32> {
        if (width, height) != self.coded && (width, height) != self.visible {
            return None;
        }
        let stride = stride.unwrap_or(width);
        if stride < width || stride > self.max_stride || !stride.is_multiple_of(2) {
            return None;
        }
        let bytes = nv12_bytes(stride, height)?;
        (bytes <= self.max_bytes).then_some(bytes)
    }

    /// `cbAlignment` is an alignment in bytes; `MFCreateAlignedMemoryBuffer`
    /// takes an alignment *mask*. Zero means no special alignment.
    pub(crate) fn allocation(
        &self,
        minimum_bytes: u32,
        bytes: u32,
        alignment: u32,
        provides_samples: bool,
    ) -> Option<Allocation> {
        let alignment = alignment.max(1);
        if !alignment.is_power_of_two()
            || alignment > ROW_ALIGNMENT
            || minimum_bytes == 0
            || minimum_bytes > self.max_bytes
            || bytes > self.max_bytes
        {
            return None;
        }
        if provides_samples {
            Some(Allocation::Decoder)
        } else if bytes >= minimum_bytes {
            Some(Allocation::Caller {
                bytes,
                alignment_mask: alignment - 1,
            })
        } else {
            None
        }
    }

    pub(crate) fn memory(&self, minimum_bytes: u32, length: u32, capacity: u32) -> bool {
        minimum_bytes > 0
            && minimum_bytes <= length
            && length <= capacity
            && capacity <= self.max_bytes
    }

    /// D3D11 may expose an array holding the decoder's reference pictures.
    /// Only one NV12 mip and one valid array slice may be presented.
    pub(crate) fn texture(&self, width: u32, height: u32, slices: u32, slice: u32) -> bool {
        width >= self.visible.0
            && height >= self.visible.1
            && width <= self.max_stride
            && height <= self.max_height
            && width.is_multiple_of(2)
            && height.is_multiple_of(2)
            && (1..=MAX_SURFACES).contains(&slices)
            && slice < slices
    }
}

fn align(value: u32, alignment: u32) -> Option<u32> {
    value
        .checked_add(alignment - 1)
        .map(|value| value & !(alignment - 1))
}

fn nv12_bytes(stride: u32, height: u32) -> Option<u32> {
    let luma = stride.checked_mul(height)?;
    luma.checked_add(luma / 2)
}

/// The diagnostic luma reader must fit every requested row, including any
/// native pitch padding, inside the locked memory buffer.
pub(crate) fn readable_luma(width: u32, height: u32, pitch: u32, capacity: u32) -> bool {
    width != 0
        && height != 0
        && pitch >= width
        && (height - 1)
            .checked_mul(pitch)
            .and_then(|offset| offset.checked_add(width))
            .is_some_and(|end| end <= capacity)
}

/// The original frame id is a unique, nonnegative COM correlation token.
/// It is not a capture clock or a new freshness deadline. Capture timestamps
/// can repeat and must not identify ownership of a returned picture.
pub(crate) fn sample_time(frame: u64) -> Option<i64> {
    i64::try_from(frame).ok()
}

/// A synchronous, no-reorder decode must return this input's one picture.
/// Success with no sample, only events, an incomplete output, or a sample
/// belonging to an earlier input must never advance the reference chain.
pub(crate) fn owns_picture(
    expected_time: i64,
    output_time: Option<i64>,
    stream_status: u32,
    process_status: u32,
    has_events: bool,
) -> bool {
    expected_time >= 0
        && output_time == Some(expected_time)
        && stream_status == 0
        && process_status == 0
        && !has_events
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds() -> OutputBounds {
        OutputBounds::new((1920, 1088), (1920, 1080)).expect("admitted geometry")
    }

    #[test]
    fn a_delayed_picture_cannot_become_the_next_inputs_picture() {
        let first = sample_time(41).unwrap();
        let next = sample_time(42).unwrap();
        assert!(owns_picture(first, Some(first), 0, 0, false));
        assert!(!owns_picture(next, Some(first), 0, 0, false));
        assert!(!owns_picture(first, Some(next), 0, 0, false));
        assert!(!owns_picture(next, None, 0, 0, false));
        assert!(owns_picture(next, Some(next), 0, 0, false));
    }

    #[test]
    fn frame_tokens_never_saturate_or_alias() {
        assert_eq!(sample_time(0), Some(0));
        assert_eq!(sample_time(i64::MAX as u64), Some(i64::MAX));
        assert_eq!(sample_time(i64::MAX as u64 + 1), None);
        assert_eq!(sample_time(u64::MAX), None);
        assert!(!owns_picture(-1, Some(-1), 0, 0, false));
    }

    #[test]
    fn successful_com_status_alone_is_not_a_picture() {
        // MF's FORMAT_CHANGE, STREAM_END, NO_SAMPLE and INCOMPLETE values.
        for status in [0x100, 0x200, 0x300, 0x0100_0000, 0x0100_0300, u32::MAX] {
            assert!(!owns_picture(7, Some(7), status, 0, false));
        }
        assert!(!owns_picture(7, Some(7), 0, 1, false));
        assert!(!owns_picture(7, Some(7), 0, 0, true));
    }

    #[test]
    fn layout_admits_crop_and_coded_geometry_but_not_a_resize() {
        let bounds = bounds();
        assert_eq!(bounds.layout(1920, 1080, None), Some(3_110_400));
        assert_eq!(bounds.layout(1920, 1088, Some(2048)), Some(3_342_336));
        assert_eq!(bounds.layout(1280, 720, None), None);
        assert_eq!(bounds.layout(1920, 1082, None), None);
        assert_eq!(bounds.layout(1920, 1080, Some(1918)), None);
        assert_eq!(bounds.layout(1920, 1080, Some(2049)), None);
        assert_eq!(bounds.layout(1920, 1080, Some(u32::MAX)), None);
    }

    #[test]
    fn native_allocation_is_bounded_before_it_is_requested() {
        let bounds = bounds();
        let minimum = bounds.layout(1920, 1080, None).unwrap();
        assert_eq!(
            bounds.allocation(minimum, 3_342_336, 512, false),
            Some(Allocation::Caller {
                bytes: 3_342_336,
                alignment_mask: 511
            })
        );
        assert_eq!(bounds.allocation(minimum, 3_342_337, 1, false), None);
        assert_eq!(bounds.allocation(minimum, u32::MAX, 1, false), None);
        assert_eq!(bounds.allocation(minimum, minimum - 1, 1, false), None);
        assert_eq!(bounds.allocation(minimum, 0, 1, false), None);
        assert_eq!(
            bounds.allocation(minimum, 0, 0, true),
            Some(Allocation::Decoder)
        );
        assert_eq!(bounds.allocation(minimum, u32::MAX, 0, true), None);
    }

    #[test]
    fn alignment_is_checked_and_converted_from_bytes_to_mask() {
        let bounds = bounds();
        let minimum = bounds.layout(1920, 1080, None).unwrap();
        for alignment in [0, 1, 2, 4, 8, 16, 32, 64, 128, 256, 512] {
            assert_eq!(
                bounds.allocation(minimum, minimum, alignment, false),
                Some(Allocation::Caller {
                    bytes: minimum,
                    alignment_mask: alignment.max(1) - 1
                })
            );
        }
        for alignment in [3, 511, 513, 1024, u32::MAX] {
            assert_eq!(bounds.allocation(minimum, minimum, alignment, false), None);
        }
    }

    #[test]
    fn decoder_supplied_memory_cannot_bypass_the_budget() {
        let bounds = bounds();
        let minimum = bounds.layout(1920, 1080, None).unwrap();
        assert!(bounds.memory(minimum, minimum, 3_342_336));
        assert!(!bounds.memory(minimum, minimum - 1, minimum));
        assert!(!bounds.memory(minimum, 3_342_336, minimum));
        assert!(!bounds.memory(minimum, minimum, 3_342_337));
    }

    #[test]
    fn texture_padding_arrays_and_slice_indices_are_bounded() {
        let bounds = bounds();
        assert!(bounds.texture(1920, 1080, 1, 0));
        assert!(bounds.texture(2048, 1088, 16, 15));
        assert!(!bounds.texture(2048, 1088, 16, 16));
        assert!(!bounds.texture(2048, 1088, 17, 0));
        assert!(!bounds.texture(2048, 1088, 0, 0));
        assert!(!bounds.texture(2050, 1088, 1, 0));
        assert!(!bounds.texture(2048, 1090, 1, 0));
        assert!(!bounds.texture(1918, 1080, 1, 0));
    }

    #[test]
    fn padding_and_plane_arithmetic_cannot_wrap() {
        assert!(OutputBounds::new((128, 96), (128, 96)).is_some());
        assert!(OutputBounds::new((0, 96), (128, 96)).is_none());
        assert!(OutputBounds::new((128, 96), (130, 96)).is_none());
        assert!(OutputBounds::new((128, 96), (127, 96)).is_none());
        assert!(OutputBounds::new((u32::MAX - 1, 96), (128, 96)).is_none());
        assert!(OutputBounds::new((65_536, 65_536), (128, 96)).is_none());
    }

    #[test]
    fn diagnostic_rows_must_fit_the_actual_memory_buffer() {
        assert!(readable_luma(128, 96, 256, 24_448));
        assert!(!readable_luma(128, 96, 256, 24_447));
        assert!(!readable_luma(128, 96, 127, u32::MAX));
        assert!(!readable_luma(128, 96, u32::MAX, u32::MAX));
        assert!(!readable_luma(128, 0, 256, 24_448));
        assert!(!readable_luma(0, 96, 256, 24_448));
    }
}

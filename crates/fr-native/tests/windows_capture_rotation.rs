//! Production geometry/resource admission and source-evidence policy. These
//! portable tests do not execute D3D or qualify a rotated physical monitor.
#![forbid(unsafe_code)]

#[path = "../src/windows_capture/freshness.rs"]
mod freshness;
#[path = "../src/windows_capture/rotation.rs"]
mod rotation;

use freshness::{Capture, Frame, Observation, Source};
use rotation::{Layout, Mismatch, Rotation, Texture};

fn layout(size: (u32, u32), degrees: u32) -> Layout {
    Layout::new(size, degrees, 8192, 16_777_216).unwrap()
}

fn texture(size: (u32, u32)) -> Texture {
    Texture {
        size,
        bgra: true,
        mip_levels: 1,
        array_size: 1,
        sample_count: 1,
        sample_quality: 0,
    }
}

#[test]
fn all_orientations_keep_catalog_and_input_dimensions_upright() {
    for (degrees, size, acquired, rotation, quarter_turns) in [
        (0, (1920, 1080), (1920, 1080), Rotation::Identity, 0),
        (90, (1080, 1920), (1920, 1080), Rotation::Clockwise90, 1),
        (180, (1920, 1080), (1920, 1080), Rotation::Clockwise180, 2),
        (270, (1080, 1920), (1920, 1080), Rotation::Clockwise270, 3),
    ] {
        let layout = layout(size, degrees);
        assert_eq!(layout.upright(), size);
        assert_eq!(layout.acquired(), acquired);
        assert_eq!(layout.rotation(), rotation);
        assert_eq!(layout.rotation().quarter_turns(), quarter_turns);
        assert!(layout.matches_mode(size, rotation));
        assert_eq!(layout.check_texture(texture(acquired)), Ok(()));
    }
}

#[test]
fn raw_portrait_texture_is_swapped_but_duplication_mode_is_not() {
    let layout = layout((768, 1024), 90);
    assert!(layout.matches_mode((768, 1024), Rotation::Clockwise90));
    assert!(!layout.matches_mode((1024, 768), Rotation::Clockwise90));
    assert_eq!(layout.check_texture(texture((1024, 768))), Ok(()));
    assert_eq!(
        layout.check_texture(texture((768, 1024))),
        Err(Mismatch::Geometry)
    );
}

#[test]
fn a_flip_cannot_reuse_a_same_sized_capture_owner() {
    for size in [(1920, 1080), (1080, 1920), (1024, 1024)] {
        for original in [0, 90, 180, 270] {
            let layout = layout(size, original);
            for replacement in [0, 90, 180, 270] {
                assert_eq!(
                    layout.matches_mode(size, Rotation::from_degrees(replacement).unwrap()),
                    original == replacement
                );
            }
            assert!(!layout.matches_mode((size.0 - 1, size.1), layout.rotation()));
            assert!(!layout.matches_mode((size.0, size.1 + 1), layout.rotation()));
        }
    }
}

#[test]
fn rotated_and_identity_allocations_share_the_original_byte_and_axis_bounds() {
    for degrees in [0, 90, 180, 270] {
        assert!(Layout::new((4096, 4096), degrees, 8192, 16_777_216).is_some());
        assert!(Layout::new((4096, 4097), degrees, 8192, 16_777_216).is_none());
        assert!(Layout::new((8193, 1), degrees, 8192, 16_777_216).is_none());
        assert!(Layout::new((1, 8193), degrees, 8192, 16_777_216).is_none());
        assert!(Layout::new((1080, 1920), degrees, 1920, 2_073_600).is_some());
        assert!(Layout::new((1080, 1920), degrees, 1920, 2_073_599).is_none());
        assert!(Layout::new((0, 1), degrees, 8192, 16_777_216).is_none());
        assert!(Layout::new((1, 0), degrees, 8192, 16_777_216).is_none());
        assert!(Layout::new((1, 1), degrees, 0, 16_777_216).is_none());
        assert!(Layout::new((1, 1), degrees, 8192, 0).is_none());
    }
}

#[test]
fn invalid_rotations_and_unsigned_native_rectangles_are_refused() {
    for degrees in [1, 89, 91, 179, 181, 269, 271, 360, u32::MAX] {
        assert!(Rotation::from_degrees(degrees).is_none());
        assert!(Layout::new((1920, 1080), degrees, 8192, 16_777_216).is_none());
    }
    for size in [(u32::MAX, 1), (1, u32::MAX), (u32::MAX, u32::MAX)] {
        assert!(Layout::new(size, 90, u32::MAX, u64::MAX).is_none());
    }
}

#[test]
fn unexpected_format_mips_arrays_and_multisampling_never_reach_gpu_work() {
    let layout = layout((1080, 1920), 270);
    let original = texture(layout.acquired());
    for changed in [
        Texture {
            bgra: false,
            ..original
        },
        Texture {
            mip_levels: 0,
            ..original
        },
        Texture {
            mip_levels: 2,
            ..original
        },
        Texture {
            array_size: 0,
            ..original
        },
        Texture {
            array_size: 2,
            ..original
        },
        Texture {
            sample_count: 0,
            ..original
        },
        Texture {
            sample_count: 2,
            ..original
        },
        Texture {
            sample_quality: 1,
            ..original
        },
    ] {
        assert_eq!(layout.check_texture(changed), Err(Mismatch::Resource));
    }
    for size in [(0, 0), (1921, 1080), (1920, 1079), (u32::MAX, u32::MAX)] {
        assert_eq!(layout.check_texture(texture(size)), Err(Mismatch::Geometry));
    }
}

#[test]
fn failed_rotation_or_resource_admission_invalidates_old_pixels() {
    let mut source = Source::default();
    assert_eq!(source.released(Frame::Image, true), Observation::Image);
    assert!(source.has_image());
    assert_eq!(source.released(Frame::Failed, true), Observation::Unknown);
    assert!(!source.has_image());
    assert_eq!(source.unchanged().capture(true), Capture::Refuse);
    assert_eq!(
        source.released(Frame::PointerOnly, true),
        Observation::Unknown
    );
    assert_eq!(
        source.released(Frame::Image, true).capture(false),
        Capture::Encode
    );
}

#[test]
fn rotated_image_requires_successful_release_before_becoming_source_evidence() {
    let mut source = Source::default();
    source.released(Frame::Image, true);
    assert_eq!(source.released(Frame::Image, false), Observation::Unknown);
    assert_eq!(source.unchanged().capture(true), Capture::Refuse);
    assert_eq!(
        source.released(Frame::PointerOnly, true),
        Observation::Unknown
    );
    assert_eq!(source.released(Frame::Image, true), Observation::Image);
    assert_eq!(source.unchanged().capture(false), Capture::Unchanged);
}

#[test]
fn replacement_rotation_cannot_revive_cached_picture_or_pointer_evidence() {
    let original = layout((1024, 1024), 90);
    let mut source = Source::default();
    source.released(Frame::Image, true);
    // This is the production recreation order: invalidate first, then
    // compare the new mode with the selected owner's original layout.
    source.invalidate();
    assert!(!original.matches_mode((1024, 1024), Rotation::Clockwise270));
    assert_eq!(source.unchanged(), Observation::Unknown);
    assert_eq!(
        source.released(Frame::PointerOnly, true),
        Observation::Unknown
    );
    assert_eq!(source.unchanged().capture(true), Capture::Refuse);
}

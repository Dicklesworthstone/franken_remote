#![cfg(all(target_os = "linux", feature = "linux-media"))]
//! Real X11 region updates on shared-memory and socket paths.
//! Native completion is not optical visibility, freshness or input authority.
#[allow(dead_code)]
#[path = "presentation_support/mod.rs"]
mod support;
use fr_core::limits::ProtocolLimits;
use fr_native::{BgraFrame, NativeError, X11Surface, image_transfer::Path};
use support::{Server, picture};

fn expected(base: &BgraFrame, source: &BgraFrame, regions: &[[u32; 4]]) -> Vec<u8> {
    let mut pixels = base.pixels().to_vec();
    let stride = base.width() as usize * 4;
    for &[x, y, w, h] in regions {
        for line in y..y + h {
            let start = line as usize * stride + x as usize * 4;
            let end = start + w as usize * 4;
            pixels[start..end].copy_from_slice(&source.pixels()[start..end]);
        }
    }
    pixels
}

fn updates(server: &Server, shared: bool) {
    let mut window = server.window();
    let target = window.presentation_target().unwrap();
    let mut renderer =
        X11Surface::present_in(Some(&server.name), target, ProtocolLimits::ABSOLUTE).unwrap();
    let mut base = picture(13);
    renderer.present(&base).unwrap();
    renderer.maintain_presentation().unwrap();
    assert_eq!(
        matches!(
            renderer.transfer_statistics().unwrap().path,
            Path::SharedMemory
        ),
        shared
    );
    // Include disjoint, overlapping, odd-sized and edge rectangles. Source
    // pixels outside the declared damage deliberately DIFFER from the baseline.
    for (value, regions) in [
        (71, vec![[1, 3, 7, 11], [53, 61, 11, 3]]),
        (125, vec![[9, 9, 10, 10], [12, 12, 10, 10]]),
        (179, vec![[63, 0, 1, 64]]),
        (211, vec![]),
    ] {
        let source = picture(value);
        let want = expected(&base, &source, &regions);
        let before = renderer.transfer_statistics().unwrap();
        renderer.present_regions(&source, &regions).unwrap();
        assert_eq!(window.snapshot().unwrap().pixels(), want);
        let after = renderer.transfer_statistics().unwrap();
        let bytes: u64 = regions
            .iter()
            .map(|r| u64::from(r[2]) * u64::from(r[3]) * 4)
            .sum();
        assert_eq!(after.copied_bytes - before.copied_bytes, bytes);
        assert_eq!(
            after.socket_pixel_bytes - before.socket_pixel_bytes,
            if shared { 0 } else { bytes }
        );
        assert_eq!(after.images - before.images, regions.len() as u64);
        assert_eq!(after.retained_bytes, before.retained_bytes);
        // Inspect the retained native backing image too, not just the pixels
        // already on screen: an accidental full copy can hide until exposure.
        server.clear(target.window(), 1);
        assert!(renderer.maintain_presentation().unwrap().repainted);
        assert_eq!(window.snapshot().unwrap().pixels(), want);
        assert_eq!(
            renderer.transfer_statistics().unwrap().copied_bytes,
            after.copied_bytes
        );
        base = BgraFrame::new(64, 64, want, &ProtocolLimits::ABSOLUTE).unwrap();
    }
}

#[test]
fn shared_memory_updates_only_declared_regions_without_another_frame_copy() {
    updates(&Server::start(), true);
}
#[test]
fn socket_fallback_transfers_only_declared_region_pixels() {
    updates(&Server::without_shared_memory(), false);
}

#[test]
fn invalid_region_lists_are_atomic_and_do_not_corrupt_later_exposure_repair() {
    let server = Server::start();
    let mut window = server.window();
    let target = window.presentation_target().unwrap();
    let mut renderer =
        X11Surface::present_in(Some(&server.name), target, ProtocolLimits::ABSOLUTE).unwrap();
    let base = picture(37);
    renderer.present(&base).unwrap();
    renderer.maintain_presentation().unwrap();
    let before = renderer.transfer_statistics().unwrap();
    for invalid in [
        vec![[0, 0, 1, 1], [63, 63, 2, 1]],
        vec![[0, 0, 0, 1]],
        vec![[64, 0, 1, 1]],
        vec![[1, 1, u32::MAX, 1]],
        vec![[0, u32::MAX, 1, 1]],
        vec![[0, 0, 1, 1]; 3],
    ] {
        assert_eq!(
            renderer.present_regions(&picture(199), &invalid),
            Err(NativeError::InvalidConfiguration)
        );
        assert_eq!(renderer.transfer_statistics().unwrap(), before);
        assert_eq!(window.snapshot().unwrap().pixels(), base.pixels());
    }
    let foreign = BgraFrame::new(32, 32, vec![0; 32 * 32 * 4], &ProtocolLimits::ABSOLUTE).unwrap();
    assert_eq!(
        renderer.present_regions(&foreign, &[]),
        Err(NativeError::GeometryChanged)
    );
    server.clear(target.window(), 1);
    assert!(renderer.maintain_presentation().unwrap().repainted);
    assert_eq!(window.snapshot().unwrap().pixels(), base.pixels());
}

#[test]
fn damage_requires_an_initialized_picture_and_cannot_paint_capture_surfaces() {
    let server = Server::start();
    let mut window = server.window();
    let before = window.transfer_statistics().unwrap();
    for regions in [&[][..], &[[0, 0, 1, 1]][..]] {
        assert_eq!(
            window.present_regions(&picture(213), regions),
            Err(NativeError::InvalidConfiguration)
        );
        assert_eq!(window.transfer_statistics().unwrap(), before);
    }
    let mut capture = X11Surface::capture(Some(&server.name), ProtocolLimits::ABSOLUTE).unwrap();
    let image = capture.snapshot().unwrap();
    assert_eq!(
        capture.present_regions(&image, &[]),
        Err(NativeError::InvalidConfiguration)
    );
}

#[test]
fn an_exposure_with_small_or_empty_damage_redraws_the_complete_retained_image() {
    for server in [Server::start(), Server::without_shared_memory()] {
        let mut window = server.window();
        let target = window.presentation_target().unwrap();
        let mut renderer =
            X11Surface::present_in(Some(&server.name), target, ProtocolLimits::ABSOLUTE).unwrap();
        let base = picture(43);
        renderer.present(&base).unwrap();
        renderer.maintain_presentation().unwrap();
        let patch = picture(127);
        let regions = [[3, 7, 5, 9]];
        let want = expected(&base, &patch, &regions);
        let before = renderer.transfer_statistics().unwrap();
        server.clear(target.window(), 1);
        assert_ne!(window.snapshot().unwrap().pixels(), base.pixels());
        renderer.present_regions(&patch, &regions).unwrap();
        assert_eq!(window.snapshot().unwrap().pixels(), want);
        let after = renderer.transfer_statistics().unwrap();
        assert_eq!(after.copied_bytes - before.copied_bytes, 5 * 9 * 4);
        assert_eq!(after.images - before.images, 1);
        server.clear(target.window(), 1);
        renderer.present_regions(&patch, &[]).unwrap();
        assert_eq!(window.snapshot().unwrap().pixels(), want);
        let empty = renderer.transfer_statistics().unwrap();
        assert_eq!(empty.copied_bytes, after.copied_bytes);
        assert_eq!(empty.images, after.images + 1);
    }
}

#[test]
fn empty_damage_still_fences_resize_away_and_back_and_unmapping() {
    for unmap in [false, true] {
        let server = Server::start();
        let mut window = server.window();
        let target = window.presentation_target().unwrap();
        let mut renderer =
            X11Surface::present_in(Some(&server.name), target, ProtocolLimits::ABSOLUTE).unwrap();
        renderer.present(&picture(53)).unwrap();
        if unmap {
            server.unmap_roundtrip(target.window());
        } else {
            server.resize_roundtrip(target.window());
        }
        assert_eq!(
            renderer.present_regions(&picture(57), &[]),
            Err(NativeError::GeometryChanged)
        );
        let ended = renderer.transfer_statistics().unwrap();
        assert_eq!(ended.retained_bytes, 0);
        assert_eq!(
            renderer.present(&picture(59)),
            Err(NativeError::GeometryChanged)
        );
        assert_eq!(renderer.transfer_statistics().unwrap(), ended);
    }
}

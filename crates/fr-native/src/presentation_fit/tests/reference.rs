//! Independent sampling oracle and CPU-stage timing comparison.
use super::*;
use std::{hint::black_box, time::Instant};

fn empty(target: X11Target) -> BgraFrame {
    let mut bytes =
        zeroed(frame_len(target.width(), target.height(), &ProtocolLimits::ABSOLUTE).unwrap())
            .unwrap();
    for pixel in bytes.as_chunks_mut::<4>().0 {
        pixel[3] = 255;
    }
    BgraFrame::new(
        target.width(),
        target.height(),
        bytes,
        &ProtocolLimits::ABSOLUTE,
    )
    .unwrap()
}
fn reference(source: &BgraFrame, output: &mut BgraFrame, fit: Fit) {
    for y in 0..fit.height {
        let sy = u64::from(y) * u64::from(source.height) / u64::from(fit.height);
        let src = usize::try_from(sy * u64::from(source.width) * 4).unwrap();
        let dst = usize::try_from(
            (u64::from(y + fit.y) * u64::from(output.width) + u64::from(fit.x)) * 4,
        )
        .unwrap();
        for x in 0..fit.width {
            let sx =
                usize::try_from(u64::from(x) * u64::from(source.width) / u64::from(fit.width) * 4)
                    .unwrap();
            let dx = usize::try_from(x).unwrap() * 4;
            if (source.width, source.height) == (2 * fit.width, 2 * fit.height) {
                let stride = usize::try_from(source.width).unwrap() * 4;
                for channel in 0..4 {
                    let start = src + sx + channel;
                    let sum: u32 = [0, 4, stride, stride + 4]
                        .map(|offset| u32::from(source.bytes[start + offset]))
                        .iter()
                        .sum();
                    output.bytes[dst + dx + channel] = u8::try_from((sum + 2) / 4).unwrap();
                }
            } else {
                output.bytes[dst + dx..dst + dx + 4]
                    .copy_from_slice(&source.bytes[src + sx..src + sx + 4]);
            }
        }
    }
}

#[test]
fn full_frames_match_reference_pixels_and_bars_across_viewport_shapes() {
    for (w, h) in [
        (64, 32),
        (32, 64),
        (82, 46),
        (48, 82),
        (320, 240),
        (1920, 1080),
    ] {
        let pixels: Vec<_> = (0..w * h).flat_map(u32::to_le_bytes).collect();
        let source = BgraFrame::new(w, h, pixels, &ProtocolLimits::ABSOLUTE).unwrap();
        for (tw, th) in [
            (16, 16),
            (18, 30),
            (30, 18),
            (32, 32),
            (64, 32),
            (32, 64),
            (640, 360),
            (1280, 720),
            (1920, 1080),
        ] {
            if tw > w || th > h {
                continue;
            }
            let target = X11Target::new(1, tw, th).unwrap();
            let mut fitted = FittedFrame::new(w, h, target, ProtocolLimits::ABSOLUTE).unwrap();
            let placement = fitted.placement();
            let mut expected = empty(target);
            reference(&source, &mut expected, placement);
            let retained = fitted.retained_bytes();
            let pointer = fitted.render(&source).unwrap().pixels().as_ptr();
            assert_eq!(fitted.render(&source).unwrap().pixels(), expected.pixels());
            assert_eq!(fitted.render(&source).unwrap().pixels().as_ptr(), pointer);
            assert_eq!(fitted.retained_bytes(), retained);
        }
    }
}

/// Optional CPU-stage comparison only, not HEVC/X11 or end-to-end latency.
/// Run optimized with `--release -- --ignored --nocapture`.
#[test]
#[ignore = "manual CPU scaling microbenchmark; no timing assertion"]
fn compare_fitted_render_cost() {
    for (w, h, tw, th) in [
        (1920, 1080, 1920, 1080),
        (1920, 1080, 1280, 720),
        (3840, 2160, 1920, 1080),
        (1920, 1080, 1022, 766),
    ] {
        let source = pattern(w, h, 17);
        let target = X11Target::new(1, tw, th).unwrap();
        let mut fitted = FittedFrame::new(w, h, target, ProtocolLimits::ABSOLUTE).unwrap();
        let placement = fitted.placement();
        let mut old_output = empty(target);
        reference(&source, &mut old_output, placement);
        assert_eq!(
            fitted.render(&source).unwrap().pixels(),
            old_output.pixels()
        );
        let mut old = Vec::new();
        let mut new = Vec::new();
        // Alternate which implementation runs first. Both consume the same
        // full frame and reuse one output allocation; neither includes setup.
        for sample in 0..6 {
            for run in 0..2 {
                let original = (sample + run) % 2 == 0;
                let start = Instant::now();
                for _ in 0..24 {
                    if original {
                        reference(
                            black_box(&source),
                            black_box(&mut old_output),
                            black_box(placement),
                        );
                        black_box(old_output.pixels());
                    } else {
                        black_box(
                            black_box(&mut fitted)
                                .render(black_box(&source))
                                .unwrap()
                                .pixels(),
                        );
                    }
                }
                let elapsed = start.elapsed().as_nanos() / 24;
                if original {
                    old.push(elapsed);
                } else {
                    new.push(elapsed);
                }
            }
        }
        old.sort_unstable();
        new.sort_unstable();
        println!(
            "fitted_cpu {w}x{h}->{tw}x{th} reference_ns={} optimized_ns={}",
            old[old.len() / 2],
            new[new.len() / 2]
        );
    }
}

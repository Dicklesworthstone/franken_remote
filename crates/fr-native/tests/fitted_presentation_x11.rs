#![cfg(all(target_os = "linux", feature = "linux-media"))]
//! Actual X11 pixel readback of the production scaler/borrowed destination.
//! The pixel source is a pattern, not HEVC, hardware or physical scanout proof.
use fr_core::limits::ProtocolLimits;
use fr_native::{BgraFrame, FittedFrame, X11Surface};
use std::{
    io::{BufRead, BufReader, Read},
    process::{Child, Command, Stdio},
};
struct Server {
    child: Child,
    display: String,
}
impl Server {
    fn start() -> Self {
        let mut child = Command::new("Xvfb")
            .args([
                "-displayfd",
                "1",
                "-screen",
                "0",
                "128x128x24",
                "-nolisten",
                "tcp",
                "-noreset",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut number = String::new();
        BufReader::new(child.stdout.take().unwrap().take(16))
            .read_line(&mut number)
            .unwrap();
        Self {
            child,
            display: format!(":{}", number.trim().parse::<u32>().unwrap()),
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
#[test]
fn borrowed_window_contains_the_exact_fitted_image_and_opaque_bars() {
    let server = Server::start();
    let limits = ProtocolLimits::ABSOLUTE;
    for (source_width, source_height, target_width, target_height) in [
        (64, 32, 32, 32),
        (32, 64, 32, 32),
        (64, 32, 34, 32),
        (64, 64, 32, 32),
    ] {
        let mut window =
            X11Surface::presenter(Some(&server.display), target_width, target_height, limits)
                .unwrap();
        let target = window.presentation_target().unwrap();
        let mut renderer = X11Surface::present_in(Some(&server.display), target, limits).unwrap();
        let mut scaler = FittedFrame::new(source_width, source_height, target, limits).unwrap();
        let placement = scaler.placement();
        for frame in [13_u8, 29, 91] {
            let mut bytes = Vec::new();
            for y in 0..source_height {
                for x in 0..source_width {
                    bytes.extend_from_slice(&[
                        u8::try_from(x).unwrap(),
                        u8::try_from(y).unwrap(),
                        frame,
                        255,
                    ]);
                }
            }
            let source = BgraFrame::new(source_width, source_height, bytes, &limits).unwrap();
            let output = scaler.render(&source).unwrap();
            renderer.present(output).unwrap();
            let displayed = window.snapshot().unwrap();
            assert_eq!(displayed.pixels(), output.pixels());
            // Independent expected pixel, not a scaler-versus-itself comparison.
            for y in 0..target_height {
                for x in 0..target_width {
                    let expected = if (placement.x..placement.x + placement.width).contains(&x)
                        && (placement.y..placement.y + placement.height).contains(&y)
                    {
                        [
                            u8::try_from((x - placement.x) * source_width / placement.width)
                                .unwrap(),
                            u8::try_from((y - placement.y) * source_height / placement.height)
                                .unwrap(),
                            frame,
                            255,
                        ]
                    } else {
                        [0, 0, 0, 255]
                    };
                    let offset = usize::try_from((y * target_width + x) * 4).unwrap();
                    assert_eq!(&displayed.pixels()[offset..offset + 4], &expected);
                }
            }
        }
        drop(renderer);
        // Borrowed decoder cleanup cannot destroy the UI's original drawable.
        assert!(window.snapshot().is_ok());
    }
}

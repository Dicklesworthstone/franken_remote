//! Actual private worker pipes, HEVC decoding and X11 pixels. The parent here
//! is a bounded test driver, not frd's authority supervisor or a tailnet session.
use super::*;
use fr_media::worker::{self, Identity, Kind, Record, overlay::Update};
use fr_native::{FittedFrame, X11Surface};
use std::{
    io::{BufRead, BufReader},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{Receiver, sync_channel},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

struct Display {
    child: Child,
    name: String,
}
impl Display {
    fn start() -> Self {
        let mut child = Command::new("Xvfb")
            .args([
                "-displayfd",
                "1",
                "-screen",
                "0",
                "320x240x24",
                "-noreset",
                "-nolisten",
                "tcp",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("Xvfb is required for native worker qualification");
        let mut number = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut number)
            .unwrap();
        Self {
            child,
            name: format!(":{}", number.trim().parse::<u32>().unwrap()),
        }
    }
}
impl Drop for Display {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
struct Worker {
    child: Child,
    input: ChildStdin,
    output: Option<Receiver<Record>>,
    reader: Option<JoinHandle<()>>,
    sequence: u64,
}
impl Worker {
    fn start(display: &Display) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_fr-media-worker"))
            .env_clear()
            .env("DISPLAY", &display.name)
            .args(["--present", "--parent-pid"])
            .arg(std::process::id().to_string())
            .arg("--verify-readback")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let mut pipe = child.stdout.take().unwrap();
        let (send, output) = sync_channel(1);
        let reader = thread::spawn(move || {
            while let Ok(Some(record)) = Record::read(&mut pipe, &ProtocolLimits::ABSOLUTE) {
                if send.send(record).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            input,
            output: Some(output),
            reader: Some(reader),
            sequence: 0,
        }
    }
    fn request(&mut self, kind: Kind, body: Vec<u8>) -> Record {
        let identity = Identity {
            epoch: 7,
            sequence: self.sequence,
        };
        self.sequence += 1;
        Record::new(kind, identity, body, &ProtocolLimits::ABSOLUTE)
            .unwrap()
            .write(&mut self.input, &ProtocolLimits::ABSOLUTE)
            .unwrap();
        let response = self
            .output
            .as_ref()
            .unwrap()
            .recv_timeout(Duration::from_secs(5))
            .expect("bounded native worker reply");
        assert_eq!(response.header.identity, identity);
        response
    }
    fn finish(&mut self, success: bool) {
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert_eq!(status.success(), success);
                return;
            }
            assert!(Instant::now() < until, "native worker failed to exit");
            thread::sleep(Duration::from_millis(1));
        }
    }
    fn stop(&mut self) {
        assert_eq!(self.request(Kind::Stop, vec![]).header.kind, Kind::Stopped);
        self.finish(true);
    }
    fn update(&mut self, update: &Update<'_>) {
        let response = self.request(
            Kind::CursorOverlay,
            worker::overlay::encode(update, &ProtocolLimits::ABSOLUTE).unwrap(),
        );
        assert_eq!(response.header.kind, Kind::CursorOverlayApplied);
        assert_eq!(response.body(), <&[u8]>::default());
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        drop(self.output.take());
        if let Some(reader) = self.reader.take() {
            reader.join().unwrap();
        }
    }
}
fn configured(
    display: &Display,
    first: &EncodedAccessUnit,
    fit: bool,
) -> (Worker, X11Surface, Option<FittedFrame>) {
    let c = configuration();
    let limits = c.limits().unwrap();
    let (width, height) = if fit { (96, 96) } else { (c.width, c.height) };
    let mut ui = X11Surface::presenter(Some(&display.name), width, height, limits).unwrap();
    let target = ui.presentation_target().unwrap();
    let mut guard = HevcGuard::new(c.codec().unwrap(), limits, 4).unwrap();
    guard.validate_length_prefixed(first.bytes(), true).unwrap();
    let record = guard.decoder_record().unwrap();
    let (kind, ready, body) = if fit {
        (
            Kind::ConfigureFittedPresentation,
            Kind::FittedPresentationReady,
            target.encode_fitted_decoder(c, &record).unwrap(),
        )
    } else {
        (
            Kind::ConfigurePresentation,
            Kind::PresentationReady,
            target.encode_decoder(c, &record).unwrap(),
        )
    };
    let mut worker = Worker::start(display);
    let response = worker.request(kind, body.clone());
    assert_eq!(response.header.kind, ready);
    assert_eq!(response.body(), body);
    let fitted = FittedFrame::for_viewport(c.width, c.height, target, limits).unwrap();
    (worker, ui, fitted)
}

#[test]
fn a_reference_only_worker_preserves_native_and_fitted_presented_pixels() {
    for fit in [false, true] {
        let display = Display::start();
        let mut encoder = encoder();
        let first = encode(&mut encoder, 0);
        let mut baseline = decoder(&first);
        let (mut worker, mut ui, mut fitted) = configured(&display, &first, fit);
        let mut first = Some(first);
        let mut visible = ui.snapshot().unwrap().pixels().to_vec();
        for n in 0..36 {
            let unit = first.take().unwrap_or_else(|| encode(&mut encoder, n));
            baseline.submit(&unit).unwrap();
            let (id, pixels) = baseline.poll_output().unwrap();
            // Start with a decode-only IDR and later present dependent pictures.
            let show = n % 9 == 8;
            let kind = if show { Kind::Present } else { Kind::Decode };
            let response = worker.request(kind, worker::unit_payload(&unit).unwrap());
            assert_eq!(
                response.header.kind,
                if show { Kind::Presented } else { Kind::Decoded }
            );
            assert_eq!(response.body(), id.as_raw().to_be_bytes());
            if show {
                let shown = match &mut fitted {
                    Some(fitted) => fitted.render(&pixels).unwrap(),
                    None => &pixels,
                };
                assert_ne!(visible, shown.pixels());
                visible = shown.pixels().to_vec();
            }
            assert_eq!(ui.snapshot().unwrap().pixels(), visible);
            assert_eq!(
                worker.request(Kind::Poll, vec![]).header.kind,
                Kind::NeedInput
            );
            assert_eq!(ui.snapshot().unwrap().pixels(), visible);
            baseline.recycle(pixels);
        }
        worker.stop();
    }
}

#[test]
fn cursor_recomposition_after_reference_only_work_uses_the_last_presented_picture() {
    for fit in [false, true] {
        let display = Display::start();
        let mut encoder = encoder();
        let first = encode(&mut encoder, 0);
        let (mut worker, mut ui, _) = configured(&display, &first, fit);
        assert_eq!(
            worker
                .request(Kind::Present, worker::unit_payload(&first).unwrap())
                .header
                .kind,
            Kind::Presented
        );
        let original = ui.snapshot().unwrap().pixels().to_vec();
        let rgba = [255; 4 * 4 * 4];
        worker.update(&Update::Shape {
            x: 16,
            y: 16,
            width: 4,
            height: 4,
            hotspot_x: 0,
            hotspot_y: 0,
            rgba: &rgba,
        });
        let with_cursor = ui.snapshot().unwrap().pixels().to_vec();
        assert_ne!(with_cursor, original);
        for n in 1..12 {
            let unit = encode(&mut encoder, n);
            let response = worker.request(Kind::Decode, worker::unit_payload(&unit).unwrap());
            assert_eq!(response.header.kind, Kind::Decoded);
            assert_eq!(response.body(), unit.frame().as_raw().to_be_bytes());
            assert_eq!(ui.snapshot().unwrap().pixels(), with_cursor);
        }
        worker.update(&Update::Move { x: 40, y: 40 });
        assert_ne!(ui.snapshot().unwrap().pixels(), with_cursor);
        worker.update(&Update::Hidden);
        assert_eq!(ui.snapshot().unwrap().pixels(), original);
        worker.stop();
    }
}

#[test]
fn a_decode_only_request_cannot_bypass_native_bitstream_refusal() {
    let display = Display::start();
    let mut encoder = encoder();
    let first = encode(&mut encoder, 0);
    let (mut worker, mut ui, _) = configured(&display, &first, false);
    let before = ui.snapshot().unwrap().pixels().to_vec();
    let malformed = EncodedAccessUnit::new(
        &ProtocolLimits::ABSOLUTE,
        first.frame(),
        first.kind(),
        first.config_generation(),
        first.capture_micros(),
        first.bytes()[..2].to_vec(),
    )
    .unwrap();
    let reply = worker.request(Kind::Decode, worker::unit_payload(&malformed).unwrap());
    assert_eq!(reply.header.kind, Kind::Refused);
    worker.finish(false);
    assert_eq!(ui.snapshot().unwrap().pixels(), before);
}

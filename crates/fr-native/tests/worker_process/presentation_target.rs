//! Real private pipes, software HEVC and X11 window pixels. No tailnet or
//! hardware/compositor qualification is inferred from this integration test.
use super::*;
use fr_media::{hevc::HevcGuard, worker::presentation::X11Target};
use fr_native::{EncodeBackend, HevcDecoder, HevcEncoder, NativeError};
use std::{path::Path, time::Duration};

fn picture() -> (
    fr_media::hevc::DecoderRecord,
    fr_media::access_unit::EncodedAccessUnit,
    BgraFrame,
) {
    let c = configuration();
    let limits = c.limits().unwrap();
    let mut encoder = HevcEncoder::new(
        c.codec().unwrap(),
        limits,
        EncodeBackend::SoftwareExplicit,
        u32::from(c.fps),
        c.bitrate,
    )
    .unwrap();
    let mut pixels = vec![0; (c.width * c.height * 4) as usize];
    for pixel in pixels.as_chunks_mut::<4>().0 {
        pixel.copy_from_slice(&[20, 120, 220, 255]);
    }
    let pixels = BgraFrame::new(c.width, c.height, pixels, &limits).unwrap();
    encoder.submit(&pixels, FrameId::FIRST, 0, true).unwrap();
    let unit = encoder.poll_output().unwrap();
    let mut guard = HevcGuard::new(c.codec().unwrap(), limits, 4).unwrap();
    guard.validate_length_prefixed(unit.bytes(), true).unwrap();
    let record = guard.decoder_record().unwrap();
    let mut decoder = HevcDecoder::new(c.codec().unwrap(), limits, record.bytes()).unwrap();
    decoder.submit(&unit).unwrap();
    let (_, expected) = decoder.poll_output().unwrap();
    (record, unit, expected)
}
fn owner(display: &Display) -> X11Surface {
    X11Surface::presenter(Some(&display.name), 320, 240, ProtocolLimits::ABSOLUTE).unwrap()
}
fn mutate(display: &Display, target: X11Target, operation: &str) {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/worker_process/target_peer.py");
    assert!(
        Command::new("python3")
            .arg(script)
            .arg(&display.name)
            .arg(target.window().to_string())
            .arg(operation)
            .status()
            .unwrap()
            .success()
    );
}
#[test]
fn real_decoder_presents_into_ui_window_and_reaping_does_not_destroy_it() {
    let display = Display::start();
    let mut ui = owner(&display);
    let target = ui.presentation_target().unwrap();
    let (record, unit, expected) = picture();
    let before = ui.snapshot().unwrap();
    assert_ne!(before.pixels(), expected.pixels());
    let mut worker = Worker::start(&display, Role::Present);
    let body = target.encode_decoder(configuration(), &record).unwrap();
    let ready = worker.transact(Kind::ConfigurePresentation, body.clone());
    assert_eq!(ready.header.kind, Kind::PresentationReady);
    assert_eq!(ready.body(), body);
    let reply = worker.transact(Kind::Present, unit_payload(&unit).unwrap());
    assert_eq!(reply.header.kind, Kind::Presented);
    assert_eq!(reply.body(), &FrameId::FIRST.as_raw().to_be_bytes());
    assert_eq!(ui.snapshot().unwrap().pixels(), expected.pixels());
    worker.stop();
    // Cleanup releases only the decoder connection/GC; the UI remains usable.
    ui.present(&before).unwrap();
    assert_eq!(ui.snapshot().unwrap().pixels(), before.pixels());
}
#[test]
fn target_change_is_terminal_even_after_original_size_or_mapping_returns() {
    for operation in ["resize-return", "unmap-return"] {
        let display = Display::start();
        let mut ui = owner(&display);
        let target = ui.presentation_target().unwrap();
        let (record, _unit, _) = picture();
        let mut worker = Worker::start(&display, Role::Present);
        let body = target.encode_decoder(configuration(), &record).unwrap();
        assert_eq!(
            worker
                .transact(Kind::ConfigurePresentation, body)
                .header
                .kind,
            Kind::PresentationReady
        );
        mutate(&display, target, operation);
        // Current workers service native lifecycle events while idle. Retirement
        // must not wait for a follow-up frame, nor invent a reply identity for an
        // unsolicited event. Require EOF and exit code 2 without sending another frame.
        assert_eq!(worker.child.wait().unwrap().code(), Some(2));
        assert!(
            Record::read(&mut worker.output, &ProtocolLimits::ABSOLUTE)
                .unwrap()
                .is_none()
        );
    }
}
#[test]
fn native_attach_rejects_actual_geometry_and_never_reexports_a_borrowed_target() {
    let display = Display::start();
    let mut ui = owner(&display);
    let target = ui.presentation_target().unwrap();
    let wrong = X11Target::new(target.window(), 322, 240).unwrap();
    assert!(matches!(
        X11Surface::present_in(Some(&display.name), wrong, ProtocolLimits::ABSOLUTE),
        Err(NativeError::GeometryChanged)
    ));
    let mut borrowed =
        X11Surface::present_in(Some(&display.name), target, ProtocolLimits::ABSOLUTE).unwrap();
    assert!(borrowed.presentation_target().is_err());
    let mut root = X11Surface::capture(Some(&display.name), ProtocolLimits::ABSOLUTE).unwrap();
    assert!(root.presentation_target().is_err());
    drop(borrowed);
    assert!(ui.snapshot().is_ok());
}
#[test]
fn canonical_supervisor_uses_the_selected_target_without_new_launch_authority() {
    use asupersync::{cx::Cx, runtime::RuntimeBuilder};
    use frd::worker::{Deadline, Launch, Worker as Supervised};
    let display = Display::start();
    let mut ui = owner(&display);
    let target = ui.presentation_target().unwrap();
    let (record, unit, expected) = picture();
    let image = Path::new(env!("CARGO_BIN_EXE_fr-media-worker"));
    assert!(
        Launch::new(image, &display.name, None, Role::Capture, 7)
            .unwrap()
            .present_in(target)
            .is_err()
    );
    let launch = Launch::new(image, &display.name, None, Role::Present, 7)
        .unwrap()
        .present_in(target)
        .unwrap();
    assert!(
        Launch::new(image, &display.name, None, Role::Present, 7)
            .unwrap()
            .present_in(target)
            .unwrap()
            .present_in(target)
            .is_err()
    );
    let runtime = RuntimeBuilder::new()
        .worker_threads(1)
        .blocking_threads(1, 2)
        .enable_platform_reactor(true)
        .build()
        .unwrap();
    runtime.block_on(async {
        let cx = Cx::current().unwrap();
        let deadline = || Deadline::after(&cx, Duration::from_secs(3)).unwrap();
        let mut worker =
            Supervised::start_decoder(&cx, launch, configuration(), &record, deadline())
                .await
                .unwrap();
        let reply = worker
            .request(&cx, Kind::Present, unit_payload(&unit).unwrap(), deadline())
            .await
            .unwrap();
        assert_eq!(reply.header.kind, Kind::Presented);
        assert_eq!(ui.snapshot().unwrap().pixels(), expected.pixels());
        worker
            .request(&cx, Kind::Stop, vec![], deadline())
            .await
            .unwrap();
        worker.reap(&cx, deadline()).await.unwrap();
    });
    assert!(ui.snapshot().is_ok());
}

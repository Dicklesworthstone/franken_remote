#![cfg(all(target_os = "linux", feature = "linux-media"))]
//! Real child process, admitted HEVC, independent X11 damage/readback. Repaint
//! must not mint worker responses or replace the last submitted reference.
#[path = "presentation_support/mod.rs"]
mod support;
use fr_core::{ids::CodecConfigurationGeneration, limits::ProtocolLimits};
use fr_media::{
    access_unit::{EncodedAccessUnit, FrameId},
    hevc::{DecoderRecord, HevcGuard},
    worker::{self, Backend, Configuration, Identity, Kind, Record, presentation::X11Target},
};
use fr_native::{BgraFrame, EncodeBackend, HevcEncoder, X11Surface};
use std::{
    io::Write,
    os::fd::{AsFd, AsRawFd, BorrowedFd},
    process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};
use support::{Server, picture};

#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}
unsafe extern "C" {
    fn poll(fds: *mut PollFd, count: usize, timeout: i32) -> i32;
}
fn readable(fd: BorrowedFd<'_>, timeout: i32) -> bool {
    let mut p = PollFd {
        fd: fd.as_raw_fd(),
        events: 1,
        revents: 0,
    };
    // SAFETY: the descriptor is borrowed for the call; one initialized pollfd.
    let count = unsafe { poll(&raw mut p, 1, timeout) };
    assert!(count >= 0);
    count > 0
}
struct Worker {
    child: Child,
    input: Option<ChildStdin>,
    output: ChildStdout,
    next: u64,
}
impl Worker {
    fn start(server: &Server) -> Self {
        // Optional local differential control; never changes production logic.
        let program = std::env::var_os("FR_TEST_WORKER")
            .unwrap_or_else(|| env!("CARGO_BIN_EXE_fr-media-worker").into());
        let mut child = Command::new(program)
            .env_clear()
            .env("DISPLAY", &server.name)
            .args(["--present", "--parent-pid", &std::process::id().to_string()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        Self {
            input: child.stdin.take(),
            output: child.stdout.take().unwrap(),
            child,
            next: 0,
        }
    }
    fn request(&mut self, kind: Kind, body: Vec<u8>) -> Record {
        let id = Identity {
            epoch: 19,
            sequence: self.next,
        };
        self.next += 1;
        Record::new(kind, id, body, &ProtocolLimits::ABSOLUTE).unwrap()
    }
    fn receive(&mut self, request: &Record) -> Record {
        assert!(
            readable(self.output.as_fd(), 2000),
            "worker response deadline"
        );
        let reply = Record::read(&mut self.output, &ProtocolLimits::ABSOLUTE)
            .unwrap()
            .expect("response, not EOF");
        assert_eq!(reply.header.identity, request.header.identity);
        reply
    }
    fn transact(&mut self, kind: Kind, body: Vec<u8>) -> Record {
        let request = self.request(kind, body);
        request
            .write(self.input.as_mut().unwrap(), &ProtocolLimits::ABSOLUTE)
            .unwrap();
        self.receive(&request)
    }
    fn configure(
        &mut self,
        target: X11Target,
        config: Configuration,
        record: &DecoderRecord,
        fitted: bool,
    ) {
        let (kind, body) = if fitted {
            (
                Kind::ConfigureFittedPresentation,
                target.encode_fitted_decoder(config, record).unwrap(),
            )
        } else {
            (
                Kind::ConfigurePresentation,
                target.encode_decoder(config, record).unwrap(),
            )
        };
        let expected = if fitted {
            Kind::FittedPresentationReady
        } else {
            Kind::PresentationReady
        };
        let reply = self.transact(kind, body.clone());
        assert_eq!(reply.header.kind, expected);
        assert_eq!(reply.body(), body);
    }
    fn frame(&mut self, kind: Kind, unit: &EncodedAccessUnit) {
        let reply = self.transact(kind, worker::unit_payload(unit).unwrap());
        assert_eq!(
            reply.header.kind,
            if kind == Kind::Present {
                Kind::Presented
            } else {
                Kind::Decoded
            }
        );
        assert_eq!(reply.body(), &unit.frame().as_raw().to_be_bytes());
    }
    fn wait(&mut self) -> ExitStatus {
        let until = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < until, "worker failed to stop");
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    fn stop(&mut self) {
        assert_eq!(self.transact(Kind::Stop, vec![]).header.kind, Kind::Stopped);
        assert!(self.wait().success());
        assert!(
            Record::read(&mut self.output, &ProtocolLimits::ABSOLUTE)
                .unwrap()
                .is_none()
        );
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn stream(
    width: u32,
    height: u32,
) -> (Configuration, HevcEncoder, EncodedAccessUnit, DecoderRecord) {
    let config = Configuration {
        width,
        height,
        fps: 30,
        backend: Backend::SoftwareExplicit,
        bitrate: 2_000_000,
        max_access_unit_bytes: 1024 * 1024,
        generation: CodecConfigurationGeneration::INITIAL,
    };
    let limits = config.limits().unwrap();
    let mut encoder = HevcEncoder::new(
        config.codec().unwrap(),
        limits,
        EncodeBackend::SoftwareExplicit,
        30,
        config.bitrate,
    )
    .unwrap();
    let source = BgraFrame::new(
        width,
        height,
        vec![77; usize::try_from(width * height * 4).unwrap()],
        &limits,
    )
    .unwrap();
    encoder.submit(&source, FrameId::FIRST, 0, true).unwrap();
    let first = encoder.poll_output().unwrap();
    let mut guard = HevcGuard::new(config.codec().unwrap(), limits, 4).unwrap();
    guard.validate_length_prefixed(first.bytes(), true).unwrap();
    (config, encoder, first, guard.decoder_record().unwrap())
}
fn await_pixels(window: &mut X11Surface, expected: &[u8]) {
    let until = Instant::now() + Duration::from_millis(500);
    loop {
        if window.snapshot().unwrap().pixels() == expected {
            break;
        }
        assert!(Instant::now() < until, "idle drawable was not restored");
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn real_hevc_worker_repairs_exposure_without_another_parent_record() {
    let server = Server::start();
    let mut window = server.window();
    let target = window.presentation_target().unwrap();
    let (config, _encoder, first, record) = stream(64, 64);
    let mut worker = Worker::start(&server);
    worker.configure(target, config, &record, false);
    worker.frame(Kind::Present, &first);
    let expected = window.snapshot().unwrap();
    assert!(expected.pixels()[0] > 40);
    for _ in 0..4 {
        server.clear(target.window(), 4);
        await_pixels(&mut window, expected.pixels());
        assert!(
            !readable(worker.output.as_fd(), 15),
            "repaint minted an unsolicited completion"
        );
    }
    worker.stop();
    assert!(
        window.snapshot().is_ok(),
        "worker destroyed borrowed UI window"
    );
}

#[test]
fn decode_only_references_do_not_replace_retained_presentation() {
    let server = Server::start();
    let mut window = server.window();
    let target = window.presentation_target().unwrap();
    let (config, mut encoder, first, record) = stream(64, 64);
    let mut worker = Worker::start(&server);
    worker.configure(target, config, &record, false);
    worker.frame(Kind::Present, &first);
    let expected = window.snapshot().unwrap();
    encoder
        .submit(&picture(149), FrameId::from_raw(1), 33_333, false)
        .unwrap();
    let reference = encoder.poll_output().unwrap();
    assert!(!reference.is_idr());
    worker.frame(Kind::Decode, &reference);
    server.clear(target.window(), 2);
    await_pixels(&mut window, expected.pixels());
    encoder
        .submit(&picture(219), FrameId::from_raw(2), 66_666, false)
        .unwrap();
    let next = encoder.poll_output().unwrap();
    assert!(!next.is_idr());
    worker.frame(Kind::Present, &next);
    let newest = window.snapshot().unwrap();
    assert_ne!(newest.pixels(), expected.pixels());
    server.clear(target.window(), 2);
    await_pixels(&mut window, newest.pixels());
    worker.stop();
}

#[test]
fn fitted_decoder_output_repaints_the_existing_viewport_and_bars() {
    let server = Server::start();
    let mut window = server.window();
    let target = window.presentation_target().unwrap();
    let (config, _encoder, first, record) = stream(128, 64);
    let mut worker = Worker::start(&server);
    worker.configure(target, config, &record, true);
    worker.frame(Kind::Present, &first);
    let expected = window.snapshot().unwrap();
    assert_eq!(&expected.pixels()[0..4], &[0, 0, 0, 255]);
    assert!(expected.pixels()[32 * 64 * 4] > 40);
    server.clear(target.window(), 2);
    await_pixels(&mut window, expected.pixels());
    worker.stop();
}

#[test]
fn exposure_before_first_decode_emits_no_completion_and_keeps_startup_usable() {
    let server = Server::start();
    let mut window = server.window();
    let target = window.presentation_target().unwrap();
    let (config, _encoder, first, record) = stream(64, 64);
    let mut worker = Worker::start(&server);
    worker.configure(target, config, &record, false);
    server.clear(target.window(), 4);
    assert!(!readable(worker.output.as_fd(), 30));
    worker.frame(Kind::Present, &first);
    worker.stop();
}

#[test]
fn coalesced_bootstrap_present_and_stop_are_not_hidden_by_stdin_prefetch() {
    let server = Server::start();
    let mut window = server.window();
    let target = window.presentation_target().unwrap();
    let (config, _encoder, first, record) = stream(64, 64);
    let mut worker = Worker::start(&server);
    let configure = worker.request(
        Kind::ConfigurePresentation,
        target.encode_decoder(config, &record).unwrap(),
    );
    let present = worker.request(Kind::Present, worker::unit_payload(&first).unwrap());
    let stop = worker.request(Kind::Stop, vec![]);
    let mut batch = vec![];
    for request in [&configure, &present, &stop] {
        request
            .write(&mut batch, &ProtocolLimits::ABSOLUTE)
            .unwrap();
    }
    worker.input.as_mut().unwrap().write_all(&batch).unwrap();
    assert_eq!(
        worker.receive(&configure).header.kind,
        Kind::PresentationReady
    );
    assert_eq!(worker.receive(&present).header.kind, Kind::Presented);
    assert_eq!(worker.receive(&stop).header.kind, Kind::Stopped);
    assert!(worker.wait().success());
}

#[test]
fn pipe_eof_stops_a_quiet_worker_and_releases_borrowed_renderer() {
    let server = Server::start();
    let mut window = server.window();
    let target = window.presentation_target().unwrap();
    let (config, _encoder, first, record) = stream(64, 64);
    let mut worker = Worker::start(&server);
    worker.configure(target, config, &record, false);
    worker.frame(Kind::Present, &first);
    drop(worker.input.take());
    assert!(worker.wait().success());
    assert!(
        Record::read(&mut worker.output, &ProtocolLimits::ABSOLUTE)
            .unwrap()
            .is_none()
    );
    // The worker no longer repairs, and never destroys the UI's own window.
    server.clear(target.window(), 1);
    assert_eq!(&window.snapshot().unwrap().pixels()[0..4], &[0, 0, 0, 255]);
}

#[test]
fn idle_resize_away_and_back_ends_worker_without_another_frame() {
    let server = Server::start();
    let mut window = server.window();
    let target = window.presentation_target().unwrap();
    let (config, _encoder, first, record) = stream(64, 64);
    let mut worker = Worker::start(&server);
    worker.configure(target, config, &record, false);
    worker.frame(Kind::Present, &first);
    server.resize_roundtrip(target.window());
    assert_eq!(worker.wait().code(), Some(2));
    assert!(
        Record::read(&mut worker.output, &ProtocolLimits::ABSOLUTE)
            .unwrap()
            .is_none()
    );
}

#[test]
fn idle_unmap_and_remap_ends_worker_without_a_parent_roundtrip() {
    let server = Server::start();
    let mut window = server.window();
    let target = window.presentation_target().unwrap();
    let (config, _encoder, first, record) = stream(64, 64);
    let mut worker = Worker::start(&server);
    worker.configure(target, config, &record, false);
    worker.frame(Kind::Present, &first);
    server.unmap_roundtrip(target.window());
    assert_eq!(worker.wait().code(), Some(2));
}

#[test]
fn destroying_an_idle_target_does_not_submit_to_or_destroy_it_again() {
    let server = Server::start();
    let mut window = server.window();
    let target = window.presentation_target().unwrap();
    let (config, _encoder, first, record) = stream(64, 64);
    let mut worker = Worker::start(&server);
    worker.configure(target, config, &record, false);
    worker.frame(Kind::Present, &first);
    server.destroy(target.window());
    assert_eq!(worker.wait().code(), Some(2));
    assert_eq!(
        window.maintain_presentation(),
        Err(fr_native::NativeError::GeometryChanged)
    );
}

#[test]
fn unrelated_native_events_do_not_spin_or_mint_media_responses() {
    let server = Server::start();
    let mut window = server.window();
    let target = window.presentation_target().unwrap();
    let (config, _encoder, first, record) = stream(64, 64);
    let mut worker = Worker::start(&server);
    worker.configure(target, config, &record, false);
    worker.frame(Kind::Present, &first);
    server.noise(target.window());
    assert!(!readable(worker.output.as_fd(), 30));
    // Linux task counters are only an idle regression check, not a product
    // performance claim. No busy event loop should accrue sustained CPU time.
    let ticks = || {
        let stat = std::fs::read_to_string(format!("/proc/{}/stat", worker.child.id())).unwrap();
        let rest = stat.rsplit_once(") ").unwrap().1;
        rest.split_whitespace()
            .skip(11)
            .take(2)
            .map(|n| n.parse::<u64>().unwrap())
            .sum::<u64>()
    };
    let before = ticks();
    std::thread::sleep(Duration::from_millis(150));
    assert!(
        ticks().saturating_sub(before) <= 2,
        "idle worker busy-spun after unrelated event"
    );
    worker.stop();
}

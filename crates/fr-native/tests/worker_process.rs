#![cfg(all(target_os = "linux", feature = "linux-media"))]
use fr_core::{ids::CodecConfigurationGeneration, limits::ProtocolLimits};
use fr_media::{access_unit::FrameId, worker::*};
use fr_native::{BgraFrame, X11Surface};
use std::{
    io::{BufRead, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
};

struct Display {
    child: Child,
    name: String,
}
impl Display {
    fn start() -> Self {
        Self::screens(false)
    }
    fn screens(two: bool) -> Self {
        let mut command = Command::new("Xvfb");
        if two {
            command.args(["-screen", "1", "320x240x24"]);
        }
        let mut child = command
            .args([
                "-displayfd",
                "1",
                "-screen",
                "0",
                "320x240x24",
                "-nolisten",
                "tcp",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("Xvfb is a native test prerequisite");
        let mut name = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut name)
            .unwrap();
        let number = name.trim().parse::<u32>().expect("Xvfb displayfd response");
        Self {
            child,
            name: format!(":{number}"),
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
    output: ChildStdout,
    next: u64,
}
impl Worker {
    fn start(display: &Display, role: Role) -> Self {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_fr-media-worker"));
        cmd.env_clear()
            .env("DISPLAY", &display.name)
            .arg(match role {
                Role::Capture => "--capture",
                Role::Present => "--present",
            });
        cmd.arg("--parent-pid").arg(std::process::id().to_string());
        if role == Role::Present {
            cmd.arg("--verify-readback");
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        Self {
            input: child.stdin.take().unwrap(),
            output: child.stdout.take().unwrap(),
            child,
            next: 0,
        }
    }
    fn transact(&mut self, kind: Kind, body: Vec<u8>) -> Record {
        let identity = Identity {
            epoch: 7,
            sequence: self.next,
        };
        self.next += 1;
        Record::new(kind, identity, body, &ProtocolLimits::ABSOLUTE)
            .unwrap()
            .write(&mut self.input, &ProtocolLimits::ABSOLUTE)
            .unwrap();
        let response = Record::read(&mut self.output, &ProtocolLimits::ABSOLUTE)
            .unwrap()
            .unwrap();
        assert_eq!(response.header.identity, identity);
        response
    }
    fn configure_decoder(&mut self) {
        let cfg = configuration();
        let limits = cfg.limits().unwrap();
        let mut encoder = fr_native::HevcEncoder::new(
            cfg.codec().unwrap(),
            limits,
            fr_native::EncodeBackend::SoftwareExplicit,
            u32::from(cfg.fps),
            cfg.bitrate,
        )
        .unwrap();
        let pixels =
            BgraFrame::new(cfg.width, cfg.height, vec![0; 320 * 240 * 4], &limits).unwrap();
        encoder.submit(&pixels, FrameId::FIRST, 0, true).unwrap();
        let unit = encoder.poll_output().unwrap();
        let mut admission =
            fr_media::hevc::HevcGuard::new(cfg.codec().unwrap(), limits, 4).unwrap();
        admission
            .validate_length_prefixed(unit.bytes(), true)
            .unwrap();
        let body = cfg
            .encode_decoder(&admission.decoder_record().unwrap())
            .unwrap();
        assert_eq!(
            self.transact(Kind::ConfigureDecoder, body).header.kind,
            Kind::DecoderReady
        );
    }
    fn configure(&mut self) {
        assert_eq!(
            self.transact(Kind::Configure, configuration().encode().unwrap())
                .header
                .kind,
            Kind::Ready
        );
    }
    fn stop(&mut self) {
        assert_eq!(self.transact(Kind::Stop, vec![]).header.kind, Kind::Stopped);
        assert!(self.child.wait().unwrap().success());
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn configuration() -> Configuration {
    Configuration {
        width: 320,
        height: 240,
        fps: 30,
        backend: Backend::SoftwareExplicit,
        bitrate: 2_000_000,
        max_access_unit_bytes: 1024 * 1024,
        generation: CodecConfigurationGeneration::INITIAL,
    }
}

#[test]
fn distinct_processes_capture_encode_decode_and_present_changing_desktop() {
    let source_display = Display::start();
    let viewer_display = Display::start();
    let limits = configuration().limits().unwrap();
    let mut source = X11Surface::presenter(Some(&source_display.name), 320, 240, limits).unwrap();
    let mut capture = Worker::start(&source_display, Role::Capture);
    capture.configure();
    let mut viewer = Worker::start(&viewer_display, Role::Present);
    assert_ne!(capture.child.id(), viewer.child.id());
    assert_ne!(capture.child.id(), std::process::id());
    let mut observed = X11Surface::capture(Some(&viewer_display.name), limits).unwrap();
    let mut previous = None;
    for frame in 0..8_u64 {
        let mut pixels = vec![0; 320 * 240 * 4];
        for (index, p) in pixels.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            p.copy_from_slice(&[
                u8::try_from(index % 200).unwrap(),
                40,
                u8::try_from(frame * 25).unwrap(),
                255,
            ]);
        }
        source
            .present(&BgraFrame::new(320, 240, pixels, &limits).unwrap())
            .unwrap();
        let response = capture.transact(
            Kind::Capture,
            capture_payload(FrameId::from_raw(frame), frame * 33_333, frame == 4),
        );
        assert_eq!(response.header.kind, Kind::Unit);
        let unit = parse_unit(response.into_body(), &limits).unwrap();
        assert_eq!(unit.frame().as_raw(), frame);
        assert_eq!(unit.is_idr(), frame == 0 || frame == 4);
        if frame == 0 {
            let mut admission =
                fr_media::hevc::HevcGuard::new(configuration().codec().unwrap(), limits, 4)
                    .unwrap();
            admission
                .validate_length_prefixed(unit.bytes(), true)
                .unwrap();
            let payload = configuration()
                .encode_decoder(&admission.decoder_record().unwrap())
                .unwrap();
            let ready = viewer.transact(Kind::ConfigureDecoder, payload.clone());
            assert_eq!(ready.header.kind, Kind::DecoderReady);
            assert_eq!(ready.body(), payload);
            assert_eq!(
                viewer.transact(Kind::Poll, vec![]).header.kind,
                Kind::NeedInput
            );
        }
        let shown = viewer.transact(Kind::Present, unit_payload(&unit).unwrap());
        assert_eq!(shown.header.kind, Kind::Presented);
        assert_eq!(shown.body(), frame.to_be_bytes());
        let current = observed.snapshot().unwrap();
        if let Some(old) = previous {
            assert_ne!(current.pixels(), old, "presentation did not change");
        }
        previous = Some(current.pixels().to_vec());
    }
    capture.stop();
    viewer.stop();
}
#[test]
fn wrong_role_refuses_before_capture_and_exits() {
    let display = Display::start();
    let mut viewer = Worker::start(&display, Role::Present);
    viewer.configure_decoder();
    let response = viewer.transact(Kind::Capture, capture_payload(FrameId::FIRST, 0, false));
    assert_eq!(response.header.kind, Kind::Refused);
    assert_eq!(response.body(), (Error::WrongRole as u16).to_be_bytes());
    assert!(!viewer.child.wait().unwrap().success());
}
#[test]
fn repeated_configuration_and_wrong_epoch_are_terminal() {
    let display = Display::start();
    let mut worker = Worker::start(&display, Role::Present);
    worker.configure_decoder();
    assert_eq!(
        worker
            .transact(Kind::Configure, configuration().encode().unwrap())
            .header
            .kind,
        Kind::Refused
    );
    assert!(!worker.child.wait().unwrap().success());
    let mut worker = Worker::start(&display, Role::Present);
    worker.configure_decoder();
    Record::new(
        Kind::Stop,
        Identity {
            epoch: 8,
            sequence: 1,
        },
        vec![],
        &ProtocolLimits::ABSOLUTE,
    )
    .unwrap()
    .write(&mut worker.input, &ProtocolLimits::ABSOLUTE)
    .unwrap();
    assert!(
        Record::read(&mut worker.output, &ProtocolLimits::ABSOLUTE)
            .unwrap()
            .is_none()
    );
    assert!(!worker.child.wait().unwrap().success());
}

#[test]
fn real_capture_skips_static_encoding_but_preserves_changed_and_forced_idr_chain() {
    let display = Display::start();
    let limits = configuration().limits().unwrap();
    let mut surface = X11Surface::presenter(Some(&display.name), 320, 240, limits).unwrap();
    let mut pixels = vec![0; 320 * 240 * 4];
    for p in pixels.as_chunks_mut::<4>().0 {
        p.copy_from_slice(&[25, 75, 125, 255]);
    }
    surface
        .present(&BgraFrame::new(320, 240, pixels.clone(), &limits).unwrap())
        .unwrap();
    let mut worker = Worker::start(&display, Role::Capture);
    worker.configure();
    let first = worker.transact(
        Kind::CaptureIfChanged,
        capture_payload(FrameId::FIRST, 100, false),
    );
    assert_eq!(first.header.kind, Kind::Unit);
    assert!(parse_unit(first.into_body(), &limits).unwrap().is_idr());
    for candidate in 1..=24 {
        let reply = worker.transact(
            Kind::CaptureIfChanged,
            capture_payload(FrameId::from_raw(candidate), 100 + candidate, false),
        );
        assert_eq!(reply.header.kind, Kind::Unchanged);
        assert_eq!(
            UnchangedCapture::decode(reply.body()).unwrap(),
            UnchangedCapture {
                candidate: FrameId::from_raw(candidate),
                reference: FrameId::FIRST,
                observed_micros: 100 + candidate,
            }
        );
        assert_eq!(
            worker.transact(Kind::Poll, vec![]).header.kind,
            Kind::NeedInput
        );
    }
    // One changed pixel is enough; a sparse sampling comparator would miss it.
    pixels[12345 * 4 + 1] = 230;
    surface
        .present(&BgraFrame::new(320, 240, pixels, &limits).unwrap())
        .unwrap();
    let changed = worker.transact(
        Kind::CaptureIfChanged,
        capture_payload(FrameId::from_raw(25), 125, false),
    );
    assert_eq!(changed.header.kind, Kind::Unit);
    let changed = parse_unit(changed.into_body(), &limits).unwrap();
    assert_eq!(changed.frame(), FrameId::from_raw(25));
    assert_eq!(
        changed.kind(),
        fr_media::access_unit::FrameKind::Predicted {
            references: FrameId::FIRST
        }
    );
    let forced = worker.transact(
        Kind::CaptureIfChanged,
        capture_payload(FrameId::from_raw(26), 126, true),
    );
    assert_eq!(forced.header.kind, Kind::Unit);
    let forced = parse_unit(forced.into_body(), &limits).unwrap();
    assert!(forced.is_idr());
    assert_eq!(forced.frame(), FrameId::from_raw(26));
    let idle = worker.transact(
        Kind::CaptureIfChanged,
        capture_payload(FrameId::from_raw(27), 127, false),
    );
    assert_eq!(
        UnchangedCapture::decode(idle.body()).unwrap().reference,
        forced.frame()
    );
    worker.stop();
}

#[test]
fn pending_native_capture_cannot_certify_unchanged_and_clock_faults_are_terminal() {
    use fr_native::{
        EncodeBackend, HevcEncoder, NativeError,
        capture::{CaptureOutput, ChangeAwareCapture},
    };
    let display = Display::start();
    let c = configuration();
    let surface = X11Surface::capture(Some(&display.name), c.limits().unwrap()).unwrap();
    let codec = HevcEncoder::new(
        c.codec().unwrap(),
        c.limits().unwrap(),
        EncodeBackend::SoftwareExplicit,
        u32::from(c.fps),
        c.bitrate,
    )
    .unwrap();
    let mut capture = ChangeAwareCapture::new(surface, codec);
    assert_eq!(
        capture.capture(FrameId::FIRST, 100, false, true),
        Ok(CaptureOutput::Submitted)
    );
    assert_eq!(
        capture.capture(FrameId::from_raw(1), 101, false, true),
        Err(NativeError::NeedDrain)
    );
    let first = capture.poll_output().unwrap();
    assert!(first.is_idr());
    assert_eq!(
        capture.capture(FrameId::from_raw(1), 101, false, true),
        Ok(CaptureOutput::Unchanged(UnchangedCapture {
            candidate: FrameId::from_raw(1),
            reference: FrameId::FIRST,
            observed_micros: 101,
        }))
    );
    assert_eq!(
        capture.capture(FrameId::from_raw(2), 99, false, true),
        Err(NativeError::StaleGeneration)
    );
    assert_eq!(
        capture.capture(FrameId::from_raw(3), 102, false, true),
        Err(NativeError::Closed)
    );
}

#[test]
fn decoder_startup_without_exact_parameters_is_terminal_before_decode() {
    let display = Display::start();
    let mut viewer = Worker::start(&display, Role::Present);
    let reply = viewer.transact(Kind::Configure, configuration().encode().unwrap());
    assert_eq!(reply.header.kind, Kind::Refused);
    assert_eq!(reply.body(), (Error::WrongState as u16).to_be_bytes());
    assert!(!viewer.child.wait().unwrap().success());
    let mut viewer = Worker::start(&display, Role::Present);
    let mut body = configuration().encode().unwrap();
    body.extend_from_slice(&[0; 23]);
    let reply = viewer.transact(Kind::ConfigureDecoder, body);
    assert_eq!(reply.header.kind, Kind::Refused);
    assert!(!viewer.child.wait().unwrap().success());
}

#[test]
fn discovered_screen_selection_encodes_the_nondefault_same_size_source() {
    let display = Display::screens(true);
    let cfg = configuration();
    let limits = cfg.limits().unwrap();
    let mut first = X11Surface::presenter(
        Some(&format!("{}.0", display.name)),
        cfg.width,
        cfg.height,
        limits,
    )
    .unwrap();
    let mut second = X11Surface::presenter(
        Some(&format!("{}.1", display.name)),
        cfg.width,
        cfg.height,
        limits,
    )
    .unwrap();
    first
        .present(&BgraFrame::new(320, 240, [220, 20, 30, 255].repeat(320 * 240), &limits).unwrap())
        .unwrap();
    second
        .present(&BgraFrame::new(320, 240, [30, 40, 210, 255].repeat(320 * 240), &limits).unwrap())
        .unwrap();
    let mut child = Worker::start(&display, Role::Capture);
    let discovered = child.transact(Kind::DiscoverCapture, vec![]);
    assert_eq!(discovered.header.kind, Kind::CaptureScreens);
    let screens = capture::Screens::decode(discovered.body()).unwrap();
    assert_eq!(screens.entries().len(), 2);
    assert_ne!(screens.entries()[0].root, screens.entries()[1].root);
    let selected = *screens.entries().iter().find(|s| s.index == 1).unwrap();
    let bytes = capture::configure(cfg, selected).unwrap();
    let ready = child.transact(Kind::ConfigureCapture, bytes.clone());
    assert_eq!(ready.header.kind, Kind::CaptureReady);
    assert_eq!(ready.body(), bytes);
    let reply = child.transact(Kind::Capture, capture_payload(FrameId::FIRST, 1, true));
    assert_eq!(reply.header.kind, Kind::Unit);
    let unit = parse_unit(reply.into_body(), &limits).unwrap();
    let mut guard = fr_media::hevc::HevcGuard::new(cfg.codec().unwrap(), limits, 4).unwrap();
    guard.validate_length_prefixed(unit.bytes(), true).unwrap();
    let mut decoder = fr_native::HevcDecoder::new(
        cfg.codec().unwrap(),
        limits,
        guard.decoder_record().unwrap().bytes(),
    )
    .unwrap();
    decoder.submit(&unit).unwrap();
    let (id, pixels) = decoder.poll_output().unwrap();
    assert_eq!(id, FrameId::FIRST);
    for (actual, wanted) in pixels.pixels()[100 * 320 * 4 + 100 * 4..][..3]
        .iter()
        .zip([30u8, 40, 210])
    {
        assert!(
            actual.abs_diff(wanted) <= 8,
            "selected nondefault screen was not captured"
        );
    }
    child.stop();
}
#[test]
fn discovery_rejects_default_fallback_forged_sources_and_second_discovery() {
    let display = Display::start();
    for variant in 0..4 {
        let mut child = Worker::start(&display, Role::Capture);
        let reply = child.transact(Kind::DiscoverCapture, vec![]);
        let screens = capture::Screens::decode(reply.body()).unwrap();
        let source = screens.entries()[0];
        let (kind, body) = match variant {
            0 => (Kind::Configure, configuration().encode().unwrap()),
            1 => (Kind::DiscoverCapture, vec![]),
            2 => (
                Kind::ConfigureCapture,
                capture::configure(
                    configuration(),
                    capture::Screen {
                        root: source.root + 1,
                        ..source
                    },
                )
                .unwrap(),
            ),
            _ => (
                Kind::ConfigureCapture,
                capture::configure(configuration(), capture::Screen { index: 1, ..source })
                    .unwrap(),
            ),
        };
        assert_eq!(child.transact(kind, body).header.kind, Kind::Refused);
        assert!(!child.child.wait().unwrap().success());
    }
    let mut child = Worker::start(&display, Role::Capture);
    let fake = capture::Screen {
        index: 0,
        root: 1,
        width: 320,
        height: 240,
    };
    assert_eq!(
        child
            .transact(
                Kind::ConfigureCapture,
                capture::configure(configuration(), fake).unwrap()
            )
            .header
            .kind,
        Kind::Refused
    );
}
#[test]
fn discovery_stops_without_codec_setup_and_never_runs_in_presenter_role() {
    let display = Display::start();
    let mut child = Worker::start(&display, Role::Capture);
    assert_eq!(
        child.transact(Kind::DiscoverCapture, vec![]).header.kind,
        Kind::CaptureScreens
    );
    child.stop();
    let mut presenter = Worker::start(&display, Role::Present);
    assert_eq!(
        presenter
            .transact(Kind::DiscoverCapture, vec![])
            .header
            .kind,
        Kind::Refused
    );
    assert!(!presenter.child.wait().unwrap().success());
}

fn runtime<F, Fut>(f: F)
where
    F: FnOnce(asupersync::cx::Cx) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let runtime = asupersync::runtime::RuntimeBuilder::new()
        .worker_threads(2)
        .blocking_threads(2, 4)
        .enable_platform_reactor(true)
        .build()
        .unwrap();
    runtime.block_on(async {
        let cx = asupersync::cx::Cx::current().unwrap();
        asupersync::time::timeout(cx.now(), std::time::Duration::from_secs(5), f(cx))
            .await
            .expect("bounded native discovery test");
    });
}
fn supervised_launch(display: &Display, role: Role) -> frd::worker::Launch {
    frd::worker::Launch::new(
        std::path::Path::new(env!("CARGO_BIN_EXE_fr-media-worker")),
        &display.name,
        None,
        role,
        91,
    )
    .unwrap()
}
#[test]
fn supervised_discovery_keeps_the_same_child_through_selection_capture_and_reap() {
    runtime(|cx| async move {
        use frd::worker::{Deadline, State, Worker as Supervised};
        use std::time::Duration;
        let display = Display::screens(true);
        let discovery = Supervised::discover_capture(
            &cx,
            supervised_launch(&display, Role::Capture),
            Deadline::after(&cx, Duration::from_secs(1)).unwrap(),
        )
        .await
        .unwrap();
        let pid = discovery.id();
        let screen = discovery.screens().entries()[1];
        assert_eq!(screen.index, 1);
        let mut worker = discovery
            .configure(
                &cx,
                screen,
                configuration(),
                Deadline::after(&cx, Duration::from_secs(1)).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(worker.id(), pid);
        assert_eq!(worker.state(), State::Running);
        let reply = worker
            .request(
                &cx,
                Kind::Capture,
                capture_payload(FrameId::FIRST, 1, true),
                Deadline::after(&cx, Duration::from_secs(1)).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(reply.header.kind, Kind::Unit);
        worker
            .request(
                &cx,
                Kind::Stop,
                vec![],
                Deadline::after(&cx, Duration::from_secs(1)).unwrap(),
            )
            .await
            .unwrap();
        assert!(
            worker
                .reap(&cx, Deadline::after(&cx, Duration::from_secs(1)).unwrap())
                .await
                .unwrap()
                .success()
        );
        assert_eq!(worker.state(), State::Reaped);
    });
}
#[test]
fn supervised_discovery_can_stop_before_codec_setup() {
    runtime(|cx| async move {
        use frd::worker::{Deadline, Worker as Supervised};
        use std::time::Duration;
        let display = Display::start();
        let mut discovery = Supervised::discover_capture(
            &cx,
            supervised_launch(&display, Role::Capture),
            Deadline::after(&cx, Duration::from_secs(1)).unwrap(),
        )
        .await
        .unwrap();
        discovery
            .stop(&cx, Deadline::after(&cx, Duration::from_secs(1)).unwrap())
            .await
            .unwrap();
        assert!(
            discovery
                .reap(&cx, Deadline::after(&cx, Duration::from_secs(1)).unwrap())
                .await
                .unwrap()
                .success()
        );
    });
}
#[test]
fn blocked_native_selection_obeys_parent_deadline_and_abandoned_selection_kills_child() {
    runtime(|cx| async move {
        use frd::worker::{Deadline, Error as ParentError, Worker as Supervised};
        use std::time::{Duration, Instant};
        let display = Display::start();
        for abandon in [false, true] {
            let discovery = Supervised::discover_capture(
                &cx,
                supervised_launch(&display, Role::Capture),
                Deadline::after(&cx, Duration::from_secs(1)).unwrap(),
            )
            .await
            .unwrap();
            let screen = discovery.screens().entries()[0];
            let pid = discovery.id().unwrap();
            assert!(
                Command::new("kill")
                    .args(["-STOP", &display.child.id().to_string()])
                    .status()
                    .unwrap()
                    .success()
            );
            let task = discovery.configure(
                &cx,
                screen,
                configuration(),
                Deadline::after(&cx, Duration::from_millis(30)).unwrap(),
            );
            if abandon {
                drop(task);
            } else {
                assert!(matches!(task.await, Err(ParentError::Deadline)));
            }
            assert!(
                Command::new("kill")
                    .args(["-CONT", &display.child.id().to_string()])
                    .status()
                    .unwrap()
                    .success()
            );
            let until = Instant::now() + Duration::from_secs(1);
            while std::path::Path::new(&format!("/proc/{pid}")).exists() {
                assert!(Instant::now() < until, "abandoned capture child survived");
                asupersync::time::sleep(cx.now(), Duration::from_millis(5)).await;
            }
        }
    });
}

#[cfg(feature = "linux-displays")]
#[test]
fn screen_discovery_never_accepts_monitor_configuration_on_the_same_child() {
    let display = Display::start();
    let mut child = Worker::start(&display, Role::Capture);
    let reply = child.transact(Kind::DiscoverCapture, vec![]);
    assert_eq!(reply.header.kind, Kind::CaptureScreens);
    let screens = capture::Screens::decode(reply.body()).unwrap();
    let mut inventory =
        fr_native::displays::X11Inventory::open(&display.name, ProtocolLimits::ABSOLUTE).unwrap();
    let monitors = inventory.catalog().unwrap();
    let selection = monitors.selection(monitors.displays()[0].handle).unwrap();
    let body =
        capture::monitors::encode_configuration(configuration(), selection, monitors).unwrap();
    let refusal = child.transact(Kind::ConfigureMonitor, body);
    assert_eq!(refusal.header.kind, Kind::Refused);
    assert_eq!(refusal.body(), &(Error::WrongState as u16).to_be_bytes());
    assert!(!child.child.wait().unwrap().success());
    assert_eq!(screens.entries()[0].width, 320);
}

#[cfg(feature = "linux-displays")]
#[test]
fn monitor_discovery_never_accepts_screen_configuration_on_the_same_child() {
    let display = Display::start();
    let mut child = Worker::start(&display, Role::Capture);
    let reply = child.transact(Kind::DiscoverMonitors, vec![]);
    assert_eq!(reply.header.kind, Kind::CaptureMonitors);
    let monitors =
        capture::monitors::decode_catalog(reply.body(), &ProtocolLimits::ABSOLUTE).unwrap();
    let screens =
        fr_native::X11Screens::open(Some(&display.name), ProtocolLimits::ABSOLUTE).unwrap();
    let body = capture::configure(configuration(), screens.catalog().entries()[0]).unwrap();
    let refusal = child.transact(Kind::ConfigureCapture, body);
    assert_eq!(refusal.header.kind, Kind::Refused);
    assert_eq!(refusal.body(), &(Error::WrongState as u16).to_be_bytes());
    assert!(!child.child.wait().unwrap().success());
    assert_eq!(monitors.displays()[0].pixel_width, 320);
}

#[cfg(not(feature = "linux-displays"))]
#[test]
fn unavailable_monitor_profile_refuses_without_falling_back_to_screen_capture() {
    let display = Display::start();
    let mut child = Worker::start(&display, Role::Capture);
    let refusal = child.transact(Kind::DiscoverMonitors, vec![]);
    assert_eq!(refusal.header.kind, Kind::Refused);
    assert_eq!(refusal.body(), &(Error::Unsupported as u16).to_be_bytes());
    assert!(!child.child.wait().unwrap().success());
}

#![cfg(all(target_os = "linux", feature = "linux-media"))]
use asupersync::{cx::Cx, runtime::RuntimeBuilder, types::Budget};
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::{CodecConfigurationGeneration, RecoveryGeneration, RemoteSessionId},
};
use fr_media::{
    access_unit::FrameId,
    delivery::{
        MediaBindings, MediaBudget, MediaEpoch, ReceiveConfig, ReceivePipeline, ReceivePolicy,
        SendPolicy,
    },
    worker::{Backend, Configuration, Kind, Role},
};
use fr_native::{BgraFrame, X11Surface};
use fr_wire::MediaLimits;
use frd::{
    media::{
        CaptureSource, Error as MediaError, ObservationControl, PresentationStage, Presenter,
        Subscription, host_now,
    },
    worker::{Deadline, Launch, State},
};
use std::{
    io::{BufRead, BufReader},
    path::Path,
    process::{Child, Command, Stdio},
    time::Duration,
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
                "-nolisten",
                "tcp",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut n = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut n)
            .unwrap();
        Self {
            child,
            name: format!(":{}", n.trim().parse::<u32>().unwrap()),
        }
    }
}
impl Drop for Display {
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
fn pattern(frame: u8, limits: &fr_core::limits::ProtocolLimits) -> BgraFrame {
    let mut pixels = vec![0; 320 * 240 * 4];
    for (i, p) in pixels.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        p.copy_from_slice(&[u8::try_from(i % 200).unwrap(), 40, frame * 25, 255]);
    }
    BgraFrame::new(320, 240, pixels, limits).unwrap()
}
#[test]
fn mutable_worker_access_fences_swapped_capture_provenance() {
    let displays = [Display::start(), Display::start()];
    let limits = configuration().limits().unwrap();
    let mut surfaces = displays
        .each_ref()
        .map(|display| X11Surface::presenter(Some(&display.name), 320, 240, limits).unwrap());
    surfaces[0].present(&pattern(1, &limits)).unwrap();
    surfaces[1].present(&pattern(7, &limits)).unwrap();
    assert_ne!(
        surfaces[0].snapshot().unwrap().pixels(),
        surfaces[1].snapshot().unwrap().pixels(),
        "the real capture sources must show different pixels"
    );
    let runtime = RuntimeBuilder::new()
        .worker_threads(1)
        .blocking_threads(1, 2)
        .enable_platform_reactor(true)
        .build()
        .unwrap();
    runtime.block_on(async {
        let cleanup = Cx::current().unwrap();
        for force_idr in [false, true] {
            let session = runtime.request_cx_with_budget(Budget::INFINITE);
            let mut authority = SessionAuthority::new(
                RemoteSessionId::from_raw(66),
                AuthorityPolicy::plan_defaults(),
            );
            authority.mark_capabilities_checked().unwrap();
            authority
                .authorize_observation(host_now(&session).unwrap())
                .unwrap();
            let control = ObservationControl::new(session, authority).unwrap();
            verify_swapped_capture(&control, &cleanup, &displays, force_idr).await;
            control.revoke();
        }
    });
}

async fn verify_swapped_capture(
    control: &ObservationControl,
    cleanup: &Cx,
    displays: &[Display; 2],
    force_idr: bool,
) {
    let config = configuration();
    let limits = config.limits().unwrap();
    let image = Path::new(env!("CARGO_BIN_EXE_fr-media-worker"));
    let launch_a = Launch::new(image, &displays[0].name, None, Role::Capture, 11).unwrap();
    let launch_b = Launch::new(image, &displays[1].name, None, Role::Capture, 12).unwrap();
    let mut a = CaptureSource::start(control, launch_a, config)
        .await
        .unwrap();
    let mut b = CaptureSource::start(control, launch_b, config)
        .await
        .unwrap();
    let initial_a = a.capture_if_changed(control, true).await.unwrap();
    let initial_b = b.capture_if_changed(control, true).await.unwrap();
    for initial in [&initial_a, &initial_b] {
        assert_eq!(initial.frame(), FrameId::FIRST);
        let encoded = initial.encoded().unwrap();
        assert!(encoded.is_idr());
        assert_eq!(encoded.config_generation(), config.generation);
    }
    let mut subscription = Subscription::new(
        control.clone(),
        MediaLimits::new(limits, 1150, 16384, 64).unwrap(),
        MediaBindings::new(1, 2, 3, 4).unwrap(),
        MediaEpoch {
            configuration: config.generation,
            recovery: RecoveryGeneration::INITIAL,
        },
        SendPolicy::default(),
    )
    .unwrap();
    subscription.enqueue_capture(initial_a).unwrap();
    let unchanged = a.capture_if_changed(control, false).await.unwrap();
    assert!(unchanged.is_unchanged());
    assert_eq!(unchanged.frame(), FrameId::FIRST);
    subscription.enqueue_capture(unchanged).unwrap();

    // Safe mutable access can replace the actual native worker without changing
    // either wrapper's configuration or numeric frame history.
    std::mem::swap(a.worker_mut(), b.worker_mut());
    let result = a.capture_if_changed(control, force_idr).await;
    if force_idr {
        // Fresh pixels from the replacement may establish a NEW source, but the
        // previous subscription must not silently adopt them under its old proof.
        let replacement = result.unwrap();
        assert!(replacement.encoded().unwrap().is_idr());
        assert_eq!(replacement.frame(), FrameId::from_raw(2));
        assert!(matches!(
            subscription.enqueue_capture(replacement),
            Err(MediaError::InvalidFrame)
        ));
    } else {
        // B's real comparison refers to B's frame zero, not A's different pixels.
        assert!(
            matches!(&result, Err(MediaError::InvalidFrame)),
            "swapped worker minted an old-source observation: {result:?}"
        );
        assert!(matches!(
            a.capture_if_changed(control, false).await,
            Err(MediaError::Worker(frd::worker::Error::Unavailable))
        ));
    }
    finish_swapped_capture(cleanup, &mut a, force_idr).await;
    finish_swapped_capture(cleanup, &mut b, true).await;
}

async fn finish_swapped_capture(cleanup: &Cx, capture: &mut CaptureSource, running: bool) {
    let worker = capture.worker_mut();
    assert_eq!(
        worker.state(),
        if running {
            State::Running
        } else {
            State::Poisoned
        }
    );
    if running {
        worker
            .request(
                cleanup,
                Kind::Stop,
                vec![],
                Deadline::after(cleanup, Duration::from_millis(500)).unwrap(),
            )
            .await
            .unwrap();
    }
    assert_eq!(
        worker
            .reap(
                cleanup,
                Deadline::after(cleanup, Duration::from_millis(500)).unwrap()
            )
            .await
            .unwrap()
            .success(),
        running
    );
}
#[test]
fn authority_to_supervised_capture_wire_and_presentation_then_revoke() {
    let source_display = Display::start();
    let viewer_display = Display::start();
    let config = configuration();
    let limits = config.limits().unwrap();
    let mut source = X11Surface::presenter(Some(&source_display.name), 320, 240, limits).unwrap();
    let runtime = RuntimeBuilder::new()
        .worker_threads(1)
        .blocking_threads(1, 2)
        .enable_platform_reactor(true)
        .build()
        .unwrap();
    let session = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let cleanup = Cx::current().unwrap();
        let mut authority = SessionAuthority::new(
            RemoteSessionId::from_raw(55),
            AuthorityPolicy::plan_defaults(),
        );
        authority.mark_capabilities_checked().unwrap();
        authority
            .authorize_observation(host_now(&session).unwrap())
            .unwrap();
        let control = ObservationControl::new(session, authority).unwrap();
        let image = Path::new(env!("CARGO_BIN_EXE_fr-media-worker"));
        let launch = Launch::new(image, &source_display.name, None, Role::Capture, 11).unwrap();
        let mut capture = CaptureSource::start(&control, launch, config)
            .await
            .unwrap();
        let bootstrap = capture.capture(&control, true).await.unwrap();
        let mut admission =
            fr_media::hevc::HevcGuard::new(config.codec().unwrap(), limits, 4).unwrap();
        admission
            .validate_length_prefixed(bootstrap.bytes(), true)
            .unwrap();
        let record = admission.decoder_record().unwrap();
        drop(bootstrap);
        let wire = MediaLimits::new(limits, 1150, 16384, 64).unwrap();
        let bindings = MediaBindings::new(1, 2, 3, 4).unwrap();
        let epoch = MediaEpoch {
            configuration: config.generation,
            recovery: RecoveryGeneration::INITIAL,
        };
        let mut subscription = Subscription::new(
            control.clone(),
            wire,
            bindings,
            epoch,
            SendPolicy::default(),
        )
        .unwrap();
        let budget = MediaBudget::new(&limits).unwrap();
        let mut receiver = ReceivePipeline::new(
            ReceiveConfig {
                limits: wire,
                bindings,
                epoch,
                policy: ReceivePolicy::default(),
            },
            budget.clone(),
        )
        .unwrap();
        let launch = Launch::new(image, &viewer_display.name, None, Role::Present, 12).unwrap();
        let mut presenter = Presenter::start(&cleanup, launch, config, &record, &mut receiver)
            .await
            .unwrap();
        let mut output = X11Surface::capture(Some(&viewer_display.name), limits).unwrap();
        exercise_pair(
            &control,
            &mut capture,
            &mut presenter,
            &mut receiver,
            &mut subscription,
            &mut source,
            &mut output,
        )
        .await;
        // Revocation fences both future captures and already queued media. No codec
        // response or network acknowledgement can renew it.
        source.present(&pattern(8, &limits)).unwrap();
        subscription
            .enqueue(capture.capture(&control, false).await.unwrap())
            .unwrap();
        let mut packet = [0; 1150];
        let queued = subscription.next_packet(&mut packet).unwrap().unwrap();
        control.revoke();
        assert!(subscription.authorize_write(&queued).is_err());
        assert!(subscription.next_packet(&mut packet).is_err());
        assert!(capture.capture(&control, false).await.is_err());
        stop_workers(&cleanup, &mut capture, &mut presenter).await;
    });
}

async fn exercise_pair(
    control: &ObservationControl,
    capture: &mut CaptureSource,
    presenter: &mut Presenter,
    receiver: &mut ReceivePipeline,
    subscription: &mut Subscription,
    source: &mut X11Surface,
    output: &mut X11Surface,
) {
    let cleanup = Cx::current().unwrap();
    let limits = configuration().limits().unwrap();
    let mut previous = None;
    for frame in 0..8_u8 {
        source.present(&pattern(frame, &limits)).unwrap();
        let unit = capture
            .capture(control, frame == 0 || frame == 4)
            .await
            .unwrap();
        assert_eq!(unit.frame().as_raw(), u64::from(frame) + 1);
        subscription.enqueue(unit).unwrap();
        let mut packet = [0; 1150];
        while let Some(offer) = subscription.next_packet(&mut packet).unwrap() {
            subscription.authorize_write(&offer).unwrap();
            receiver
                .receive(
                    offer.channel(),
                    &packet[..offer.byte_len()],
                    host_now(&cleanup).unwrap().as_micros(),
                )
                .unwrap();
        }
        if frame == 3 {
            asupersync::time::sleep(
                cleanup.timer_driver().unwrap().now(),
                Duration::from_millis(65),
            )
            .await;
        }
        let receipt = presenter
            .present_next(&cleanup, receiver)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(receipt.frame.as_raw(), u64::from(frame) + 1);
        assert_eq!(
            receipt.stage,
            if frame == 3 {
                PresentationStage::DecodedOnly
            } else {
                PresentationStage::SubmittedToCompositor
            }
        );
        assert_eq!(
            receiver.budget_usage().pictures,
            0,
            "decoder reservation leaked after IPC completion"
        );
        let pixels = output.snapshot().unwrap();
        if let Some(old) = previous {
            if frame == 3 {
                assert_eq!(pixels.pixels(), old, "late reference was displayed");
            } else {
                assert_ne!(pixels.pixels(), old);
            }
        }
        previous = Some(pixels.pixels().to_vec());
    }
}

async fn stop_workers(cleanup: &Cx, capture: &mut CaptureSource, presenter: &mut Presenter) {
    let worker = capture.worker_mut();
    worker
        .request(
            cleanup,
            Kind::Stop,
            vec![],
            Deadline::after(cleanup, Duration::from_millis(500)).unwrap(),
        )
        .await
        .unwrap();
    assert!(
        worker
            .reap(
                cleanup,
                Deadline::after(cleanup, Duration::from_millis(500)).unwrap()
            )
            .await
            .unwrap()
            .success()
    );

    presenter
        .stop(
            cleanup,
            Deadline::after(cleanup, Duration::from_millis(500)).unwrap(),
        )
        .await
        .unwrap();
    assert!(
        presenter
            .reap(
                cleanup,
                Deadline::after(cleanup, Duration::from_millis(500)).unwrap()
            )
            .await
            .unwrap()
            .success()
    );
}

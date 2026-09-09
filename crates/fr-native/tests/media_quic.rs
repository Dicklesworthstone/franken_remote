#![cfg(all(target_os = "linux", feature = "linux-media"))]
//! Two private X servers, real supervised codecs and real UDP/TLS QUIC.
//! Fixture authority/routes/hvcC are local setup, not tailnet admission or the
//! negotiated network startup protocol. Loss is injected AFTER QUIC reception.
#[path = "../../fr-transport/tests/support/mod.rs"]
mod network;
use asupersync::{cx::Cx, runtime::RuntimeBuilder, types::Budget};
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::*,
};
use fr_media::{
    delivery::*,
    hevc::HevcGuard,
    worker::{Backend, Configuration, Kind, Role},
};
use fr_native::{BgraFrame, X11Surface};
use fr_transport::quic::{self, Disposition, Messages, Policy, Route, StreamRoute};
use fr_wire::{Channel, MediaLimits, Record, decode_fragment};
use frd::{
    media::{
        CaptureSource, ObservationControl, PresentationStage, Presenter, Subscription, host_now,
    },
    media_egress::{Egress, Lane, Progress},
    media_quic::{QuicEgress, RepairAdmission, Routes},
    worker::{Deadline, Launch},
};
use std::{
    io::{BufRead, BufReader, Read},
    path::Path,
    process::{Child, Command, Stdio},
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
fn configuration() -> Configuration {
    Configuration {
        width: 320,
        height: 240,
        fps: 30,
        backend: Backend::SoftwareExplicit,
        bitrate: 4_000_000,
        max_access_unit_bytes: 1_048_576,
        generation: CodecConfigurationGeneration::INITIAL,
    }
}
fn paint(surface: &mut X11Surface, index: u8) -> [u8; 3] {
    let mut pixels = Vec::with_capacity(320 * 240 * 4);
    for y in 0u16..240 {
        for x in 0u16..320 {
            let tile = u8::try_from((x / 16 + 3 * (y / 16)) % 16).unwrap();
            pixels.extend_from_slice(&[
                tile.wrapping_mul(13).wrapping_add(index.wrapping_mul(17)),
                tile.wrapping_mul(7).wrapping_add(index.wrapping_mul(29)),
                tile.wrapping_mul(11).wrapping_add(index.wrapping_mul(19)),
                255,
            ]);
        }
    }
    let at = (123 * 320 + 163) * 4;
    let expected = pixels[at..at + 3].try_into().unwrap();
    surface
        .present(&BgraFrame::new(320, 240, pixels, &configuration().limits().unwrap()).unwrap())
        .unwrap();
    expected
}
#[derive(Clone, Copy)]
enum Loss {
    None,
    Fragment,
    FinalPicture,
}
#[derive(Default, Debug)]
struct Counts {
    frames: usize,
    records: usize,
    backpressure: usize,
    drops: usize,
    repairs: usize,
    peak_transport_bytes: usize,
}
struct Native {
    source: X11Surface,
    capture: CaptureSource,
    presenter: Presenter,
    readback: X11Surface,
    network: MediaNetwork,
    _displays: [Display; 2],
}
struct FrameLoss {
    mode: Loss,
    index: u8,
    repair_seen: bool,
    dropped_fragment: bool,
    pending_repair: Option<(Vec<u8>, u64)>,
}
struct MediaNetwork {
    control: ObservationControl,
    pair: network::Pair,
    routes: Routes,
    bindings: MediaBindings,
    wire: MediaLimits,
    sender: QuicEgress,
    receiver: ReceivePipeline,
    counts: Counts,
}
impl MediaNetwork {
    async fn new(cx: &Cx, control: ObservationControl) -> Self {
        let cfg = configuration();
        let limits = cfg.limits().unwrap();
        let policy = Policy {
            retained_send_records: 1,
            critical_send_records: 1,
            ..Policy::default()
        };
        let mut pair = network::pair(cx, policy).await;
        network::drive(cx, &mut pair).await;
        let bindings = MediaBindings::new(1, 2, 3, 4).unwrap();
        let routes = Routes::new(
            bindings,
            pair.host_routes[0],
            pair.host_routes[1],
            pair.video,
            pair.host_routes[2],
        )
        .unwrap();
        let wire = MediaLimits::new(limits, 1150, 16384, 64).unwrap();
        let epoch = MediaEpoch {
            configuration: cfg.generation,
            recovery: RecoveryGeneration::INITIAL,
        };
        let subscription = Subscription::new(
            control.clone(),
            wire,
            bindings,
            epoch,
            SendPolicy::default(),
        )
        .unwrap();
        let sender = QuicEgress::new(Egress::new(subscription), routes);
        let mut receiver = ReceivePipeline::new(
            ReceiveConfig {
                limits: wire,
                bindings,
                epoch,
                // Bounded correctness profile, not a 50ms latency benchmark.
                policy: ReceivePolicy {
                    display_budget_micros: 250_000,
                    ..ReceivePolicy::default()
                },
            },
            MediaBudget::new(&limits).unwrap(),
        )
        .unwrap();
        receiver.decoder_configured(network::clock(cx)).unwrap();
        Self {
            control,
            pair,
            routes,
            bindings,
            wire,
            sender,
            receiver,
            counts: Counts::default(),
        }
    }
    async fn drive(&mut self, cx: &Cx) {
        for lane in [Lane::Original, Lane::Repair] {
            for _ in 0..32 {
                match self
                    .sender
                    .transmit(cx, &mut self.pair.server, lane)
                    .unwrap()
                {
                    Progress::Accepted(_) => self.counts.records += 1,
                    Progress::Pending(_) => {
                        self.counts.backpressure += 1;
                        break;
                    }
                    Progress::Idle => break,
                }
            }
        }
        let (a, b) = Box::pin(network::both(
            self.sender
                .drive(cx, &mut self.pair.server, Duration::from_millis(1)),
            self.pair
                .client
                .drive(cx, Duration::from_millis(1), || true),
        ))
        .await;
        a.unwrap();
        b.unwrap();
        let usage = self.pair.server.usage();
        self.counts.peak_transport_bytes = self
            .counts
            .peak_transport_bytes
            .max(usage.retained_send_upper_bound);
        assert!(usage.critical_send_records <= 1);
        assert!(usage.retained_send_records - usage.critical_send_records <= 1);
        assert!(usage.retained_send_records <= 2);
        assert!(usage.critical_send_bytes <= Policy::default().critical_send_bytes);
        assert!(
            usage.retained_send_upper_bound - usage.critical_send_bytes
                <= Policy::default().retained_send_bytes
        );
    }
    fn receive(&mut self, cx: &Cx, loss: &mut FrameLoss) {
        self.pair
            .client
            .receive(
                cx,
                || true,
                |route, bytes| {
                    let channel = self.routes.viewer_channel(route).unwrap();
                    if channel == Channel::Video {
                        let packet = Record::decode(
                            bytes,
                            &self.wire,
                            self.bindings.for_channel(channel),
                            channel,
                        )
                        .unwrap();
                        let fragment = decode_fragment(packet, &self.wire).unwrap();
                        let drop = match loss.mode {
                            Loss::None => false,
                            Loss::Fragment => {
                                loss.index == 3 && fragment.index == 0 && !loss.dropped_fragment
                            }
                            Loss::FinalPicture => loss.index == 6 && !loss.repair_seen,
                        };
                        if drop {
                            self.counts.drops += 1;
                            loss.dropped_fragment = true;
                            return Ok(Disposition::Consumed);
                        }
                    }
                    self.receiver
                        .receive(channel, bytes, network::clock(cx))
                        .unwrap();
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
        self.pair
            .server
            .receive(
                cx,
                || self.control.check().is_ok(),
                |route, bytes| {
                    if self.sender.repair(route, bytes).unwrap() == RepairAdmission::Queued {
                        loss.repair_seen = true;
                        self.counts.repairs += 1;
                    }
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
    }
    fn request_repair(&mut self, cx: &Cx, loss: &mut FrameLoss) {
        if loss.pending_repair.is_none() {
            let mut bytes = vec![0; self.wire.record_bytes()];
            if let Some(n) = self
                .receiver
                .repair_request(network::clock(cx), &mut bytes)
                .unwrap()
            {
                bytes.truncate(n);
                loss.pending_repair = Some((bytes, network::clock(cx) + 80_000));
            }
        }
        if let Some((bytes, deadline)) = loss.pending_repair.as_ref() {
            let route = Route::Stream(StreamRoute {
                outbound: true,
                ..self.routes.repair_stream()
            });
            match self.pair.client.send(cx, route, bytes, *deadline, || true) {
                Ok(()) => loss.pending_repair = None,
                Err(quic::Error::Backpressure) => (),
                Err(error) => panic!("repair send: {error:?}"),
            }
        }
    }
}
impl Native {
    async fn start(cx: &Cx) -> Self {
        let host = Display::start();
        let viewer = Display::start();
        let cfg = configuration();
        let limits = cfg.limits().unwrap();
        let mut source = X11Surface::presenter(Some(&host.name), 320, 240, limits).unwrap();
        paint(&mut source, 0);
        let mut auth = SessionAuthority::new(
            RemoteSessionId::from_raw(99),
            AuthorityPolicy::plan_defaults(),
        );
        auth.mark_capabilities_checked().unwrap();
        auth.authorize_observation(host_now(cx).unwrap()).unwrap();
        let control = ObservationControl::new(cx.clone(), auth).unwrap();
        let binary = Path::new(env!("CARGO_BIN_EXE_fr-media-worker"));
        let mut capture = CaptureSource::start(
            &control,
            Launch::new(binary, &host.name, None, Role::Capture, 21).unwrap(),
            cfg,
        )
        .await
        .unwrap();
        let bootstrap = capture.capture(&control, true).await.unwrap();
        let mut guard = HevcGuard::new(cfg.codec().unwrap(), limits, 4).unwrap();
        guard
            .validate_length_prefixed(bootstrap.bytes(), true)
            .unwrap();
        let presenter = Presenter::start(
            cx,
            Launch::new(binary, &viewer.name, None, Role::Present, 22).unwrap(),
            cfg,
            &guard.decoder_record().unwrap(),
        )
        .await
        .unwrap();
        let readback = X11Surface::capture(Some(&viewer.name), limits).unwrap();
        let network = MediaNetwork::new(cx, control).await;
        Self {
            source,
            capture,
            presenter,
            readback,
            network,
            _displays: [host, viewer],
        }
    }
    async fn frame(&mut self, cx: &Cx, index: u8, mode: Loss) {
        let expected = paint(&mut self.source, index);
        let unit = self
            .capture
            .capture(&self.network.control, index == 1)
            .await
            .unwrap();
        let frame_id = unit.frame();
        self.network.sender.enqueue(unit).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut loss = FrameLoss {
            mode,
            index,
            repair_seen: false,
            dropped_fragment: false,
            pending_repair: None,
        };
        loop {
            assert!(
                Instant::now() < deadline,
                "media stalled: {:?}",
                self.network.counts
            );
            self.network.drive(cx).await;
            self.network.receive(cx, &mut loss);
            self.network.request_repair(cx, &mut loss);
            if let Some(receipt) = self
                .presenter
                .present_next(cx, &mut self.network.receiver)
                .await
                .unwrap()
            {
                assert_eq!(receipt.frame, frame_id);
                assert_eq!(receipt.stage, PresentationStage::SubmittedToCompositor);
                let image = self.readback.snapshot().unwrap();
                for (actual, expected) in image.pixels()[(123 * 320 + 163) * 4..][..3]
                    .iter()
                    .zip(expected)
                {
                    assert!(
                        actual.abs_diff(expected) <= 16,
                        "unexpected decoded tile at frame {index}"
                    );
                }
                self.network.counts.frames += 1;
                break;
            }
        }
    }
    async fn close(&mut self, cx: &Cx) {
        self.network.sender.close();
        self.network.pair.client.close();
        self.network.pair.server.close();
        assert_eq!(self.network.sender.cache_usage().bytes, 0);
        for worker in [self.capture.worker_mut(), self.presenter.worker_mut()] {
            worker
                .request(
                    cx,
                    Kind::Stop,
                    vec![],
                    Deadline::after(cx, Duration::from_millis(500)).unwrap(),
                )
                .await
                .unwrap();
            assert!(
                worker
                    .reap(cx, Deadline::after(cx, Duration::from_millis(500)).unwrap())
                    .await
                    .unwrap()
                    .success()
            );
        }
    }
}
async fn scenario(loss: Loss) {
    let cx = Cx::current().unwrap();
    let mut native = Native::start(&cx).await;
    for index in 1..=6 {
        native.frame(&cx, index, loss).await;
    }
    let counts = &native.network.counts;
    assert_eq!(counts.frames, 6);
    assert!(counts.backpressure > 0);
    assert!(counts.records > counts.frames);
    if !matches!(loss, Loss::None) {
        assert!(counts.drops > 0 && counts.repairs > 0);
    }
    assert!(native.network.sender.allocated_record_bytes() <= native.network.wire.record_bytes());
    eprintln!("real_x11_hevc_quic {counts:?}");
    native.close(&cx).await;
}
fn run(loss: Loss) {
    // Keep measured deadline fixtures independent of parallel codec startup load.
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _lock = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let runtime = RuntimeBuilder::new()
        .worker_threads(2)
        .blocking_threads(1, 2)
        .enable_platform_reactor(true)
        .build()
        .unwrap();
    runtime.block_on(scenario(loss));
}
#[test]
fn native_capture_hevc_quic_and_visible_readback_survive_actual_backpressure() {
    run(Loss::None);
}
#[test]
fn native_quic_selective_repair_preserves_a_dropped_reference_fragment() {
    run(Loss::Fragment);
}
#[test]
fn native_quic_progress_recovers_an_entire_lost_final_picture_before_idle() {
    run(Loss::FinalPicture);
}
async fn sender_fixture(cx: &Cx) -> (QuicEgress, network::Pair, ObservationControl, Routes) {
    let mut auth = SessionAuthority::new(
        RemoteSessionId::from_raw(41),
        AuthorityPolicy::plan_defaults(),
    );
    auth.mark_capabilities_checked().unwrap();
    auth.authorize_observation(host_now(cx).unwrap()).unwrap();
    let control = ObservationControl::new(cx.clone(), auth).unwrap();
    let pair = network::pair(
        cx,
        Policy {
            retained_send_records: 1,
            critical_send_records: 1,
            ..Policy::default()
        },
    )
    .await;
    let bindings = MediaBindings::new(1, 2, 3, 4).unwrap();
    let routes = Routes::new(
        bindings,
        pair.host_routes[0],
        pair.host_routes[1],
        pair.video,
        pair.host_routes[2],
    )
    .unwrap();
    let wire = MediaLimits::new(configuration().limits().unwrap(), 1150, 16384, 64).unwrap();
    let epoch = MediaEpoch {
        configuration: configuration().generation,
        recovery: RecoveryGeneration::INITIAL,
    };
    let subscription = Subscription::new(
        control.clone(),
        wire,
        bindings,
        epoch,
        SendPolicy::default(),
    )
    .unwrap();
    let mut sender = QuicEgress::new(Egress::new(subscription), routes);
    // Payload is intentionally opaque here: these two tests assert native socket
    // cancellation and retained-buffer ownership, not HEVC decoding.
    sender
        .enqueue(
            fr_media::access_unit::EncodedAccessUnit::new(
                wire.protocol(),
                fr_media::access_unit::FrameId::FIRST,
                fr_media::access_unit::FrameKind::Idr {
                    recovery: epoch.recovery,
                },
                epoch.configuration,
                network::clock(cx),
                vec![0; 4096],
            )
            .unwrap(),
        )
        .unwrap();
    (sender, pair, control, routes)
}
#[test]
fn revoke_between_actual_quic_admissions_closes_retained_media_and_connection() {
    let runtime = network::runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let (mut sender, mut pair, control, _) = sender_fixture(&cx).await;
        assert!(matches!(
            sender
                .transmit(&cx, &mut pair.server, Lane::Original)
                .unwrap(),
            Progress::Accepted(_)
        ));
        // Progress and bulk each have their own one-record reservation. Both
        // admit once; the next recovery chunk must remain owned under pressure.
        assert!(matches!(
            sender
                .transmit(&cx, &mut pair.server, Lane::Original)
                .unwrap(),
            Progress::Accepted(_)
        ));
        assert!(matches!(
            sender
                .transmit(&cx, &mut pair.server, Lane::Original)
                .unwrap(),
            Progress::Pending(_)
        ));
        assert!(sender.pending().is_some());
        assert!(pair.server.usage().retained_send_upper_bound > 0);
        control.revoke();
        assert!(
            sender
                .transmit(&cx, &mut pair.server, Lane::Original)
                .is_err()
        );
        assert!(sender.is_closed() && pair.server.is_closed());
        assert_eq!(sender.cache_usage().bytes, 0);
        assert_eq!(sender.allocated_record_bytes(), 0);
        assert_eq!(pair.server.usage().retained_send_upper_bound, 0);
    });
}
#[test]
fn dropped_live_drive_is_terminal_for_media_and_quic_without_another_poll() {
    use std::{
        future::Future,
        task::{Context, Poll, Waker},
    };
    let runtime = network::runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let (mut sender, mut pair, control, _) = sender_fixture(&cx).await;
        assert!(sender.cache_usage().bytes > 0);
        let mut driving = Box::pin(sender.drive(&cx, &mut pair.server, Duration::from_millis(100)));
        assert!(matches!(
            driving
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop())),
            Poll::Pending
        ));
        drop(driving);
        assert!(sender.is_closed() && pair.server.is_closed());
        assert_eq!(sender.cache_usage().bytes, 0);
        assert_eq!(sender.allocated_record_bytes(), 0);
        assert!(
            control.check().is_ok(),
            "closing one subscription must not revoke shared observation"
        );
    });
}
#[test]
fn route_binding_direction_and_kinds_cannot_be_substituted() {
    let runtime = network::runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let (mut sender, pair, _, routes) = sender_fixture(&cx).await;
        let bindings = MediaBindings::new(1, 2, 3, 4).unwrap();
        for bad in [
            StreamRoute {
                outbound: false,
                ..pair.host_routes[0]
            },
            StreamRoute {
                binding: 99,
                ..pair.host_routes[0]
            },
            StreamRoute {
                messages: Messages::Exact(0x35),
                ..pair.host_routes[0]
            },
            StreamRoute {
                stream: pair.host_routes[1].stream,
                ..pair.host_routes[0]
            },
        ] {
            assert!(
                Routes::new(
                    bindings,
                    bad,
                    pair.host_routes[1],
                    pair.video,
                    pair.host_routes[2]
                )
                .is_err()
            );
        }
        assert!(
            routes
                .viewer_channel(Route::Stream(pair.host_routes[0]))
                .is_err()
        );
        assert!(routes.viewer_channel(Route::Datagram(pair.video)).is_err());
        assert_eq!(
            sender.repair(Route::Stream(pair.host_routes[0]), b""),
            Err(frd::media_quic::Error::InvalidRoutes)
        );
    });
}

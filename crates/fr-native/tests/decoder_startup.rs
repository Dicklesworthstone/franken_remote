#![cfg(all(target_os = "linux", feature = "linux-media"))]
//! Real X11 capture, network-carried configuration, supervised HEVC and UDP/TLS.
//! Tailnet admission and capability selection use explicit local fixtures. All
//! media channels attach over QUIC; no public listener or full login is claimed.
#[path = "../../fr-transport/tests/support/mod.rs"]
#[allow(dead_code)]
mod network;
use asupersync::{cx::Cx, runtime::RuntimeBuilder};
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::*,
};
use fr_media::{
    delivery::*,
    worker::{Backend, Configuration, Role},
};
use fr_native::{BgraFrame, X11Surface};
use fr_transport::quic::{
    self, Disposition, Messages, Policy, Priority, QuicRecords, Route, StreamRoute,
};
use fr_wire::{
    Channel, MediaLimits,
    decoder::{self, Binding, Message},
    input::{InputDelivery as T, InputDirection as D},
    negotiation::{Capability, ControlBinding, Offer},
};
use frd::{
    media::{
        CaptureSource, ObservationControl,
        decoder_startup::{Error, Host, Setup, Viewer},
    },
    media_egress::{Lane, Progress},
    media_quic::{NegotiatedMedia, QuicEgress, RepairAdmission},
    worker::Launch,
};
use std::{
    future::Future,
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
fn binding() -> Binding {
    Binding {
        parent: ControlBinding {
            id: 8,
            host_boot: HostBootId::from_raw(1),
            os_session: OsSessionId::from_raw(2),
            remote_session: RemoteSessionId::from_raw(3),
        },
        display: 4,
        geometry: DisplayGeometryGeneration::INITIAL,
        configuration: CodecConfigurationGeneration::INITIAL,
        recovery: RecoveryGeneration::INITIAL,
        viewport: ViewportMappingGeneration::INITIAL,
    }
}
fn launch(display: &Display, epoch: u128, role: Role) -> Launch {
    Launch::new(
        Path::new(env!("CARGO_BIN_EXE_fr-media-worker")),
        &display.name,
        None,
        role,
        epoch,
    )
    .unwrap()
}
fn run<F, Fut>(f: F)
where
    F: FnOnce(Cx) -> Fut,
    Fut: Future<Output = ()>,
{
    let runtime = RuntimeBuilder::new()
        .worker_threads(2)
        .enable_platform_reactor(true)
        .blocking_threads(2, 4)
        .build()
        .unwrap();
    runtime.block_on(async {
        let cx = Cx::current().unwrap();
        asupersync::time::timeout(cx.now(), Duration::from_secs(10), f(cx))
            .await
            .expect("decoder startup test exceeded bounded deadline");
    });
}
struct Link {
    client: QuicRecords,
    host: QuicRecords,
    config: StreamRoute,
    progress: StreamRoute,
    replies: StreamRoute,
    wire: MediaLimits,
    receive: ReceiveConfig,
    host_channels: NegotiatedMedia,
    client_channels: NegotiatedMedia,
}
impl Link {
    async fn new(cx: &Cx) -> Self {
        let (c, h) = network::native_pair(cx, "localhost", quic::ALPN).await;
        let (mut c, mut h) = (c.unwrap(), h.unwrap());
        // Only the session-control pair is installed by the startup fixture.
        // No decoder, recovery, feedback or datagram route exists initially.
        let config = StreamRoute {
            stream: h.connection_mut().open_uni_stream(cx).unwrap(),
            binding: 7,
            messages: Messages::SessionControl,
            priority: Priority::Critical,
            outbound: true,
            maximum: 4096,
        };
        let replies = StreamRoute {
            stream: c.connection_mut().open_uni_stream(cx).unwrap(),
            outbound: false,
            ..config
        };
        let streams = [config, replies];
        let reverse = streams.map(|r| StreamRoute {
            outbound: !r.outbound,
            ..r
        });
        let policy = Policy {
            retained_send_records: 1,
            critical_send_records: 1,
            ..Policy::default()
        };
        let mut host = QuicRecords::new(h, cx, &streams, &[], policy).unwrap();
        let mut client = QuicRecords::new(c, cx, &reverse, &[], policy).unwrap();
        let control = (config, replies);
        let (hc, cc) = attach_role(
            cx,
            &mut host,
            &mut client,
            control,
            fr_wire::attachment::MediaRole::Configuration,
            binding(),
        )
        .await;
        let mut recovery_binding = binding();
        recovery_binding.parent.id = 9;
        let (hr, cr) = attach_role(
            cx,
            &mut host,
            &mut client,
            control,
            fr_wire::attachment::MediaRole::Recovery,
            recovery_binding,
        )
        .await;
        let mut video_binding = binding();
        video_binding.parent.id = 10;
        let (hv, cv) = attach_role(
            cx,
            &mut host,
            &mut client,
            control,
            fr_wire::attachment::MediaRole::Video,
            video_binding,
        )
        .await;
        let selection = selected_attachment();
        let host_channels = NegotiatedMedia::new(&host, &selection, &hc, &hr, &hv).unwrap();
        let client_channels = NegotiatedMedia::new(&client, &selection, &cc, &cr, &cv).unwrap();
        assert!(NegotiatedMedia::new(&host, &selection, &hc, &hr, &cv).is_err());
        assert!(NegotiatedMedia::new(&host, &selection, &hc, &hv, &hr).is_err());
        let mut altered = selection.clone();
        altered.limits =
            fr_core::limits::ProtocolLimits::with_overrides(fr_core::limits::LimitOverrides {
                max_control_message_bytes: Some(512),
                ..Default::default()
            })
            .unwrap();
        assert!(NegotiatedMedia::new(&host, &altered, &hc, &hr, &hv).is_err());
        let (cfg, vid) = (
            hc.completed_on(&host).unwrap(),
            hv.completed_on(&host).unwrap(),
        );
        let wire = host_channels.limits();
        assert_eq!(wire.record_bytes(), 1150);
        let receive = client_channels
            .receiver_config(
                &client,
                ReceivePolicy {
                    display_budget_micros: 250_000,
                    ..ReceivePolicy::default()
                },
            )
            .unwrap();
        Self {
            client,
            host,
            config: cfg.outbound,
            replies: cfg.inbound,
            progress: vid.outbound,
            wire,
            receive,
            host_channels,
            client_channels,
        }
    }
    fn setup(&self, host: bool, timeout: Duration) -> Setup {
        let mut unselected = selected_attachment();
        unselected.capabilities.clear();
        assert!(Setup::new(binding(), self.config, self.replies, &unselected, timeout).is_err());
        if host {
            self.host_channels
                .decoder_setup(&self.host, timeout)
                .unwrap()
        } else {
            self.client_channels
                .decoder_setup(&self.client, timeout)
                .unwrap()
        }
    }
    async fn drive(&mut self, cx: &Cx) {
        let (a, b) = Box::pin(network::both(
            self.host.drive(cx, Duration::from_millis(2), || true),
            self.client.drive(cx, Duration::from_millis(2), || true),
        ))
        .await;
        a.unwrap();
        b.unwrap();
    }
    fn receive_config(&mut self, cx: &Cx) -> Option<Vec<u8>> {
        let expected = Route::Stream(StreamRoute {
            outbound: false,
            ..self.config
        });
        let mut bytes = None;
        self.client
            .receive_ready(
                cx,
                || true,
                |r| r == expected,
                |_, b| {
                    assert!(bytes.is_none());
                    bytes = Some(b.to_vec());
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
        bytes
    }
    async fn configuration_bytes(&mut self, cx: &Cx, host: &mut Host) -> Vec<u8> {
        let until = Instant::now() + Duration::from_secs(2);
        let mut sent = false;
        loop {
            assert!(Instant::now() < until, "no configuration delivered");
            if !sent {
                sent = host.transmit(&mut self.host).unwrap();
            }
            assert!(
                host.take_recovery().unwrap().is_none(),
                "pixels escaped before configured ack"
            );
            self.drive(cx).await;
            if let Some(bytes) = self.receive_config(cx) {
                return bytes;
            }
        }
    }
    async fn send_reply(&mut self, cx: &Cx, bytes: &[u8]) {
        let route = Route::Stream(StreamRoute {
            outbound: true,
            ..self.replies
        });
        let until = network::clock(cx) + 1_000_000;
        loop {
            match self.client.send(cx, route, bytes, until, || true) {
                Ok(()) => break,
                Err(quic::Error::Backpressure) => self.drive(cx).await,
                Err(e) => panic!("{e:?}"),
            }
        }
        self.drive(cx).await;
    }
}
struct Source {
    capture: CaptureSource,
    control: ObservationControl,
    surface: X11Surface,
    _display: Display,
}
impl Source {
    async fn new(cx: &Cx) -> Self {
        let display = Display::start();
        let surface = X11Surface::presenter(
            Some(&display.name),
            320,
            240,
            configuration().limits().unwrap(),
        )
        .unwrap();
        let mut authority = SessionAuthority::new(
            binding().parent.remote_session,
            AuthorityPolicy::plan_defaults(),
        );
        authority.mark_capabilities_checked().unwrap();
        authority
            .authorize_observation(frd::media::host_now(cx).unwrap())
            .unwrap();
        let control = ObservationControl::new(cx.clone(), authority).unwrap();
        let capture = CaptureSource::start(
            &control,
            launch(&display, 11, Role::Capture),
            configuration(),
        )
        .await
        .unwrap();
        Self {
            capture,
            control,
            surface,
            _display: display,
        }
    }
    fn paint(&mut self, index: u8) -> [u8; 3] {
        let color = [
            index.wrapping_mul(17),
            index.wrapping_mul(23),
            index.wrapping_mul(31),
        ];
        let bytes = [color[0], color[1], color[2], 255].repeat(320 * 240);
        self.surface
            .present(&BgraFrame::new(320, 240, bytes, &configuration().limits().unwrap()).unwrap())
            .unwrap();
        color
    }
    async fn host(&mut self, link: &Link, timeout: Duration) -> Host {
        let update = self
            .capture
            .capture_if_changed(&self.control, true)
            .await
            .unwrap();
        Host::new(
            self.control.clone(),
            &link.host,
            link.setup(true, timeout),
            configuration(),
            update,
        )
        .unwrap()
    }
}
fn assert_pixel(readback: &mut X11Surface, color: [u8; 3]) {
    let frame = readback.snapshot().unwrap();
    for (v, e) in frame.pixels()[123 * 320 * 4 + 163 * 4..][..3]
        .iter()
        .zip(color)
    {
        assert!(v.abs_diff(e) <= 8, "pixel {v} != {e}");
    }
}
#[test]
fn actual_network_configuration_precedes_idr_and_survives_handoff_to_regular_media() {
    run(|cx| media_path(cx, false));
}
#[test]
fn all_negotiated_channels_repair_the_entire_final_picture_without_another_capture() {
    run(|cx| media_path(cx, true));
}
async fn media_path(cx: Cx, lose_final_picture: bool) {
    let mut link = Link::new(&cx).await;
    let mut source = Source::new(&cx).await;
    let color = source.paint(3);
    let target = Display::start();
    let mut readback =
        X11Surface::capture(Some(&target.name), configuration().limits().unwrap()).unwrap();
    let mut host = source.host(&link, Duration::from_secs(2)).await;
    let bytes = link.configuration_bytes(&cx, &mut host).await;
    let mut viewer = Viewer::start(
        cx.clone(),
        &link.client,
        link.setup(false, Duration::from_secs(2)),
        &bytes,
        launch(&target, 12, Role::Present),
        link.receive,
    )
    .await
    .unwrap();
    assert!(!viewer.is_complete());
    assert!(host.take_recovery().unwrap().is_none());
    assert!(viewer.transmit(&mut link.client).unwrap());
    let recovery = loop {
        link.drive(&cx).await;
        host.dispatch(&mut link.host).unwrap();
        if let Some(update) = host.take_recovery().unwrap() {
            break update;
        }
    };
    let first = recovery.frame();
    let mut sender = link
        .host_channels
        .sender(&link.host, source.control.clone(), SendPolicy::default())
        .unwrap();
    sender.enqueue_capture(recovery).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let receipt = loop {
        assert!(Instant::now() < deadline, "first IDR not decoded");
        host.check_transport(&link.host).unwrap();
        sender
            .transmit(&cx, &mut link.host, Lane::Original)
            .unwrap();
        link.drive(&cx).await;
        link.client_channels
            .receive_ready(
                &cx,
                &mut link.client,
                || true,
                |channel, b| {
                    viewer.receive_media(channel, b).unwrap();
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
        if let Some(receipt) = viewer.present_first().await.unwrap() {
            break receipt;
        }
    };
    assert_eq!(receipt.frame, first);
    assert!(!host.is_complete());
    assert_pixel(&mut readback, color);
    loop {
        if viewer.transmit(&mut link.client).unwrap() {
            break;
        }
        link.drive(&cx).await;
    }
    while !host.is_complete() {
        link.drive(&cx).await;
        host.dispatch(&mut link.host).unwrap();
    }
    assert!(viewer.is_complete());
    followup(
        &cx,
        &mut link,
        &mut source,
        &mut sender,
        viewer.finish().unwrap(),
        &mut readback,
        lose_final_picture,
    )
    .await;
    assert!(source.control.check().is_ok());
}
async fn followup(
    cx: &Cx,
    link: &mut Link,
    source: &mut Source,
    sender: &mut QuicEgress,
    presentation: (frd::media::Presenter, ReceivePipeline),
    readback: &mut X11Surface,
    lose_final_picture: bool,
) {
    let (mut presenter, mut receiver) = presentation;
    let deadline = Instant::now() + Duration::from_secs(2);
    let color = source.paint(5);
    let next = source
        .capture
        .capture_if_changed(&source.control, false)
        .await
        .unwrap();
    assert!(!next.encoded().unwrap().is_idr());
    let next_id = next.frame();
    sender.enqueue_capture(next).unwrap();
    let mut original_done = false;
    let mut repairs = 0;
    let mut dropped = 0;
    let mut pending_repair: Option<(Vec<u8>, u64)> = None;
    let mut reply = vec![0; link.wire.record_bytes()];
    let host_repair = Route::Stream(link.host_channels.repair_stream(&link.host).unwrap());
    let viewer_repair = Route::Stream(link.client_channels.repair_stream(&link.client).unwrap());
    loop {
        assert!(
            Instant::now() < deadline,
            "dependent frame not decoded after handoff"
        );
        if !original_done {
            original_done =
                sender.transmit(cx, &mut link.host, Lane::Original).unwrap() == Progress::Idle;
        }
        if original_done && repairs != 0 {
            sender.transmit(cx, &mut link.host, Lane::Repair).unwrap();
        }
        link.drive(cx).await;
        link.client_channels
            .receive_ready(
                cx,
                &mut link.client,
                || true,
                |channel, b| {
                    // Loss is applied AFTER actual QUIC reception, never faked at the
                    // packetizer. No new capture occurs after this dependent picture.
                    if lose_final_picture && channel == Channel::Video && repairs == 0 {
                        dropped += 1;
                        return Ok(Disposition::Consumed);
                    }
                    receiver.receive(channel, b, network::clock(cx)).unwrap();
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
        // One retained repair request, with its original deadline. Backpressure
        // cannot advance the receiver's repair cursor or produce another request.
        if original_done
            && pending_repair.is_none()
            && let Some(n) = receiver
                .repair_request(network::clock(cx), &mut reply)
                .unwrap()
        {
            pending_repair = Some((reply[..n].to_vec(), network::clock(cx) + 80_000));
        }
        if let Some((bytes, until)) = &pending_repair {
            match link.client.send(cx, viewer_repair, bytes, *until, || true) {
                Ok(()) => pending_repair = None,
                Err(quic::Error::Backpressure) => (),
                other => panic!("repair send refused: {other:?}"),
            }
        }
        // Leave all unrelated session records in the transport. A single bounded
        // slot moves this request out of the borrowed callback before owner use.
        let mut request = None;
        link.host
            .receive_ready(
                cx,
                || source.control.check().is_ok(),
                |r| r == host_repair,
                |r, b| {
                    if request.is_some() {
                        return Ok(Disposition::Blocked);
                    }
                    request = Some((r, b.to_vec()));
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
        if let Some((route, bytes)) = request {
            assert_eq!(
                sender.repair_on(&link.host, route, &bytes).unwrap(),
                RepairAdmission::Queued
            );
            repairs += 1;
        }
        if let Some(r) = presenter.present_next(cx, &mut receiver).await.unwrap() {
            assert_eq!(r.frame, next_id);
            break;
        }
    }
    if lose_final_picture {
        assert!(dropped > 0);
        assert!(repairs > 0);
    } else {
        assert_eq!(dropped, 0);
        assert_eq!(repairs, 0);
    }
    assert_pixel(readback, color);
}
#[test]
fn malformed_hvcc_codec_and_geometry_refuse_before_even_attempting_worker_launch() {
    run(|cx| async move {
        let mut link = Link::new(&cx).await;
        let mut source = Source::new(&cx).await;
        source.paint(1);
        let mut host = source.host(&link, Duration::from_secs(2)).await;
        let original = link.configuration_bytes(&cx, &mut host).await;
        let Message::Configuration(c) = decoder::decode(
            &original,
            binding(),
            &configuration().limits().unwrap(),
            D::HostToViewer,
            T::Reliable,
        )
        .unwrap() else {
            panic!()
        };
        for changed in [
            decoder::Configuration {
                hvcc: &[0; 23],
                ..c
            },
            decoder::Configuration {
                codec: "hev1.1.6.L1.90",
                ..c
            },
            decoder::Configuration {
                coded_width: c.coded_width + 16,
                ..c
            },
            decoder::Configuration { primaries: 9, ..c },
        ] {
            let mut bytes = vec![0; 4096];
            let n = decoder::encode(
                Message::Configuration(changed),
                binding(),
                &configuration().limits().unwrap(),
                &mut bytes,
                D::HostToViewer,
                T::Reliable,
            )
            .unwrap();
            let missing = Launch::new(
                Path::new("/nonexistent/fr-private-test-worker"),
                ":0",
                None,
                Role::Present,
                12,
            )
            .unwrap();
            let error = Viewer::start(
                cx.clone(),
                &link.client,
                link.setup(false, Duration::from_secs(1)),
                &bytes[..n],
                missing,
                link.receive,
            )
            .await
            .err()
            .expect("invalid config started worker");
            assert!(
                matches!(error, Error::Hevc(_) | Error::UnsupportedConfiguration),
                "wrong boundary: {error:?}"
            );
        }
        assert!(host.take_recovery().unwrap().is_none());
    });
}
#[test]
fn failed_native_startup_cannot_acknowledge_or_release_pixels() {
    run(|cx| async move {
        let mut link = Link::new(&cx).await;
        let mut source = Source::new(&cx).await;
        let mut host = source.host(&link, Duration::from_secs(2)).await;
        let bytes = link.configuration_bytes(&cx, &mut host).await;
        let missing = Launch::new(
            Path::new("/nonexistent/fr-private-test-worker"),
            ":0",
            None,
            Role::Present,
            12,
        )
        .unwrap();
        assert!(matches!(
            Viewer::start(
                cx.clone(),
                &link.client,
                link.setup(false, Duration::from_secs(1)),
                &bytes,
                missing,
                link.receive
            )
            .await,
            Err(Error::Media(_))
        ));
        link.drive(&cx).await;
        host.dispatch(&mut link.host).unwrap();
        assert!(host.take_recovery().unwrap().is_none());
    });
}
#[test]
fn early_decode_and_foreign_full_binding_reports_cannot_release_the_bootstrap() {
    run(|cx| async move {
        for foreign in [false, true] {
            let mut link = Link::new(&cx).await;
            let mut source = Source::new(&cx).await;
            let mut host = source.host(&link, Duration::from_secs(2)).await;
            let _ = link.configuration_bytes(&cx, &mut host).await;
            let mut b = binding();
            if foreign {
                b.geometry = b.geometry.next().unwrap();
            }
            let m = if foreign {
                Message::Configured
            } else {
                Message::FirstDecoded {
                    frame: 0,
                    decoder_micros: 0,
                }
            };
            let mut bytes = [0; 256];
            let n = decoder::encode(
                m,
                b,
                &configuration().limits().unwrap(),
                &mut bytes,
                D::ViewerToHost,
                T::Reliable,
            )
            .unwrap();
            link.send_reply(&cx, &bytes[..n]).await;
            // One drive slice may consume only ACK/timer traffic. Wait for this
            // reply to actually reach the dispatcher, without extending the
            // host's fixed deadline or allowing recovery while it is pending.
            let error = loop {
                if let Err(error) = host.dispatch(&mut link.host) {
                    break error;
                }
                assert!(
                    host.take_recovery().unwrap().is_none(),
                    "invalid reply released pixels before rejection"
                );
                link.drive(&cx).await;
            };
            assert_eq!(
                error,
                if foreign {
                    Error::Wire(fr_wire::WireError::InvalidBinding)
                } else {
                    Error::WrongState
                },
                "the received reply must be rejected, not merely time out"
            );
            assert!(host.take_recovery().is_err());
            assert!(
                source.control.check().is_ok(),
                "one startup ended the shared capture authority"
            );
        }
    });
}
#[test]
fn bootstrap_expiry_is_serviced_without_any_network_reply() {
    run(|cx| async move {
        let link = Link::new(&cx).await;
        let mut source = Source::new(&cx).await;
        let mut host = source.host(&link, Duration::from_millis(10)).await;
        let deadline = host.deadline_us();
        std::thread::sleep(Duration::from_millis(15));
        assert!(matches!(host.take_recovery(), Err(Error::Expired)));
        assert_eq!(host.deadline_us(), deadline);
        assert!(source.control.check().is_ok());
    });
}
#[test]
fn media_before_configured_ack_is_terminal() {
    run(|cx| async move {
        let mut link = Link::new(&cx).await;
        let mut source = Source::new(&cx).await;
        let target = Display::start();
        let mut host = source.host(&link, Duration::from_secs(2)).await;
        let bytes = link.configuration_bytes(&cx, &mut host).await;
        let mut viewer = Viewer::start(
            cx.clone(),
            &link.client,
            link.setup(false, Duration::from_secs(1)),
            &bytes,
            launch(&target, 12, Role::Present),
            link.receive,
        )
        .await
        .unwrap();
        assert!(matches!(
            viewer.receive_media(Channel::Recovery, &[]),
            Err(Error::WrongState)
        ));
        assert!(viewer.transmit(&mut link.client).is_err());
        assert!(host.take_recovery().unwrap().is_none());
    });
}

#[test]
fn genuine_transport_backpressure_preserves_configuration_and_original_deadline() {
    run(|cx| async move {
        let mut link = Link::new(&cx).await;
        let mut source = Source::new(&cx).await;
        let mut host = source.host(&link, Duration::from_secs(2)).await;
        let deadline = host.deadline_us();
        // One unrelated, explicitly synthetic progress record occupies the
        // actual critical queue. It is drained, never supplied to a decoder.
        let mut buffer = [0; 256];
        let now = network::clock(&cx);
        let n = fr_wire::encode_progress(
            fr_wire::Progress {
                descriptor: fr_wire::FrameDescriptor {
                    frame: 999,
                    reference: None,
                    total_bytes: 1,
                    stride: link.wire.fragment_stride(),
                    capture_micros: now,
                },
                observed_micros: now,
                observation: fr_wire::SourceObservation::Unknown,
                pipeline: fr_wire::PipelineState::Running,
            },
            link.progress.binding,
            &link.wire,
            &mut buffer,
        )
        .unwrap();
        // The real attachment ACK may still hold the one-record critical slot.
        // Admit the synthetic pressure record only after those bytes drain;
        // keep the same absolute startup deadline throughout this setup.
        loop {
            match link.host.send(
                &cx,
                Route::Stream(link.progress),
                &buffer[..n],
                deadline,
                || true,
            ) {
                Ok(()) => break,
                Err(quic::Error::Backpressure) => link.drive(&cx).await,
                Err(e) => panic!("pressure setup failed: {e:?}"),
            }
        }
        assert!(!host.transmit(&mut link.host).unwrap());
        assert!(!host.transmit(&mut link.host).unwrap());
        assert_eq!(host.deadline_us(), deadline);
        let expected = Route::Stream(StreamRoute {
            outbound: false,
            ..link.progress
        });
        while link.host.usage().critical_send_records != 0 {
            link.drive(&cx).await;
            link.client
                .receive_ready(
                    &cx,
                    || true,
                    |r| r == expected,
                    |_, bytes| {
                        assert_eq!(bytes, &buffer[..n]);
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
        }
        let bytes = link.configuration_bytes(&cx, &mut host).await;
        assert!(matches!(
            decoder::decode(
                &bytes,
                binding(),
                &configuration().limits().unwrap(),
                D::HostToViewer,
                T::Reliable
            )
            .unwrap(),
            Message::Configuration(_)
        ));
        assert_eq!(host.deadline_us(), deadline);
        assert!(host.take_recovery().unwrap().is_none());
    });
}
#[test]
fn equal_numeric_routes_on_another_connection_cannot_take_over_startup() {
    run(|cx| async move {
        let link = Link::new(&cx).await;
        let mut source = Source::new(&cx).await;
        let mut host = source.host(&link, Duration::from_secs(2)).await;
        let mut other = Link::new(&cx).await;
        // Attachment may leave its genuine ACK in native retransmission ownership.
        // Rejection must preserve EVERY queue charge, not assume an empty link.
        let before = other.host.usage();
        assert!(matches!(
            host.transmit(&mut other.host),
            Err(Error::ForeignConnection)
        ));
        assert!(host.take_recovery().is_err());
        assert!(source.control.check().is_ok());
        assert_eq!(other.host.usage(), before);
    });
}
#[test]
fn negotiated_media_owners_refuse_foreign_connections_without_touching_their_queues() {
    run(|cx| async move {
        let link = Link::new(&cx).await;
        let mut other = Link::new(&cx).await;
        let mut source = Source::new(&cx).await;
        source.paint(4);
        let capture = source
            .capture
            .capture_if_changed(&source.control, true)
            .await
            .unwrap();
        let mut sender = link
            .host_channels
            .sender(&link.host, source.control.clone(), SendPolicy::default())
            .unwrap();
        sender.enqueue_capture(capture).unwrap();
        let before = other.host.usage();
        assert_eq!(
            sender.transmit(&cx, &mut other.host, Lane::Original),
            Err(frd::media_quic::Error::ForeignConnection)
        );
        assert!(sender.is_closed());
        assert_eq!(sender.cache_usage().bytes, 0);
        assert_eq!(other.host.usage(), before);
        assert!(!other.host.is_closed());
        let before = other.client.usage();
        let mut dispatched = false;
        assert_eq!(
            link.client_channels.receive_ready(
                &cx,
                &mut other.client,
                || true,
                |_, _| {
                    dispatched = true;
                    Ok(Disposition::Consumed)
                }
            ),
            Err(frd::media_quic::Error::ForeignConnection)
        );
        assert!(!dispatched);
        assert_eq!(other.client.usage(), before);
        assert!(!other.client.is_closed());
        assert!(source.control.check().is_ok());
        // A second sender also rejects foreign repair dispatch before parsing.
        let mut sender = link
            .host_channels
            .sender(&link.host, source.control.clone(), SendPolicy::default())
            .unwrap();
        let before = other.host.usage();
        let route = Route::Stream(other.host_channels.repair_stream(&other.host).unwrap());
        assert_eq!(
            sender.repair_on(&other.host, route, &[]),
            Err(frd::media_quic::Error::ForeignConnection)
        );
        assert!(sender.is_closed());
        assert_eq!(other.host.usage(), before);
    });
}

#[test]
fn negotiated_sender_cannot_borrow_another_sessions_approved_observation() {
    run(|cx| async move {
        let link = Link::new(&cx).await;
        let mut authority = SessionAuthority::new(
            RemoteSessionId::from_raw(99),
            AuthorityPolicy::plan_defaults(),
        );
        authority.mark_capabilities_checked().unwrap();
        authority
            .authorize_observation(frd::media::host_now(&cx).unwrap())
            .unwrap();
        let foreign = ObservationControl::new(cx.clone(), authority).unwrap();
        let before = link.host.usage();
        assert!(matches!(
            link.host_channels
                .sender(&link.host, foreign.clone(), SendPolicy::default()),
            Err(frd::media_quic::Error::InvalidRoutes)
        ));
        assert_eq!(link.host.usage(), before);
        assert!(foreign.check().is_ok());
        assert!(!link.host.is_closed());
    });
}

#[test]
fn local_cancellation_before_ack_aborts_and_reaps_without_releasing_host_pixels() {
    let runtime = RuntimeBuilder::new()
        .worker_threads(2)
        .enable_platform_reactor(true)
        .blocking_threads(2, 4)
        .build()
        .unwrap();
    let cx = runtime.request_cx_with_budget(asupersync::types::Budget::INFINITE);
    let vcx = runtime.request_cx_with_budget(asupersync::types::Budget::INFINITE);
    runtime.block_on(async {
        let mut link = Link::new(&cx).await;
        let mut source = Source::new(&cx).await;
        let target = Display::start();
        let mut host = source.host(&link, Duration::from_secs(2)).await;
        let bytes = link.configuration_bytes(&cx, &mut host).await;
        let mut viewer = Viewer::start(
            vcx.clone(),
            &link.client,
            link.setup(false, Duration::from_secs(1)),
            &bytes,
            launch(&target, 12, Role::Present),
            link.receive,
        )
        .await
        .unwrap();
        assert!(viewer.worker_id().is_some());
        vcx.cancel_fast(asupersync::types::CancelKind::User);
        assert!(viewer.transmit(&mut link.client).is_err());
        assert!(host.take_recovery().unwrap().is_none());
        viewer
            .reap(
                &cx,
                frd::worker::Deadline::after(&cx, Duration::from_millis(500)).unwrap(),
            )
            .await
            .unwrap();
        assert!(viewer.worker_id().is_none());
        assert!(source.control.check().is_ok());
    });
}

async fn attach_role(
    cx: &Cx,
    host: &mut QuicRecords,
    client: &mut QuicRecords,
    control: (StreamRoute, StreamRoute),
    role: fr_wire::attachment::MediaRole,
    target: Binding,
) -> (
    fr_transport::quic::MediaChannel,
    fr_transport::quic::MediaChannel,
) {
    use fr_transport::quic::{ChannelRequest, ChannelScope, ControlRoutes};
    use fr_wire::attachment::Ticket;
    let (config, replies) = control;
    let selection = selected_attachment();
    let parent = ControlBinding {
        id: 7,
        ..binding().parent
    };
    let hc = ControlRoutes {
        outbound: config,
        inbound: replies,
    };
    let cc = ControlRoutes {
        outbound: StreamRoute {
            outbound: true,
            ..replies
        },
        inbound: StreamRoute {
            outbound: false,
            ..config
        },
    };
    let mut h = host
        .offer_media_role(
            cx,
            ChannelScope {
                control: hc,
                parent,
                selection: &selection,
            },
            ChannelRequest {
                binding: target,
                ticket: Ticket(0xa770 + u128::from(target.parent.id)),
                timeout: Duration::from_secs(2),
            },
            role,
            || true,
        )
        .unwrap();
    let mut c = None;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        assert!(
            Instant::now() < deadline,
            "configuration channel attachment stalled"
        );
        h.transmit(host, cx, || true).unwrap();
        if let Some(viewer) = &mut c {
            fr_transport::quic::MediaChannel::transmit(viewer, client, cx, || true).unwrap();
        }
        drive_attachment(cx, host, client).await;
        if c.is_none() {
            let mut offer = None;
            let ready = std::cell::Cell::new(true);
            client
                .receive_ready(
                    cx,
                    || true,
                    |r| ready.get() && r == Route::Stream(cc.inbound),
                    |_, bytes| {
                        ready.set(false);
                        offer = Some(bytes.to_vec());
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
            if let Some(bytes) = offer {
                c = Some(
                    client
                        .accept_media_channel(
                            cx,
                            ChannelScope {
                                control: cc,
                                parent,
                                selection: &selection,
                            },
                            &bytes,
                            Duration::from_secs(2),
                            || true,
                        )
                        .unwrap(),
                );
            }
        }
        h.dispatch(host, cx, || true).unwrap();
        if let Some(viewer) = &mut c {
            viewer.dispatch(client, cx, || true).unwrap();
            if let (Some(hp), Some(vp)) = (
                h.finish(host, cx, || true).unwrap(),
                viewer.finish(client, cx, || true).unwrap(),
            ) {
                assert_eq!(hp.descriptor, vp.descriptor);
                return (h, c.take().unwrap());
            }
        }
    }
}

fn selected_attachment() -> fr_wire::negotiation::Selection {
    Offer {
        versions: vec![0],
        profile: 1,
        profile_version: 0,
        role: fr_wire::negotiation::Role::Observe,
        limits: configuration().limits().unwrap(),
        capabilities: vec![
            Capability {
                name: decoder::CAPABILITY.into(),
                version: decoder::VERSION,
                required: true,
            },
            Capability {
                name: fr_wire::attachment::CAPABILITY.into(),
                version: 1,
                required: true,
            },
            Capability {
                name: fr_wire::attachment::DELIVERY_CAPABILITY.into(),
                version: 1,
                required: true,
            },
        ],
    }
    .select()
    .unwrap()
}

async fn drive_attachment(cx: &Cx, host: &mut QuicRecords, client: &mut QuicRecords) {
    let (a, b) = Box::pin(network::both(
        host.drive(cx, Duration::from_millis(1), || true),
        client.drive(cx, Duration::from_millis(1), || true),
    ))
    .await;
    a.unwrap();
    b.unwrap();
}

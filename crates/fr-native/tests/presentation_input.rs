#![cfg(all(
    target_os = "linux",
    feature = "linux-media",
    feature = "linux-input-agent"
))]
//! Real capture/HEVC/IPC/X11 and input composition on two PRIVATE X servers.
//! A controlled X11 readback qualifies test visibility, not physical scanout.
//! Clock samples and authority grants are local fixtures, not Tailscale proof.
use asupersync::{
    cx::Cx,
    runtime::{Runtime, RuntimeBuilder},
    types::Budget,
};
use fr_client::input::presentation::PresentedInput;
use fr_client::input::{Action, ClientInstant, InputClient, Policy, ResultEvent, StopReason};
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::*,
    input::*,
    input_sequence::InputOutcome,
    input_submission::InputSession,
};
use fr_media::{
    delivery::*,
    freshness::{ClockCorrelation, ClockPolicy, ClockSample, Error as ViewError},
    worker::{Backend, Configuration, Kind, Role},
};
use fr_native::{BgraFrame, X11Surface, input::X11Pointer, input_agent::start_x11};
use fr_wire::{Channel, MediaLimits, input::InputDelivery};
use frd::{
    input_agent::{Agent, Route, Seat, Shutdown},
    input_watchdog::{StopReason as HostStop, host_now as input_now},
    media::{
        CaptureSource, ObservationControl, PresentationStage, Presenter, Subscription, host_now,
    },
    worker::{Deadline, Launch},
};
use std::{
    io::{BufRead, BufReader, Read},
    path::Path,
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};
struct Server {
    child: Child,
    name: String,
}
impl Server {
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
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn config() -> Configuration {
    Configuration {
        width: 320,
        height: 240,
        fps: 30,
        backend: Backend::SoftwareExplicit,
        bitrate: 2_000_000,
        max_access_unit_bytes: 1_048_576,
        generation: CodecConfigurationGeneration::INITIAL,
    }
}
fn credentials() -> InputCredentials {
    InputCredentials {
        session: RemoteSessionId::from_raw(1),
        lease: InputLeaseId::from_raw(2),
        ticket: InputTicketId::from_raw(3),
        view: InputView {
            geometry: DisplayGeometryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
    }
}
fn now(cx: &Cx) -> ClientInstant {
    ClientInstant(host_now(cx).unwrap().as_micros())
}
struct Running {
    done: mpsc::Receiver<Shutdown>,
    join: thread::JoinHandle<()>,
}
impl Running {
    fn finish(self) -> Shutdown {
        let value = self.done.recv_timeout(Duration::from_secs(5)).unwrap();
        self.join.join().unwrap();
        value
    }
}
fn native_input(server: &Server, observer: &X11Pointer) -> (Agent, Running, Seat) {
    let rt = RuntimeBuilder::new().worker_threads(1).build().unwrap();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let at = input_now(&cx).unwrap();
    let c = credentials();
    let mut auth = SessionAuthority::new(c.session, AuthorityPolicy::plan_defaults());
    auth.mark_capabilities_checked().unwrap();
    auth.authorize_observation(at).unwrap();
    auth.mark_view_ready(at).unwrap();
    auth.grant_lease(c.lease, at).unwrap();
    auth.issue_input_ticket(c.lease, c.ticket, at).unwrap();
    let session =
        InputSession::new(auth, c, observer.bounds(), observer.capabilities(), at).unwrap();
    let seat = Seat::default();
    let (agent, driver) = start_x11(
        &seat,
        cx,
        session,
        Route::new(7, config().limits().unwrap()),
        &server.name,
    )
    .unwrap();
    let (tx, done) = mpsc::sync_channel(1);
    let join = thread::spawn(move || {
        let _ = tx.send(rt.block_on(driver));
    });
    (agent, Running { done, join }, seat)
}
fn runtime() -> Runtime {
    RuntimeBuilder::new()
        .worker_threads(1)
        .blocking_threads(1, 2)
        .enable_platform_reactor(true)
        .build()
        .unwrap()
}
async fn close_media(cx: &Cx, capture: &mut CaptureSource, presenter: &mut Presenter) {
    for worker in [capture.worker_mut(), presenter.worker_mut()] {
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
struct Video {
    _source: X11Surface,
    control: ObservationControl,
    capture: CaptureSource,
    presenter: Presenter,
    receiver: ReceivePipeline,
    subscription: Subscription,
    client: PresentedInput,
    observer: X11Pointer,
    wire: MediaLimits,
}
async fn prepare(source_server: &Server, viewer_server: &Server, cx: &Cx) -> Video {
    let cfg = config();
    let limits = cfg.limits().unwrap();
    let mut source = X11Surface::presenter(Some(&source_server.name), 320, 240, limits).unwrap();
    let pixels = [40, 80, 180, 255].repeat(320 * 240);
    source
        .present(&BgraFrame::new(320, 240, pixels, &limits).unwrap())
        .unwrap();
    let mut auth = SessionAuthority::new(
        RemoteSessionId::from_raw(50),
        AuthorityPolicy::plan_defaults(),
    );
    auth.mark_capabilities_checked().unwrap();
    auth.authorize_observation(host_now(cx).unwrap()).unwrap();
    let control = ObservationControl::new(cx.clone(), auth).unwrap();
    let binary = Path::new(env!("CARGO_BIN_EXE_fr-media-worker"));
    let capture = CaptureSource::start(
        &control,
        Launch::new(binary, &source_server.name, None, Role::Capture, 11).unwrap(),
        cfg,
    )
    .await
    .unwrap();
    let presenter = Presenter::start(
        cx,
        Launch::new(binary, &viewer_server.name, None, Role::Present, 12).unwrap(),
        cfg,
    )
    .await
    .unwrap();
    let wire = MediaLimits::new(limits, 1150, 16384, 64).unwrap();
    let bindings = MediaBindings::new(1, 2, 3, 4).unwrap();
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
    let mut receiver = ReceivePipeline::new(
        ReceiveConfig {
            limits: wire,
            bindings,
            epoch,
            policy: ReceivePolicy::default(),
        },
        MediaBudget::new(&limits).unwrap(),
    )
    .unwrap();
    receiver.decoder_configured(now(cx).0).unwrap();
    let observer = X11Pointer::open(&source_server.name).unwrap();
    let input = InputClient::new(
        credentials(),
        7,
        observer.bounds(),
        observer.capabilities(),
        limits,
        Policy::default(),
        now(cx),
    )
    .unwrap();
    // The real timers share a local domain here. The three samples are ordered;
    // the uncertainty code still consumes the full measured exchange bracket.
    let sent = now(cx).0;
    let sampled = host_now(cx).unwrap().as_micros();
    let reply_at = now(cx).0;
    let clock = ClockCorrelation::new(
        ClockSample {
            host_boot: HostBootId::from_raw(1),
            client_sent_us: sent,
            host_sample_us: sampled,
            client_received_us: reply_at,
        },
        ClockPolicy::default(),
    )
    .unwrap();
    let mut client = PresentedInput::new(input, &receiver, clock, now(cx)).unwrap();
    client
        .confirm_mapping(credentials().session, credentials().view, now(cx))
        .unwrap();
    Video {
        _source: source,
        control,
        capture,
        presenter,
        receiver,
        subscription,
        client,
        observer,
        wire,
    }
}
async fn scenario(delay_receipt: bool) {
    let source_server = Server::start();
    let viewer_server = Server::start();
    let cx = Cx::current().unwrap();
    let Video {
        _source,
        control,
        mut capture,
        mut presenter,
        mut receiver,
        mut subscription,
        mut client,
        mut observer,
        wire,
    } = prepare(&source_server, &viewer_server, &cx).await;
    let limits = config().limits().unwrap();
    let unit = capture.capture(&control, false).await.unwrap();
    subscription.enqueue(unit).unwrap();
    let mut packet = [0; 1150];
    while let Some(offer) = subscription.next_packet(&mut packet).unwrap() {
        subscription.authorize_write(&offer).unwrap();
        let at = now(&cx);
        receiver
            .receive(offer.channel, &packet[..offer.byte_len], at.0)
            .unwrap();
        if offer.channel == Channel::MediaConfig {
            client
                .progress(&packet[..offer.byte_len], &wire, at)
                .unwrap();
        }
    }
    let receipt = presenter
        .present_next(&cx, &mut receiver)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt.stage, PresentationStage::SubmittedToCompositor);
    let frame = receipt.frame.as_raw();
    let display_until = receipt.decoded.display_deadline_us();
    if delay_receipt {
        asupersync::time::sleep(cx.timer_driver().unwrap().now(), Duration::from_millis(65)).await;
    }
    client.decoded(receipt.decoded, true, now(&cx)).unwrap();
    assert!(
        !client.tick(now(&cx)).unwrap(),
        "compositor submission alone enabled input"
    );
    let mut readback = X11Surface::capture(Some(&viewer_server.name), limits).unwrap();
    let image = readback.snapshot().unwrap();
    // No other writer exists on this isolated display. Observe the expected
    // solid-color picture with room for HEVC/color-conversion rounding.
    let p = &image.pixels()[4 * (120 * 320 + 160)..][..4];
    assert!(
        p[0].abs_diff(40) < 10 && p[1].abs_diff(80) < 10 && p[2].abs_diff(180) < 10,
        "presenter readback did not contain the decoded picture"
    );
    if delay_receipt {
        assert!(now(&cx).0 >= display_until);
        assert_eq!(
            client.visible(frame, now(&cx)),
            Err(fr_client::input::presentation::Error::Media(
                ViewError::QueueExpired
            ))
        );
        assert!(
            client
                .pointer(DesktopPoint { x: 30, y: 40 }, &mut packet, now(&cx))
                .is_err()
        );
        close_media(&cx, &mut capture, &mut presenter).await;
        return;
    }
    let evidence = client.visible(frame, now(&cx)).unwrap();
    assert!(evidence.source_age_upper_us < 250_000);
    drag_then_expire(&mut client, &mut observer, &source_server, &cx).await;
    close_media(&cx, &mut capture, &mut presenter).await;
}
async fn drag_then_expire(
    client: &mut PresentedInput,
    observer: &mut X11Pointer,
    server: &Server,
    cx: &Cx,
) {
    let limits = config().limits().unwrap();
    let mut packet = [0; 1150];
    let (mut agent, running, seat) = native_input(server, observer);
    let encoded = client
        .action(
            Action::Button {
                button: PointerButton::Primary,
                pressed: true,
                position: DesktopPoint { x: 30, y: 40 },
            },
            &mut packet,
            now(cx),
        )
        .unwrap();
    agent
        .submit(&packet[..encoded.bytes], InputDelivery::Reliable)
        .unwrap();
    let response = agent.input_response().unwrap().await.unwrap();
    let frd::input_agent::InputReply::Record(result) = response else {
        panic!("real native receipt required")
    };
    assert_eq!(result.outcome, InputOutcome::SubmittedToOs);
    assert_eq!(observer.query_pointer().unwrap().1 & 256, 256);
    let count = fr_wire::input_result::encode_input_result(
        result,
        &mut packet,
        &limits,
        fr_wire::input::InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
    .unwrap();
    // No further capture or codec frame. Idle service observes the old source
    // deadline, and the lifecycle signal ends the independent native lease.
    asupersync::time::sleep(cx.timer_driver().unwrap().now(), Duration::from_millis(260)).await;
    assert!(client.tick(now(cx)).is_err());
    assert_eq!(client.stopped(), Some(StopReason::ViewStale));
    agent.control().stop(HostStop::ViewInvalidated);
    let shutdown = running.finish();
    assert!(shutdown.handoff_safe());
    assert!(!seat.is_occupied());
    assert_eq!(observer.query_pointer().unwrap().1 & 256, 0);
    assert_eq!(
        client.result(&packet[..count], now(cx)).unwrap(),
        ResultEvent::Completed(result)
    );
    assert!(
        client
            .pointer(DesktopPoint { x: 80, y: 90 }, &mut packet, now(cx))
            .is_err()
    );
}

#[test]
fn captured_video_visible_readback_input_receipt_and_idle_stale_cleanup() {
    runtime().block_on(scenario(false));
}
#[test]
fn delayed_native_decode_receipt_cannot_enable_input_on_an_old_display_deadline() {
    runtime().block_on(scenario(true));
}

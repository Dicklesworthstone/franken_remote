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
fn run(scenario: impl std::future::Future<Output = ()>) {
    // Each experiment measures real 50 ms display deadlines. Obtain the fixture
    // slot before creating runtimes, native workers, or time-limited grants.
    static SCENARIOS: std::sync::Mutex<()> = std::sync::Mutex::new(());
    // No shared experiment state is protected, so one failing private fixture
    // must not turn the remaining tests into lock-poison failures.
    let _guard = SCENARIOS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    runtime().block_on(scenario);
}
async fn close_media(cx: &Cx, capture: &mut CaptureSource, presenter: &mut Presenter) {
    let worker = capture.worker_mut();
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

    presenter
        .stop(cx, Deadline::after(cx, Duration::from_millis(500)).unwrap())
        .await
        .unwrap();
    assert!(
        presenter
            .reap(cx, Deadline::after(cx, Duration::from_millis(500)).unwrap())
            .await
            .unwrap()
            .success()
    );
}
struct Video {
    source: X11Surface,
    control: ObservationControl,
    capture: CaptureSource,
    presenter: Presenter,
    receiver: ReceivePipeline,
    subscription: Subscription,
    client: PresentedInput,
    observer: X11Pointer,
    wire: MediaLimits,
}
async fn bootstrap_record(
    capture: &mut CaptureSource,
    control: &ObservationControl,
    cfg: Configuration,
) -> fr_media::hevc::DecoderRecord {
    // A real bounded bootstrap obtains parameter sets; it is never relabelled
    // as a fresh observation after the decoder startup delay.
    let bootstrap = capture.capture(control, true).await.unwrap();
    let mut admission =
        fr_media::hevc::HevcGuard::new(cfg.codec().unwrap(), cfg.limits().unwrap(), 4).unwrap();
    admission
        .validate_length_prefixed(bootstrap.bytes(), true)
        .unwrap();
    let record = admission.decoder_record().unwrap();
    eprintln!(
        "decoder_bootstrap frame={} encoded_bytes={} hvcc_bytes={} published=false",
        bootstrap.frame().as_raw(),
        bootstrap.bytes().len(),
        record.bytes().len()
    );
    record
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
    let mut capture = CaptureSource::start(
        &control,
        Launch::new(binary, &source_server.name, None, Role::Capture, 11).unwrap(),
        cfg,
    )
    .await
    .unwrap();
    let record = bootstrap_record(&mut capture, &control, cfg).await;
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
    let presenter = Presenter::start(
        cx,
        Launch::new(binary, &viewer_server.name, None, Role::Present, 12).unwrap(),
        cfg,
        &record,
        &mut receiver,
    )
    .await
    .unwrap();
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
        source,
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
        source: _source,
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
    let unit = capture.capture(&control, true).await.unwrap();
    subscription.enqueue(unit).unwrap();
    let mut packet = [0; 1150];
    while let Some(offer) = subscription.next_packet(&mut packet).unwrap() {
        subscription.authorize_write(&offer).unwrap();
        let at = now(&cx);
        receiver
            .receive(offer.channel(), &packet[..offer.byte_len()], at.0)
            .unwrap();
        if offer.channel() == Channel::MediaConfig {
            client
                .progress(&packet[..offer.byte_len()], &wire, at)
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
    observe_pressed(observer, &agent, cx).await;
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
    run(scenario(false));
}
#[test]
fn delayed_native_decode_receipt_cannot_enable_input_on_an_old_display_deadline() {
    run(scenario(true));
}

#[derive(Clone, Copy, Debug)]
enum Loss {
    FinalFragment,
    EntirePicture,
    EveryFifth,
}
#[derive(Debug, Default)]
struct RecoveryCounts {
    original_fragments: usize,
    dropped_fragments: usize,
    duplicate_fragments: usize,
    repair_fragments: usize,
    repair_wire_bytes: usize,
    encoded_bytes: usize,
    peak_sender_bytes: usize,
    peak_receiver_bytes: usize,
}
fn check_bounds(video: &Video, counts: &mut RecoveryCounts) {
    let send = video.subscription.cache_usage();
    let receive = video.receiver.budget_usage();
    assert!(send.bytes <= SendPolicy::default().max_cached_bytes);
    assert!(send.pictures <= SendPolicy::default().max_cached_pictures);
    assert!(
        u64::try_from(receive.bytes).unwrap()
            <= video.wire.protocol().per_viewer_compressed_bytes()
    );
    assert!(receive.pictures <= usize::from(video.wire.protocol().reassembly_window_pictures()));
    counts.peak_sender_bytes = counts.peak_sender_bytes.max(send.bytes);
    counts.peak_receiver_bytes = counts.peak_receiver_bytes.max(receive.bytes);
}
fn receive_packet(video: &mut Video, offer: &PacketOffer, packet: &[u8], cx: &Cx) {
    let at = now(cx);
    video
        .receiver
        .receive(offer.channel(), packet, at.0)
        .unwrap();
    if offer.channel() == Channel::MediaConfig {
        video.client.progress(packet, &video.wire, at).unwrap();
    }
}
fn flush_fragments(
    video: &mut Video,
    held: &mut [Option<(PacketOffer, [u8; 1150])>; 4],
    counts: &mut RecoveryCounts,
    cx: &Cx,
) {
    for entry in held.iter_mut().rev() {
        if let Some((offer, packet)) = entry.take() {
            receive_packet(video, &offer, &packet[..offer.byte_len()], cx);
            let before = video.receiver.budget_usage();
            receive_packet(video, &offer, &packet[..offer.byte_len()], cx);
            assert_eq!(
                video.receiver.budget_usage(),
                before,
                "duplicate allocated again"
            );
            counts.duplicate_fragments += 1;
            check_bounds(video, counts);
        }
    }
}
fn impaired_transfer(video: &mut Video, loss: Option<Loss>, counts: &mut RecoveryCounts, cx: &Cx) {
    let mut packet = [0; 1150];
    // The fault injector itself has exactly four inline records, including
    // their metadata. It never accumulates a whole encoded picture's packets.
    let mut held = core::array::from_fn(|_| None);
    let mut held_count = 0;
    while let Some(offer) = video.subscription.next_packet(&mut packet).unwrap() {
        video.subscription.authorize_write(&offer).unwrap();
        if offer.channel() == Channel::Video {
            let record = fr_wire::Record::decode(
                &packet[..offer.byte_len()],
                &video.wire,
                1,
                Channel::Video,
            )
            .unwrap();
            let fragment = fr_wire::decode_fragment(record, &video.wire).unwrap();
            let total = fragment.descriptor.fragment_count().unwrap();
            assert!(
                total > 1,
                "real HEVC picture did not exercise fragmentation"
            );
            counts.original_fragments += 1;
            let drop = match loss {
                Some(Loss::FinalFragment) => fragment.index + 1 == total,
                Some(Loss::EntirePicture) => true,
                Some(Loss::EveryFifth) => fragment.index.is_multiple_of(5),
                None => false,
            };
            if drop {
                counts.dropped_fragments += 1;
                continue;
            }
            held[held_count] = Some((offer, packet));
            held_count += 1;
            if held_count == held.len() {
                flush_fragments(video, &mut held, counts, cx);
                held_count = 0;
            }
        } else {
            assert!(matches!(
                offer.channel(),
                Channel::MediaConfig | Channel::Recovery
            ));
            receive_packet(video, &offer, &packet[..offer.byte_len()], cx);
            check_bounds(video, counts);
        }
    }
    flush_fragments(video, &mut held, counts, cx);
}
fn marker(phase: u8) -> [u8; 4] {
    if phase == 0 {
        [190, 160, 60, 255]
    } else {
        [60, 180, 220, 255]
    }
}
async fn capture_pattern(
    video: &mut Video,
    phase: u8,
    counts: &mut RecoveryCounts,
) -> (usize, u64) {
    paint_pattern(video, phase);
    let unit = video.capture.capture(&video.control, false).await.unwrap();
    assert!(
        !unit.is_idr(),
        "repair must exercise a predictive HEVC picture"
    );
    let bytes = unit.bytes().len();
    let frame = unit.frame().as_raw();
    assert!(bytes > video.wire.fragment_stride() as usize);
    counts.encoded_bytes += bytes;
    let before = video.subscription.cache_usage().bytes;
    video.subscription.enqueue(unit).unwrap();
    assert!(
        video.subscription.cache_usage().bytes > before + bytes,
        "cache omitted picture metadata"
    );
    check_bounds(video, counts);
    (bytes, frame)
}
fn paint_pattern(video: &mut Video, phase: u8) {
    let limits = config().limits().unwrap();
    let mut pixels = vec![0; 320 * 240 * 4];
    for (index, pixel) in pixels.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let (width, height) = if phase == 0 { (4, 4) } else { (3, 5) };
        let tile = ((index % 320) / width + (index / 320) / height) % 2;
        pixel.copy_from_slice(if tile == 0 {
            &[40, 80, 180, 255]
        } else {
            &[190, 160, 60, 255]
        });
        // A large interior marker identifies the source frame despite HEVC
        // color rounding and subsampling of the fine checkerboard.
        if (128..192).contains(&(index % 320)) && (96..144).contains(&(index / 320)) {
            pixel.copy_from_slice(&marker(phase));
        }
    }
    video
        .source
        .present(&BgraFrame::new(320, 240, pixels, &limits).unwrap())
        .unwrap();
}
async fn sleep_until(cx: &Cx, until: u64) {
    let remaining = until.saturating_sub(now(cx).0);
    asupersync::time::sleep(
        cx.timer_driver().unwrap().now(),
        Duration::from_micros(remaining),
    )
    .await;
}
async fn present_visible(
    video: &mut Video,
    output: &mut X11Surface,
    cx: &Cx,
    expected_frame: u64,
    expected_marker: [u8; 4],
) -> Vec<u8> {
    let receipt = video
        .presenter
        .present_next(cx, &mut video.receiver)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        receipt.stage,
        PresentationStage::SubmittedToCompositor,
        "expected visible frame {expected_frame}; current_us={} display_deadline_us={}",
        now(cx).0,
        receipt.decoded.display_deadline_us()
    );
    let display_deadline = receipt.decoded.display_deadline_us();
    let receipt_observed_at = now(cx).0;
    let frame = receipt.frame.as_raw();
    assert_eq!(frame, expected_frame);
    video
        .client
        .decoded(receipt.decoded, true, now(cx))
        .unwrap();
    let readback_started = now(cx).0;
    let pixels = output.snapshot().unwrap().pixels().to_vec();
    for (x, y) in [(144, 108), (160, 120), (176, 132)] {
        let p = &pixels[4 * (y * 320 + x)..][..3];
        assert!(
            p.iter()
                .zip(expected_marker)
                .all(|(&actual, expected)| actual.abs_diff(expected) < 10),
            "wrong source frame is visible"
        );
    }
    let visible_at = now(cx);
    video.client.visible(frame, visible_at).unwrap_or_else(|error| {
        panic!("visible frame {frame} refused: {error:?}; receipt_observed_us={receipt_observed_at} readback_start_us={readback_started} visible_us={} display_deadline_us={display_deadline}", visible_at.0)
    });
    assert_eq!(video.receiver.budget_usage(), BudgetUsage::default());
    pixels
}
async fn repair_missing(video: &mut Video, counts: &mut RecoveryCounts, cx: &Cx) {
    let deadline = video.receiver.next_deadline().unwrap();
    sleep_until(cx, deadline).await;
    let mut packet = [0; 1150];
    let size = video
        .receiver
        .repair_request(now(cx).0, &mut packet)
        .unwrap()
        .unwrap();
    assert!(size <= video.wire.record_bytes());
    video.subscription.queue_repair(&packet[..size]).unwrap();
    assert!(
        video
            .receiver
            .repair_request(now(cx).0, &mut packet)
            .unwrap()
            .is_none(),
        "repair rate limit not enforced"
    );
    while let Some(offer) = video.subscription.next_repair(&mut packet).unwrap() {
        assert_eq!(offer.channel(), Channel::Video);
        video.subscription.authorize_write(&offer).unwrap();
        receive_packet(video, &offer, &packet[..offer.byte_len()], cx);
        counts.repair_fragments += 1;
        counts.repair_wire_bytes += offer.byte_len();
        check_bounds(video, counts);
    }
    assert!(counts.repair_wire_bytes <= SendPolicy::default().repair_bytes_per_window);
}
fn collect_input_result(
    client: &mut PresentedInput,
    result: fr_wire::input_result::InputResult,
    cx: &Cx,
) {
    let mut packet = [0; fr_wire::input_result::INPUT_RESULT_BYTES];
    let bytes = fr_wire::input_result::encode_input_result(
        result,
        &mut packet,
        &config().limits().unwrap(),
        fr_wire::input::InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
    .unwrap();
    assert_eq!(
        client.result(&packet[..bytes], now(cx)).unwrap(),
        ResultEvent::Completed(result)
    );
}
async fn press_drag(video: &mut Video, agent: &mut Agent, cx: &Cx) {
    let mut packet = [0; 1150];
    let press = video
        .client
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
        .submit(&packet[..press.bytes], InputDelivery::Reliable)
        .unwrap();
    let frd::input_agent::InputReply::Record(result) =
        agent.input_response().unwrap().await.unwrap()
    else {
        panic!("real native press receipt required")
    };
    assert_eq!(result.outcome, InputOutcome::SubmittedToOs);
    assert_eq!(result.submitted_operations, 2);
    observe_pressed(&mut video.observer, agent, cx).await;
    collect_input_result(&mut video.client, result, cx);
}

async fn observe_pressed(observer: &mut X11Pointer, agent: &Agent, cx: &Cx) {
    // SubmittedToOs certifies XFlush on the native connection. This independent
    // observer connection may be serviced first. Wait only for observation;
    // never resubmit an action, renew authority, or extend a source deadline.
    let started = now(cx).0;
    loop {
        let state = observer.query_pointer().unwrap();
        let elapsed = now(cx).0 - started;
        assert!(
            !agent.control().is_stopped(),
            "press observation after {elapsed} us: control stopped {:?}, state {state:?}",
            agent.control().reason()
        );
        assert!(
            elapsed < 50_000,
            "press not observed after {elapsed} us: {state:?}"
        );
        if state == (DesktopPoint { x: 30, y: 40 }, 256) {
            return;
        }
        asupersync::time::sleep(cx.timer_driver().unwrap().now(), Duration::from_millis(1)).await;
    }
}
async fn queued_input_fence(video: &mut Video, server: &Server, cx: &Cx, expire_ticket: bool) {
    let (mut agent, running, seat) = native_input(server, &video.observer);
    press_drag(video, &mut agent, cx).await;
    let mut packet = [0; 1150];
    let before = video.observer.query_pointer().unwrap();
    assert_eq!(before.1 & 256, 256);
    let queued = video
        .client
        .action(
            Action::Button {
                button: PointerButton::Primary,
                pressed: false,
                position: DesktopPoint { x: 80, y: 90 },
            },
            &mut packet,
            now(cx),
        )
        .unwrap();
    // One immutable reliable action is retained by this transport-fault fixture.
    // No ticket refresh or implicit retry may change what reaches the host.
    let queued_at = now(cx).0;
    let fence_trigger_at;
    if expire_ticket {
        sleep_until(cx, queued_at + 260_000).await;
        assert!(video.client.tick(now(cx)).is_err());
        assert_eq!(video.client.stopped(), Some(StopReason::ViewStale));
        // Deliberately withhold the client's stop signal too. This isolates the
        // host ticket check when stalled transport delays both action and stop.
        sleep_until(cx, queued_at + 1_100_000).await;
        assert!(
            !agent.control().is_stopped(),
            "lease ended before the ticket oracle"
        );
        assert_eq!(video.observer.query_pointer().unwrap(), before);
        fence_trigger_at = now(cx).0;
        agent
            .submit(&packet[..queued.bytes], InputDelivery::Reliable)
            .unwrap();
        let frd::input_agent::InputReply::Record(expired) =
            agent.input_response().unwrap().await.unwrap()
        else {
            panic!("real native ticket-expiry receipt required")
        };
        assert_eq!(expired.outcome, InputOutcome::ExpiredBeforeSubmission);
        assert_eq!(expired.stage, fr_wire::input_result::Stage::Admitted);
        assert_eq!(
            expired.reason,
            Some(fr_wire::input_result::Reason::TicketExpired)
        );
        assert_eq!(expired.submitted_operations, 0);
        // A terminal refused action revokes the sequence and triggers separate
        // release-only cleanup. It must never perform its requested movement.
        assert!(agent.control().is_stopped());
        assert_eq!(video.observer.query_pointer().unwrap().0, before.0);
        collect_input_result(&mut video.client, expired, cx);
        assert_eq!(video.client.pending_actions(), 0);
    } else {
        fence_trigger_at = now(cx).0;
        agent.control().stop(HostStop::LocalRevoke);
        // The authority fence is synchronous; native cleanup is a separate result.
        assert!(agent.control().is_stopped());
        assert_eq!(
            agent.submit(&packet[..queued.bytes], InputDelivery::Reliable),
            Err(frd::input_agent::Error::Stopped)
        );
        video.client.hidden();
    }
    let shutdown = running.finish();
    assert_eq!(
        shutdown.reason,
        if expire_ticket {
            HostStop::AuthorityEnded
        } else {
            HostStop::LocalRevoke
        }
    );
    assert!(shutdown.handoff_safe());
    assert!(!seat.is_occupied());
    let after = video.observer.query_pointer().unwrap();
    assert_eq!(
        after.0, before.0,
        "queued expired/revoked action moved the pointer"
    );
    assert_eq!(after.1 & 256, 0);
    println!(
        "input_fence ticket_expiry={expire_ticket} queued_age_us={} fence_trigger_to_cleanup_readback_us={} queued_record_bytes={}",
        now(cx).0 - queued_at,
        now(cx).0 - fence_trigger_at,
        queued.bytes
    );
}
async fn decode_late_reference(
    video: &mut Video,
    cx: &Cx,
    reference_frame: u64,
    output: &mut X11Surface,
    old_pixels: &[u8],
) {
    let receipt = video
        .presenter
        .present_next(cx, &mut video.receiver)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt.stage, PresentationStage::DecodedOnly);
    assert_eq!(receipt.frame.as_raw(), reference_frame);
    assert!(now(cx).0 >= receipt.decoded.display_deadline_us());
    video
        .client
        .decoded(receipt.decoded, false, now(cx))
        .unwrap();
    assert_eq!(
        output.snapshot().unwrap().pixels(),
        old_pixels,
        "late reference was displayed"
    );
    assert_eq!(video.receiver.budget_usage().pictures, 1);
}
async fn recovery_case(loss: Loss, late_reference: bool) {
    let source = Server::start();
    let viewer = Server::start();
    let cx = Cx::current().unwrap();
    let mut video = prepare(&source, &viewer, &cx).await;
    let mut output = X11Surface::capture(Some(&viewer.name), config().limits().unwrap()).unwrap();
    let mut counts = RecoveryCounts::default();
    let bootstrap = video.capture.capture(&video.control, true).await.unwrap();
    assert!(bootstrap.is_idr());
    let bootstrap_frame = bootstrap.frame().as_raw();
    video.subscription.enqueue(bootstrap).unwrap();
    impaired_transfer(&mut video, None, &mut counts, &cx);
    let old_pixels = present_visible(
        &mut video,
        &mut output,
        &cx,
        bootstrap_frame,
        [40, 80, 180, 255],
    )
    .await;
    let (bytes, reference_frame) = capture_pattern(&mut video, 0, &mut counts).await;
    let mut final_frame = reference_frame;
    let arrival = now(&cx).0;
    impaired_transfer(&mut video, Some(loss), &mut counts, &cx);
    assert!(counts.dropped_fragments > 0);
    assert!(
        video.receiver.budget_usage().bytes > bytes,
        "receiver omitted fragment metadata"
    );
    assert!(
        video
            .presenter
            .present_next(&cx, &mut video.receiver)
            .await
            .unwrap()
            .is_none(),
        "incomplete HEVC reached decoder"
    );
    assert_eq!(output.snapshot().unwrap().pixels(), old_pixels);
    if late_reference {
        // Capture the dependent only AFTER the missing reference becomes too
        // old to display, so this is not a test that presents two old pictures.
        sleep_until(&cx, arrival + 65_000).await;
        final_frame = capture_pattern(&mut video, 1, &mut counts).await.1;
        impaired_transfer(&mut video, None, &mut counts, &cx);
        assert!(
            video
                .presenter
                .present_next(&cx, &mut video.receiver)
                .await
                .unwrap()
                .is_none(),
            "broken dependency reached decoder"
        );
    }
    // No additional capture/progress packet is needed to find final-frame loss.
    let repair_started_at = now(&cx).0;
    repair_missing(&mut video, &mut counts, &cx).await;
    assert_eq!(counts.repair_fragments, counts.dropped_fragments);
    if late_reference {
        let decode_started_at = now(&cx).0;
        decode_late_reference(&mut video, &cx, reference_frame, &mut output, &old_pixels).await;
        println!(
            "late_schedule pre_repair_us={} repair_us={} reference_decode_and_readback_us={}",
            repair_started_at - arrival,
            decode_started_at - repair_started_at,
            now(&cx).0 - decode_started_at
        );
    }
    let new_pixels = present_visible(
        &mut video,
        &mut output,
        &cx,
        final_frame,
        marker(u8::from(late_reference)),
    )
    .await;
    assert_ne!(
        new_pixels, old_pixels,
        "repaired HEVC did not change the visible image"
    );
    println!(
        "recovery={loss:?} late_reference={late_reference} elapsed_us={} counts={counts:?} injector_bytes={} subscription_fixed_bytes={}",
        now(&cx).0 - arrival,
        std::mem::size_of::<[Option<(PacketOffer, [u8; 1150])>; 4]>(),
        std::mem::size_of::<Subscription>()
    );
    match loss {
        Loss::FinalFragment => {
            drag_then_expire(&mut video.client, &mut video.observer, &source, &cx).await;
        }
        Loss::EntirePicture => queued_input_fence(&mut video, &source, &cx, true).await,
        Loss::EveryFifth => queued_input_fence(&mut video, &source, &cx, false).await,
    }
    expire_cache_and_close(&mut video, &cx).await;
}
async fn expire_cache_and_close(video: &mut Video, cx: &Cx) {
    // Service the actual host-clock cache deadline during idle, without
    // fabricating a capture/progress heartbeat to keep this subscription alive.
    while let Some(deadline) = video.subscription.next_deadline() {
        sleep_until(cx, deadline.as_micros()).await;
        video.subscription.tick().unwrap();
    }
    assert_eq!(video.subscription.cache_usage(), BudgetUsage::default());
    close_media(cx, &mut video.capture, &mut video.presenter).await;
}
#[test]
fn actual_hevc_final_fragment_loss_repairs_without_a_later_frame_and_idle_cache_expires() {
    run(recovery_case(Loss::FinalFragment, false));
}
#[test]
fn actual_hevc_lost_final_picture_is_announced_and_repaired_before_idle() {
    run(recovery_case(Loss::EntirePicture, false));
}
#[test]
fn actual_hevc_late_repaired_reference_unlocks_a_fresh_dependent_without_presenting_old_pixels() {
    run(recovery_case(Loss::EveryFifth, true));
}

// Local fixture routing for an already decoded, unchanged native configuration.
// This is NOT the missing exact-hvcC DecoderConfiguration/Configured handshake.
fn replace_recovery(video: &mut Video, cx: &Cx, generation: u64, first_binding: u32) {
    let epoch = MediaEpoch {
        configuration: config().generation,
        recovery: RecoveryGeneration::from_raw(generation),
    };
    let bindings = MediaBindings::new(
        first_binding,
        first_binding + 1,
        first_binding + 2,
        first_binding + 3,
    )
    .unwrap();
    video.subscription.recover(epoch, bindings).unwrap();
    video
        .presenter
        .recover(cx, &mut video.receiver, epoch, bindings)
        .unwrap();
}
async fn recovered_frame(
    video: &mut Video,
    output: &mut X11Surface,
    cx: &Cx,
    frame: u64,
    phase: u8,
) {
    // Both reliable chunks and datagrams use this fixture's 1150-byte cap.
    let mut packet = [0; 1150];
    while let Some(offer) = video.subscription.next_packet(&mut packet).unwrap() {
        video.subscription.authorize_write(&offer).unwrap();
        video
            .receiver
            .receive(offer.channel(), &packet[..offer.byte_len()], now(cx).0)
            .unwrap();
    }
    let receipt = video
        .presenter
        .present_next(cx, &mut video.receiver)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt.frame.as_raw(), frame);
    assert_eq!(receipt.stage, PresentationStage::SubmittedToCompositor);
    assert_eq!(
        receipt.decoded.epoch().recovery,
        RecoveryGeneration::from_raw(2)
    );
    let image = output.snapshot().unwrap();
    for (x, y) in [(144, 108), (160, 120), (176, 132)] {
        assert!(
            image.pixels()[4 * (y * 320 + x)..][..3]
                .iter()
                .zip(marker(phase))
                .all(|(&actual, expected)| actual.abs_diff(expected) < 10)
        );
    }
    assert_eq!(video.receiver.state(), ReceiveState::Streaming);
    assert_eq!(video.receiver.budget_usage(), BudgetUsage::default());
    assert!(
        video.client.stopped().is_some(),
        "new media resurrected old input view"
    );
}
fn deliver_next(video: &mut Video, cx: &Cx, packet: &mut [u8]) -> PacketOffer {
    let offer = video.subscription.next_packet(packet).unwrap().unwrap();
    video.subscription.authorize_write(&offer).unwrap();
    video
        .receiver
        .receive(offer.channel(), &packet[..offer.byte_len()], now(cx).0)
        .unwrap();
    offer
}
#[test]
fn actual_hevc_partial_recovery_is_fenced_then_fresh_idr_and_dependent_are_visible() {
    run(async {
        let source = Server::start();
        let viewer = Server::start();
        let cx = Cx::current().unwrap();
        let mut video = prepare(&source, &viewer, &cx).await;
        let mut output =
            X11Surface::capture(Some(&viewer.name), config().limits().unwrap()).unwrap();
        let initial = video.capture.capture(&video.control, true).await.unwrap();
        video.subscription.enqueue(initial).unwrap();
        let mut counts = RecoveryCounts::default();
        impaired_transfer(&mut video, None, &mut counts, &cx);
        let visible = present_visible(&mut video, &mut output, &cx, 1, [40, 80, 180, 255]).await;
        replace_recovery(&mut video, &cx, 1, 5);
        assert!(video.client.tick(now(&cx)).is_err());
        paint_pattern(&mut video, 0);
        let partial = video.capture.capture(&video.control, true).await.unwrap();
        assert!(partial.is_idr());
        let partial_bytes = partial.bytes().len();
        // Require a genuinely multi-chunk encoded IDR; no padding or fake AU.
        assert!(partial_bytes > video.wire.record_bytes());
        video.subscription.enqueue(partial).unwrap();
        let mut packet = [0; 1150];
        let progress = deliver_next(&mut video, &cx, &mut packet);
        assert_eq!(progress.channel(), Channel::MediaConfig);
        let old = deliver_next(&mut video, &cx, &mut packet);
        assert_eq!(old.channel(), Channel::Recovery);
        let partial_charge = video.receiver.budget_usage();
        assert!(partial_charge.bytes > partial_bytes);
        assert_eq!(partial_charge.pictures, 1);
        assert!(
            video
                .presenter
                .present_next(&cx, &mut video.receiver)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(output.snapshot().unwrap().pixels(), visible);
        // Model a reset of this reliable recovery stream. Discarding the old
        // remaining chunks cannot discard or silently complete a decoder input.
        replace_recovery(&mut video, &cx, 2, 9);
        assert_eq!(video.subscription.cache_usage(), BudgetUsage::default());
        assert_eq!(video.receiver.budget_usage(), BudgetUsage::default());
        assert_eq!(
            video.subscription.authorize_write(&old),
            Err(frd::media::Error::Send(SendError::Delivery(
                DeliveryError::StaleGeneration
            )))
        );
        assert_eq!(
            video
                .receiver
                .receive(old.channel(), &packet[..old.byte_len()], now(&cx).0),
            Err(DeliveryError::StaleGeneration)
        );
        paint_pattern(&mut video, 1);
        let fresh = video.capture.capture(&video.control, true).await.unwrap();
        assert!(fresh.is_idr());
        let fresh_frame = fresh.frame().as_raw();
        assert_eq!(
            fresh_frame, 3,
            "bootstrap consumes frame zero; shared capture identity restarted"
        );
        let fresh_bytes = fresh.bytes().len();
        video.subscription.enqueue(fresh).unwrap();
        recovered_frame(&mut video, &mut output, &cx, fresh_frame, 1).await;
        paint_pattern(&mut video, 0);
        let dependent = video.capture.capture(&video.control, false).await.unwrap();
        assert_eq!(
            dependent.kind(),
            fr_media::access_unit::FrameKind::Predicted {
                references: fr_media::access_unit::FrameId::from_raw(fresh_frame)
            }
        );
        let dependent_frame = dependent.frame().as_raw();
        let dependent_bytes = dependent.bytes().len();
        video.subscription.enqueue(dependent).unwrap();
        recovered_frame(&mut video, &mut output, &cx, dependent_frame, 0).await;
        println!(
            "recovery_replacement partial_idr_bytes={partial_bytes} partial_receiver_charge={} fresh_idr_bytes={fresh_bytes} dependent_bytes={dependent_bytes} retained_old_record_bytes={} record_scratch_bytes=1150 subscription_fixed_bytes={} sender_identity_allocations=1",
            partial_charge.bytes,
            old.byte_len(),
            std::mem::size_of::<Subscription>()
        );
        expire_cache_and_close(&mut video, &cx).await;
    });
}

fn receiver_config(wire: MediaLimits) -> ReceiveConfig {
    ReceiveConfig {
        limits: wire,
        bindings: MediaBindings::new(1, 2, 3, 4).unwrap(),
        epoch: MediaEpoch {
            configuration: config().generation,
            recovery: RecoveryGeneration::INITIAL,
        },
        policy: ReceivePolicy::default(),
    }
}
async fn queue_first_picture(video: &mut Video, cx: &Cx) -> Vec<(Channel, Vec<u8>)> {
    paint_pattern(video, 0);
    let unit = video.capture.capture(&video.control, true).await.unwrap();
    assert!(unit.is_idr());
    assert_eq!(unit.frame().as_raw(), 1);
    video.subscription.enqueue(unit).unwrap();
    let mut packets = Vec::new();
    let mut packet = [0; 1150];
    while let Some(offer) = video.subscription.next_packet(&mut packet).unwrap() {
        video.subscription.authorize_write(&offer).unwrap();
        receive_packet(video, &offer, &packet[..offer.byte_len()], cx);
        packets.push((offer.channel(), packet[..offer.byte_len()].to_vec()));
    }
    assert_eq!(video.receiver.budget_usage().pictures, 1);
    packets
}
#[test]
fn native_presenter_refuses_foreign_receiver_without_consuming_either_picture() {
    run(async {
        let source = Server::start();
        let viewer = Server::start();
        let cx = Cx::current().unwrap();
        let mut video = prepare(&source, &viewer, &cx).await;
        let c = receiver_config(video.wire);
        let mut foreign =
            ReceivePipeline::new(c, MediaBudget::new(c.limits.protocol()).unwrap()).unwrap();
        // Only this adversarial receiver is manually advanced. It must never
        // reach the real decoder despite matching every numeric configuration.
        foreign.decoder_configured(now(&cx).0).unwrap();
        let mut output =
            X11Surface::capture(Some(&viewer.name), config().limits().unwrap()).unwrap();
        let worker_id = video.presenter.worker_id();
        let packets = queue_first_picture(&mut video, &cx).await;
        for (channel, packet) in &packets {
            foreign.receive(*channel, packet, now(&cx).0).unwrap();
        }
        let before = (
            video.receiver.budget_usage(),
            foreign.budget_usage(),
            foreign.state(),
        );
        assert!(matches!(
            video.presenter.present_next(&cx, &mut foreign).await,
            Err(frd::media::Error::Receiver(DeliveryError::DecodeMismatch))
        ));
        assert_eq!(
            (
                video.receiver.budget_usage(),
                foreign.budget_usage(),
                foreign.state()
            ),
            before
        );
        assert_eq!(video.presenter.worker_id(), worker_id);
        present_visible(&mut video, &mut output, &cx, 1, marker(0)).await;
        assert!(video.client.tick(now(&cx)).unwrap());
        close_media(&cx, &mut video.capture, &mut video.presenter).await;
        assert!(matches!(
            video.client.tick(now(&cx)),
            Err(fr_client::input::presentation::Error::Media(
                ViewError::StaleBinding
            ))
        ));
        assert_eq!(video.client.stopped(), Some(StopReason::ViewStale));
        assert_eq!(
            video.receiver.tick(now(&cx).0),
            Err(DeliveryError::WrongState)
        );
        assert_eq!(video.receiver.state(), ReceiveState::Closed);
    });
}
#[test]
fn dropping_native_decode_fences_receiver_and_forbids_recovery_on_dead_worker() {
    run(async {
        use std::{
            future::{Future, poll_fn},
            pin::pin,
            task::Poll,
        };
        let source = Server::start();
        let viewer = Server::start();
        let cx = Cx::current().unwrap();
        let mut video = prepare(&source, &viewer, &cx).await;
        pause_decoder(&video.presenter);
        queue_first_picture(&mut video, &cx).await;
        {
            let mut operation = pin!(video.presenter.present_next(&cx, &mut video.receiver));
            poll_fn(|task| {
                assert!(operation.as_mut().poll(task).is_pending());
                Poll::Ready(())
            })
            .await;
            // No completion receipt exists. Any native effect already submitted
            // is unknown, not claimed rolled back by dropping the operation.
        }
        assert_eq!(video.receiver.state(), ReceiveState::Closed);
        assert_eq!(video.receiver.budget_usage(), BudgetUsage::default());
        assert!(video.client.tick(now(&cx)).is_err());
        assert!(
            video
                .presenter
                .recover(
                    &cx,
                    &mut video.receiver,
                    MediaEpoch {
                        configuration: config().generation,
                        recovery: RecoveryGeneration::from_raw(2)
                    },
                    MediaBindings::new(5, 6, 7, 8).unwrap()
                )
                .is_err()
        );
        assert!(
            !video
                .presenter
                .reap(
                    &cx,
                    Deadline::after(&cx, Duration::from_millis(500)).unwrap()
                )
                .await
                .unwrap()
                .success()
        );
        video.capture.worker_mut().abort();
        video
            .capture
            .worker_mut()
            .reap(
                &cx,
                Deadline::after(&cx, Duration::from_millis(500)).unwrap(),
            )
            .await
            .unwrap();
    });
}
#[test]
fn failed_native_startup_never_configures_receiver_and_drop_invalidates_view() {
    run(async {
        let source = Server::start();
        let viewer = Server::start();
        let cx = Cx::current().unwrap();
        let mut video = prepare(&source, &viewer, &cx).await;
        let record = bootstrap_record(&mut video.capture, &video.control, config()).await;
        let c = receiver_config(video.wire);
        let mut receiver =
            ReceivePipeline::new(c, MediaBudget::new(c.limits.protocol()).unwrap()).unwrap();
        // A real failed child launch must not leave a configured receiver. The
        // missing executable is a failure witness, not a mock decoder backend.
        let missing =
            std::env::temp_dir().join(format!("fr-missing-decoder-{}", std::process::id()));
        assert!(!missing.exists());
        let launch = Launch::new(&missing, &viewer.name, None, Role::Present, 99).unwrap();
        assert!(matches!(
            Presenter::start(&cx, launch, config(), &record, &mut receiver).await,
            Err(frd::media::Error::Worker(frd::worker::Error::SpawnFailed))
        ));
        assert_eq!(receiver.state(), ReceiveState::Closed);
        assert_eq!(receiver.budget_usage(), BudgetUsage::default());
        drop(video.presenter);
        assert!(matches!(
            video.client.tick(now(&cx)),
            Err(fr_client::input::presentation::Error::Media(
                ViewError::StaleBinding
            ))
        ));
        assert_eq!(video.client.stopped(), Some(StopReason::ViewStale));
        assert_eq!(
            video.receiver.tick(now(&cx).0),
            Err(DeliveryError::WrongState)
        );
        video.capture.worker_mut().abort();
        video
            .capture
            .worker_mut()
            .reap(
                &cx,
                Deadline::after(&cx, Duration::from_millis(500)).unwrap(),
            )
            .await
            .unwrap();
    });
}

#[test]
fn canceled_native_stop_cannot_rebind_before_receiver_watchdog_runs() {
    run(async {
        let source = Server::start();
        let viewer = Server::start();
        let cx = Cx::current().unwrap();
        let mut video = prepare(&source, &viewer, &cx).await;
        // This context owns only the stop attempt. The healthy cleanup clock
        // remains available to prove no new recovery can reauthorize this owner.
        let cancel_runtime = runtime();
        let cancelled = cancel_runtime.request_cx_with_budget(Budget::INFINITE);
        let until = Deadline::after(&cancelled, Duration::from_millis(500)).unwrap();
        cancelled.cancel_fast(asupersync::types::CancelKind::User);
        assert!(matches!(
            video.presenter.stop(&cancelled, until).await,
            Err(frd::media::Error::Worker(frd::worker::Error::Cancelled))
        ));
        // Deliberately NO receiver.tick between revocation and rebind attempt.
        assert!(
            video
                .presenter
                .recover(
                    &cx,
                    &mut video.receiver,
                    MediaEpoch {
                        configuration: config().generation,
                        recovery: RecoveryGeneration::from_raw(2)
                    },
                    MediaBindings::new(5, 6, 7, 8).unwrap()
                )
                .is_err()
        );
        assert!(
            !video
                .presenter
                .reap(
                    &cx,
                    Deadline::after(&cx, Duration::from_millis(500)).unwrap()
                )
                .await
                .unwrap()
                .success()
        );
        assert!(video.client.tick(now(&cx)).is_err());
        video.capture.worker_mut().abort();
        video
            .capture
            .worker_mut()
            .reap(
                &cx,
                Deadline::after(&cx, Duration::from_millis(500)).unwrap(),
            )
            .await
            .unwrap();
    });
}

// Stop only the decoder child owned by this Presenter. Confirm kernel stop
// state before polling; speed of a real worker cannot turn the drop witness
// into an already-completed operation. Presenter/Worker Drop sends SIGKILL even
// during unwinding, which also terminates a stopped child without resuming it.
fn pause_decoder(presenter: &Presenter) {
    let pid = presenter.worker_id().unwrap();
    assert!(
        Command::new("kill")
            .args(["-STOP", &pid.to_string()])
            .status()
            .unwrap()
            .success()
    );
    let until = std::time::Instant::now() + Duration::from_secs(1);
    loop {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
        if status.lines().any(|line| line.starts_with("State:\tT")) {
            return;
        }
        assert!(
            std::time::Instant::now() < until,
            "owned decoder did not stop"
        );
        thread::sleep(Duration::from_millis(1));
    }
}

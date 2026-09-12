//! Real startup, four negotiated channels, clock exchange, input and receipts.
//! Capture/decode and native effects are explicit fixtures, not HEVC/X11 claims.
use super::*;
use crate::{
    input_agent::{self, Driver, Seat},
    media::{ObservationControl, clock::ClockSync},
    session_startup::{
        ControlledHost,
        running::{
            controlled::tests::attach,
            tests::{pair_initialized, run},
        },
        tests::support,
    },
};
use fr_client::input::{InputClient, Policy};
use fr_core::{
    ids::*,
    input::*,
    input_submission::{
        Capabilities, Capability, InputSink, Operation as Op, PlatformError, Submission,
    },
    limits::ProtocolLimits,
    time::HostInstant,
};
use fr_media::{delivery::*, freshness::ClockPolicy};
use fr_wire::{attachment::MediaRole, negotiation::Capability as WireCapability, *};
use std::sync::Mutex;

fn creds() -> InputCredentials {
    InputCredentials {
        session: RemoteSessionId::from_raw(13),
        lease: InputLeaseId::from_raw(19),
        ticket: InputTicketId::from_raw(23),
        view: InputView {
            geometry: DisplayGeometryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
    }
}
fn bounds() -> InputBounds {
    InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap()
}
fn caps() -> Capabilities {
    Capabilities::default()
        .with(Capability::Keys)
        .with(Capability::Absolute)
}
fn key(down: bool) -> Action<'static> {
    Action::Key {
        key: PhysicalKey::new(4).unwrap(),
        transition: if down {
            KeyTransition::Press
        } else {
            KeyTransition::Release
        },
    }
}
fn nonce(counter: &mut u128) -> Result<u128, ()> {
    *counter = counter.checked_add(1).ok_or(())?;
    Ok(*counter)
}
#[derive(Default)]
struct Effects {
    keys: Vec<bool>,
    operations: Vec<Op>,
}
struct Sink(Arc<Mutex<Effects>>);
impl InputSink for Sink {
    fn prepare(&mut self, _: Op) -> Result<(), PlatformError> {
        Ok(())
    }
    fn submit(&mut self, op: Op) -> Submission {
        let mut effects = self.0.lock().unwrap();
        if let Op::Key { transition, .. } = op {
            effects.keys.push(transition != KeyTransition::Release);
        }
        effects.operations.push(op);
        Submission::Submitted
    }
}
#[allow(clippy::unnecessary_wraps)]
fn block(_: Route, _: &[u8]) -> Result<Disposition, ()> {
    Ok(Disposition::Blocked)
}
struct Fixture {
    presenter: Option<crate::media::Presenter>,
    video: quic::DatagramRoute,
    host: ControlledHost,
    viewer: ControlledViewer,
    host_clock: ClockSync,
    host_media: NegotiatedMedia,
    receiver: ReceivePipeline,
    descriptor: FrameDescriptor,
    last_announce: u64,
    driver: Option<Driver>,
    seat: Seat,
    effects: Arc<Mutex<Effects>>,
    observation: ObservationControl,
}
async fn fixture(client_cx: &Cx, host_cx: &Cx) -> Fixture {
    Box::pin(fixture_with_caps(client_cx, host_cx, caps())).await
}
#[allow(clippy::too_many_lines)]
async fn fixture_with_caps(client_cx: &Cx, host_cx: &Cx, capabilities: Capabilities) -> Fixture {
    Box::pin(fixture_with_decoder(
        client_cx,
        host_cx,
        capabilities,
        false,
    ))
    .await
}
#[allow(clippy::too_many_lines)]
async fn fixture_with_decoder(
    client_cx: &Cx,
    host_cx: &Cx,
    capabilities: Capabilities,
    decode: bool,
) -> Fixture {
    Box::pin(fixture_with_wire_feedback(
        client_cx,
        host_cx,
        capabilities,
        decode,
        false,
    ))
    .await
}
#[allow(clippy::too_many_lines)]
async fn fixture_with_wire_feedback(
    client_cx: &Cx,
    host_cx: &Cx,
    capabilities: Capabilities,
    decode: bool,
    feedback: bool,
) -> Fixture {
    let mut wire_capabilities: Vec<WireCapability> = [
        fr_wire::clock::CAPABILITY,
        decoder::CAPABILITY,
        attachment::INPUT_CAPABILITY,
        attachment::CAPABILITY,
        attachment::DELIVERY_CAPABILITY,
    ]
    .into_iter()
    .map(|s| WireCapability {
        name: s.into(),
        version: 1,
        required: true,
    })
    .collect();
    if feedback {
        wire_capabilities.push(WireCapability {
            name: fr_wire::receiver_metrics::CAPABILITY.into(),
            version: 1,
            required: false,
        });
    }
    wire_capabilities.sort_by(|a, b| a.name.cmp(&b.name));
    let (mut host, mut viewer) = pair_initialized(client_cx, host_cx, wire_capabilities, |host| {
        let host_result = host.authority.as_mut().unwrap();
        let stamp = HostInstant::from_micros(now(host_cx).unwrap());
        host_result.mark_view_ready(stamp).unwrap();
        host_result.grant_lease(creds().lease, stamp).unwrap();
        host_result
            .issue_input_ticket(creds().lease, creds().ticket, stamp)
            .unwrap();
    })
    .await;
    let (hc, vc) = attach(
        &mut host,
        &mut viewer,
        client_cx,
        host_cx,
        MediaRole::Configuration,
        8,
    )
    .await;
    let (hr, vr) = attach(
        &mut host,
        &mut viewer,
        client_cx,
        host_cx,
        MediaRole::Recovery,
        9,
    )
    .await;
    let (hi, vi) = attach(
        &mut host,
        &mut viewer,
        client_cx,
        host_cx,
        MediaRole::Input,
        10,
    )
    .await;
    let (hv, vv) = attach(
        &mut host,
        &mut viewer,
        client_cx,
        host_cx,
        MediaRole::Video,
        11,
    )
    .await;
    let parent = host.binding();
    let selection = host.selection().clone();
    let observation = host.observation().unwrap();
    let host_media = NegotiatedMedia::new(host.io().unwrap().0, &selection, &hc, &hr, &hv).unwrap();
    let media = NegotiatedMedia::new(viewer.io().unwrap().0, &selection, &vc, &vr, &vv).unwrap();
    let video = hv
        .completed_on(host.io().unwrap().0)
        .unwrap()
        .datagram
        .unwrap();
    let hn = NegotiatedInput::new(host.io().unwrap().0, &selection, &hc, hi).unwrap();
    let vn = NegotiatedInput::new(viewer.io().unwrap().0, &selection, &vc, vi).unwrap();
    let (hq, routes) = host.io().unwrap();
    let mut host_clock =
        ClockSync::host(observation.clone(), hq, routes, parent, &selection).unwrap();
    let (vq, routes) = viewer.io().unwrap();
    let mut clock = ClockSync::viewer(
        client_cx.clone(),
        vq,
        routes,
        parent,
        &selection,
        ClockPolicy::default(),
    )
    .unwrap();
    let mut counter = 5000;
    let until = now(host_cx).unwrap() + 1_000_000;
    let correlation = loop {
        assert!(now(host_cx).unwrap() < until);
        clock.receive(viewer.io().unwrap().0, block).unwrap();
        clock.service(viewer.io().unwrap().0).unwrap();
        host_clock.receive(host.io().unwrap().0, block).unwrap();
        host_clock.service(host.io().unwrap().0).unwrap();
        let (host_result, viewer_result) = Box::pin(support::both(
            host.drive(Duration::from_millis(1), || nonce(&mut counter), block),
            viewer.drive(Duration::from_millis(1), block),
        ))
        .await;
        host_result.unwrap();
        viewer_result.unwrap();
        if let Some(sample) = clock.correlation(viewer.io().unwrap().0).unwrap() {
            break sample;
        }
    };
    let session = observation
        .input_session(creds(), bounds(), capabilities)
        .unwrap();
    let effects = Arc::new(Mutex::new(Effects::default()));
    let sink = effects.clone();
    let seat = Seat::default();
    let (agent, driver) = seat
        .start(
            host_cx.clone(),
            session,
            input_agent::Route::new(10, ProtocolLimits::ABSOLUTE),
            move || Ok(Sink(sink)),
            |_| true,
        )
        .unwrap();
    let mut native = hn
        .into_host(host_cx.clone(), agent, host.io().unwrap().0, &observation)
        .unwrap();
    let mut input = InputClient::new(
        creds(),
        10,
        bounds(),
        capabilities,
        ProtocolLimits::ABSOLUTE,
        Policy::default(),
        ClientInstant(now(client_cx).unwrap()),
    )
    .unwrap();
    // Bind the initial expiry to an actual native ticket received on QUIC.
    let mut ticket_arrived = false;
    let until = now(host_cx).unwrap() + 500_000;
    while !ticket_arrived {
        assert!(now(host_cx).unwrap() < until);
        native
            .service(host.io().unwrap().0, || observation.check().is_ok())
            .unwrap();
        native
            .renew_ticket(
                host.io().unwrap().0,
                || observation.check().is_ok(),
                || Some(InputTicketId::from_raw(1000)),
            )
            .unwrap();
        let (host_result, viewer_result) = Box::pin(support::both(
            host.drive(Duration::from_millis(1), || nonce(&mut counter), block),
            viewer.drive(Duration::from_millis(1), |_, bytes| {
                if bytes[6..8] == (Kind::InputTicket as u16).to_be_bytes() {
                    input
                        .accept_ticket(bytes, correlation, ClientInstant(now(client_cx).unwrap()))
                        .unwrap();
                    ticket_arrived = true;
                    Ok(Disposition::Consumed)
                } else {
                    Ok(Disposition::Blocked)
                }
            }),
        ))
        .await;
        host_result.unwrap();
        viewer_result.unwrap();
    }
    let limits = media.limits();
    let mut receiver = ReceivePipeline::new(
        media
            .receiver_config(
                viewer.io().unwrap().0,
                ReceivePolicy {
                    display_budget_micros: if decode { 200_000 } else { 50_000 },
                    ..ReceivePolicy::default()
                },
            )
            .unwrap(),
        MediaBudget::new(limits.protocol()).unwrap(),
    )
    .unwrap();
    let presenter = if decode {
        Some(
            crate::media::Presenter::stream_fixture(
                client_cx,
                viewer.io().unwrap().0,
                &media,
                &mut receiver,
            )
            .await,
        )
    } else {
        receiver
            .decoder_configured(now(client_cx).unwrap())
            .unwrap();
        None
    };
    let stamp = now(client_cx).unwrap();
    let mut input =
        PresentedInput::new(input, &receiver, correlation, ClientInstant(stamp)).unwrap();
    input
        .confirm_mapping(creds().session, creds().view, ClientInstant(stamp))
        .unwrap();
    let mut bytes = [0; 1150];
    let len = encode_recovery(
        RecoveryChunk {
            frame: 0,
            total_bytes: 4,
            offset: 0,
            capture_micros: stamp,
            bytes: b"data",
        },
        media.bindings().for_channel(Channel::Recovery),
        &limits,
        &mut bytes,
    )
    .unwrap();
    receiver
        .receive(Channel::Recovery, &bytes[..len], stamp)
        .unwrap();
    let unit = receiver.take_decodable(stamp).unwrap().unwrap();
    let descriptor = unit.descriptor();
    let len = encode_progress(
        Progress {
            descriptor,
            observed_micros: stamp,
            observation: SourceObservation::Captured,
            pipeline: PipelineState::Running,
        },
        media.bindings().for_channel(Channel::MediaConfig),
        &limits,
        &mut bytes,
    )
    .unwrap();
    receiver
        .receive(Channel::MediaConfig, &bytes[..len], stamp)
        .unwrap();
    input
        .progress(&bytes[..len], &limits, ClientInstant(stamp))
        .unwrap();
    input
        .decoded(
            receiver.complete_decode(&unit, stamp).unwrap(),
            true,
            ClientInstant(stamp),
        )
        .unwrap();
    input.visible(0, ClientInstant(stamp)).unwrap();
    let viewer = viewer.into_controlled(vn, media, input, clock).unwrap();
    Fixture {
        presenter,
        video,
        last_announce: stamp,
        host: host.into_controlled(native).unwrap(),
        viewer,
        host_clock,
        host_media,
        receiver,
        descriptor,
        driver: Some(driver),
        seat,
        effects,
        observation,
    }
}
fn announce(
    host: &mut ControlledHost,
    media: &NegotiatedMedia,
    observation: &ObservationControl,
    host_cx: &Cx,
    d: FrameDescriptor,
    unknown: bool,
) {
    let q = host.io().unwrap().0;
    let limits = media.limits();
    let mut bytes = [0; 1150];
    let counter = encode_progress(
        Progress {
            descriptor: d,
            observed_micros: now(host_cx).unwrap(),
            observation: if unknown {
                SourceObservation::Unknown
            } else {
                SourceObservation::QualifiedUnchanged
            },
            pipeline: PipelineState::Idle,
        },
        media.bindings().for_channel(Channel::MediaConfig),
        &limits,
        &mut bytes,
    )
    .unwrap();
    // Find the exact negotiated host progress route from its descriptor pair.
    let route = media.progress_for_test(q);
    match q.send(
        host_cx,
        Route::Stream(route),
        &bytes[..counter],
        now(host_cx).unwrap() + 200_000,
        || observation.check().is_ok(),
    ) {
        Ok(()) | Err(quic::Error::Backpressure) => {}
        other => panic!("announce {other:?}"),
    }
}
async fn turn(
    state: &mut Fixture,
    client_cx: &Cx,
    host_cx: &Cx,
    counter: &mut u128,
    stamp: &mut u128,
) -> (Result<(), crate::session_startup::Error>, Result<(), Error>) {
    if now(host_cx).unwrap() >= state.last_announce + 50_000 {
        announce(
            &mut state.host,
            &state.host_media,
            &state.observation,
            host_cx,
            state.descriptor,
            false,
        );
        state.last_announce = now(host_cx).unwrap();
    }
    state
        .host_clock
        .receive(state.host.io().unwrap().0, block)
        .unwrap();
    state
        .host_clock
        .service(state.host.io().unwrap().0)
        .unwrap();
    Box::pin(support::both(state.host.drive(Duration::from_millis(1),||nonce(counter),||{*stamp+=1;Some(InputTicketId::from_raw(*stamp))},block),state.viewer.drive(Duration::from_millis(1),|_|{},|route,bytes| {
        if matches!(route,Route::Stream(s) if s.messages==quic::Messages::Exact(Kind::Progress as u16)) {
            state.receiver.receive(Channel::MediaConfig,bytes,now(client_cx).unwrap()).map_err(|_|())?;Ok(Disposition::Consumed)
        }else{Ok(Disposition::Blocked)}
    }))).await
}
#[test]
fn persistent_pair_renews_clock_view_control_and_tickets_without_new_pixels() {
    run(|client_cx, host_cx| async move {
        let mut state = Box::pin(fixture(&client_cx, &host_cx)).await;
        let mut counter = 10000;
        let mut stamp = 20000;
        let driver = state.driver.take().unwrap();
        let ((), shutdown) = Box::pin(support::both(
            async {
                let start = now(&client_cx).unwrap();
                let before_clock = state.viewer.clock_at;
                assert_eq!(state.viewer.action(key(true)).unwrap().sequence, 0);
                let mut released = false;
                while now(&client_cx).unwrap() < start + 3_300_000 {
                    let (host_result, viewer_result) =
                        turn(&mut state, &client_cx, &host_cx, &mut counter, &mut stamp).await;
                    host_result.unwrap();
                    viewer_result.unwrap();
                    if !released
                        && now(&client_cx).unwrap() > start + 3_100_000
                        && state.viewer.pending_actions() == 0
                    {
                        assert_eq!(state.viewer.action(key(false)).unwrap().sequence, 1);
                        released = true;
                    }
                }
                assert!(released);
                assert_eq!(state.viewer.pending_actions(), 0);
                assert_eq!(state.effects.lock().unwrap().keys, [true, false]);
                assert!(state.viewer.clock_at > before_clock);
                assert_eq!(state.descriptor.frame, 0);
                state.viewer.close();
                state.host.close();
            },
            driver,
        ))
        .await;
        assert!(shutdown.handoff_safe());
        assert!(!state.seat.is_occupied());
    });
}
#[test]
fn unpolled_viewer_drive_stops_the_existing_owner() {
    run(|client_cx, host_cx| async move {
        let mut state = Box::pin(fixture(&client_cx, &host_cx)).await;
        let _ = state.viewer.action(key(true)).unwrap();
        drop(state.viewer.drive(Duration::from_millis(1), |_| {}, block));
        assert!(state.viewer.is_closed());
        assert!(!state.viewer.pending_send());
        assert_eq!(state.effects.lock().unwrap().keys, [] as [bool; 0]);
        state.host.close();
        assert!(state.driver.take().unwrap().await.handoff_safe());
    });
}
#[test]
fn lifecycle_handle_fences_queued_input_without_borrowing_viewer() {
    run(|client_cx, host_cx| async move {
        let mut state = Box::pin(fixture(&client_cx, &host_cx)).await;
        let stop = state.viewer.control();
        let _ = state.viewer.action(key(true)).unwrap();
        let drive = state.viewer.drive(Duration::from_millis(1), |_| {}, block);
        stop.stop();
        assert!(drive.await.is_err());
        assert!(state.viewer.is_closed());
        assert_eq!(state.effects.lock().unwrap().keys, [] as [bool; 0]);
        state.host.close();
        assert!(state.driver.take().unwrap().await.handoff_safe());
    });
}

// Synthetic successor bytes exercise the real bounded receiver/token lifetime.
// They are never passed to a native decoder or described as actual video.
fn submit_successor(state: &mut Fixture, client_cx: &Cx) {
    let stamp = now(client_cx).unwrap();
    let limits = state.viewer.media.limits();
    let mut bytes = [0; 1150];
    let descriptor = FrameDescriptor {
        frame: 1,
        reference: Some(0),
        total_bytes: 4,
        stride: 4,
        capture_micros: stamp,
    };
    let size = encode_fragment(
        Fragment {
            descriptor,
            index: 0,
            bytes: b"next",
        },
        state.viewer.media.bindings().for_channel(Channel::Video),
        &limits,
        &mut bytes,
    )
    .unwrap();
    state
        .receiver
        .receive(Channel::Video, &bytes[..size], stamp)
        .unwrap();
    let unit = state.receiver.take_decodable(stamp).unwrap().unwrap();
    let descriptor = unit.descriptor();
    let size = encode_progress(
        Progress {
            descriptor,
            observed_micros: stamp,
            observation: SourceObservation::Captured,
            pipeline: PipelineState::Running,
        },
        state
            .viewer
            .media
            .bindings()
            .for_channel(Channel::MediaConfig),
        &limits,
        &mut bytes,
    )
    .unwrap();
    state
        .receiver
        .receive(Channel::MediaConfig, &bytes[..size], stamp)
        .unwrap();
    state
        .viewer
        .input
        .progress(&bytes[..size], &limits, ClientInstant(stamp))
        .unwrap();
    state
        .viewer
        .decoded(state.receiver.complete_decode(&unit, stamp).unwrap(), true)
        .unwrap();
    state.descriptor = descriptor;
}

#[test]
fn new_submission_pauses_sends_until_independent_visibility_without_retiming_action() {
    run(|client_cx, host_cx| async move {
        let mut state = Box::pin(fixture(&client_cx, &host_cx)).await;
        let original = state.viewer.action(key(true)).unwrap();
        let before = state.viewer.pending.as_ref().unwrap();
        let original_bytes = before.bytes[..before.len].to_vec();
        let original_until = before.until;
        submit_successor(&mut state, &client_cx);
        let before_wait = std::time::Instant::now();
        state
            .viewer
            .drive(Duration::from_millis(3), |_| {}, block)
            .await
            .unwrap();
        assert!(before_wait.elapsed() >= Duration::from_millis(3));
        assert!(!state.viewer.is_closed());
        let pending = state.viewer.pending.as_ref().unwrap();
        assert_eq!(pending.until, original_until);
        assert_eq!(&pending.bytes[..pending.len], original_bytes);
        assert_eq!(state.effects.lock().unwrap().keys, [] as [bool; 0]);
        assert_eq!(state.viewer.action(key(false)), Err(Error::Backpressure));
        assert_eq!(state.viewer.visible(1).unwrap().frame, 1);
        assert_eq!(original.sequence, 0);
        state.viewer.close();
        state.host.close();
        assert!(state.driver.take().unwrap().await.handoff_safe());
    });
}

#[test]
fn empty_presentation_gap_is_temporary_but_does_not_permit_new_input() {
    run(|client_cx, host_cx| async move {
        let mut state = Box::pin(fixture(&client_cx, &host_cx)).await;
        submit_successor(&mut state, &client_cx);
        assert_eq!(
            state.viewer.action(key(true)),
            Err(Error::View(presentation::Error::Input(
                fr_client::input::Error::NoPresentedView
            )))
        );
        assert!(!state.viewer.is_closed());
        assert_eq!(state.viewer.pending_actions(), 0);
        state
            .viewer
            .drive(Duration::ZERO, |_| {}, block)
            .await
            .unwrap();
        state.viewer.visible(1).unwrap();
        assert_eq!(state.viewer.action(key(true)).unwrap().sequence, 0);
        state.viewer.close();
        state.host.close();
        assert!(state.driver.take().unwrap().await.handoff_safe());
    });
}

#[test]
fn lost_visibility_callback_keeps_the_preceding_view_expiry() {
    run(|client_cx, host_cx| async move {
        let mut state = Box::pin(fixture(&client_cx, &host_cx)).await;
        let until = state
            .viewer
            .input
            .view_deadline(ClientInstant(now(&client_cx).unwrap()))
            .unwrap()
            .0;
        submit_successor(&mut state, &client_cx);
        while now(&client_cx).unwrap() < until {
            state
                .viewer
                .drive(Duration::ZERO, |_| {}, block)
                .await
                .unwrap();
            asupersync::time::sleep(client_cx.now(), Duration::from_millis(1)).await;
        }
        assert!(
            state
                .viewer
                .drive(Duration::ZERO, |_| {}, block)
                .await
                .is_err()
        );
        assert!(state.viewer.is_closed());
        assert!(state.viewer.visible(1).is_err());
        state.host.close();
        assert!(state.driver.take().unwrap().await.handoff_safe());
    });
}

#[test]
fn receiver_destruction_fences_an_already_encoded_action_before_network_submission() {
    run(|client_cx, host_cx| async move {
        let mut state = Box::pin(fixture(&client_cx, &host_cx)).await;
        assert_eq!(state.viewer.action(key(true)).unwrap().sequence, 0);
        state.receiver.close();
        assert!(
            state
                .viewer
                .drive(Duration::from_millis(1), |_| {}, block)
                .await
                .is_err()
        );
        assert!(state.viewer.is_closed());
        assert_eq!(state.effects.lock().unwrap().keys, [] as [bool; 0]);
        state.host.close();
        assert!(state.driver.take().unwrap().await.handoff_safe());
    });
}

#[test]
fn local_slot_backpressure_does_not_consume_action_or_pointer_identities() {
    run(|client_cx, host_cx| async move {
        let mut state = Box::pin(fixture(&client_cx, &host_cx)).await;
        assert_eq!(state.viewer.action(key(true)).unwrap().sequence, 0);
        assert_eq!(state.viewer.action(key(false)), Err(Error::Backpressure));
        assert_eq!(
            state.viewer.pointer(DesktopPoint { x: 1, y: 1 }),
            Err(Error::Backpressure)
        );
        assert_eq!(state.viewer.pending_actions(), 1);
        let driver = state.driver.take().unwrap();
        let ((), shutdown) = Box::pin(support::both(
            async {
                let mut counter = 10000;
                let mut tickets = 20000;
                let until = now(&client_cx).unwrap() + 300_000;
                while state.viewer.pending_send() || state.viewer.pending_actions() != 0 {
                    assert!(now(&client_cx).unwrap() < until);
                    let (left, right) =
                        turn(&mut state, &client_cx, &host_cx, &mut counter, &mut tickets).await;
                    left.unwrap();
                    right.unwrap();
                }
                assert_eq!(
                    state
                        .viewer
                        .pointer(DesktopPoint { x: 1, y: 1 })
                        .unwrap()
                        .sequence,
                    0
                );
                while state.viewer.pending_send() {
                    let (left, right) =
                        turn(&mut state, &client_cx, &host_cx, &mut counter, &mut tickets).await;
                    left.unwrap();
                    right.unwrap();
                }
                assert_eq!(state.viewer.action(key(false)).unwrap().sequence, 1);
                state.viewer.close();
                state.host.close();
            },
            driver,
        ))
        .await;
        assert!(shutdown.handoff_safe());
    });
}

#[test]
fn real_send_backpressure_and_fresh_source_do_not_retime_an_encoded_action() {
    run(|client_cx, host_cx| async move {
        let mut state = Box::pin(fixture(&client_cx, &host_cx)).await;
        let limits = state.viewer.media.limits();
        let mut bytes = [0; 128];
        let length = fr_wire::authority::encode(
            fr_wire::authority::Message::Response {
                scope: fr_wire::authority::Scope::Observation,
                nonce: 9000,
            },
            fr_wire::authority::Binding {
                channel: 7,
                session: creds().session,
            },
            limits.protocol(),
            &mut bytes,
            fr_wire::input::InputDirection::ViewerToHost,
            fr_wire::input::InputDelivery::Reliable,
        )
        .unwrap();
        let q = &mut state.viewer.session.transport;
        let route = Route::Stream(state.viewer.session.routes.outbound);
        // Actual retained-queue pressure; this filler is never transmitted or
        // accepted as authority. No counter or reservation is edited for the test.
        let end = now(&client_cx).unwrap() + 1_000_000;
        for attempt in 0..2 {
            match q.send(&client_cx, route, &bytes[..length], end, || true) {
                Ok(()) => assert_eq!(attempt, 0),
                Err(quic::Error::Backpressure) => break,
                other => panic!("unexpected queue result {other:?}"),
            }
        }
        assert_eq!(state.viewer.action(key(true)).unwrap().sequence, 0);
        let pending = state.viewer.pending.as_ref().unwrap();
        let original = pending.bytes[..pending.len].to_vec();
        let until = pending.until;
        state.viewer.send().unwrap();
        assert!(state.viewer.pending_send());
        let halfway = now(&client_cx).unwrap() + (until - now(&client_cx).unwrap()) / 2;
        while now(&client_cx).unwrap() < halfway {
            asupersync::time::sleep(client_cx.now(), Duration::from_millis(1)).await;
        }
        let stamp = now(&client_cx).unwrap();
        let mut progress = [0; 1150];
        let length = encode_progress(
            Progress {
                descriptor: state.descriptor,
                observed_micros: stamp,
                observation: SourceObservation::QualifiedUnchanged,
                pipeline: PipelineState::Idle,
            },
            state
                .viewer
                .media
                .bindings()
                .for_channel(Channel::MediaConfig),
            &limits,
            &mut progress,
        )
        .unwrap();
        state
            .receiver
            .receive(Channel::MediaConfig, &progress[..length], stamp)
            .unwrap();
        state
            .viewer
            .input
            .progress(&progress[..length], &limits, ClientInstant(stamp))
            .unwrap();
        assert!(
            state
                .viewer
                .input
                .view_deadline(ClientInstant(stamp))
                .unwrap()
                .0
                > until
        );
        state.viewer.send().unwrap();
        let pending = state.viewer.pending.as_ref().unwrap();
        assert_eq!(pending.until, until);
        assert_eq!(&pending.bytes[..pending.len], original);
        while now(&client_cx).unwrap() < until {
            asupersync::time::sleep(client_cx.now(), Duration::from_millis(1)).await;
        }
        assert_eq!(
            state.viewer.drive(Duration::ZERO, |_| {}, block).await,
            Err(Error::Expired)
        );
        assert!(state.viewer.is_closed());
        assert_eq!(state.viewer.pending_actions(), 1);
        assert_eq!(state.effects.lock().unwrap().keys, [] as [bool; 0]);
        state.host.close();
        assert!(state.driver.take().unwrap().await.handoff_safe());
    });
}

#[test]
fn actual_feedback_remains_interpretable_after_viewer_closure_without_replay() {
    run(|client_cx, host_cx| async move {
        let mut state = Box::pin(fixture(&client_cx, &host_cx)).await;
        assert_eq!(state.viewer.action(key(true)).unwrap().sequence, 0);
        let driver = state.driver.take().unwrap();
        let ((), shutdown) = Box::pin(support::both(
            async {
                state
                    .viewer
                    .drive(Duration::ZERO, |_| {}, block)
                    .await
                    .unwrap();
                let mut wire = None;
                let mut counter = 10000;
                let mut tickets = 20000;
                let until = now(&client_cx).unwrap() + 300_000;
                while wire.is_none() {
                    assert!(now(&client_cx).unwrap() < until);
                    let (left, right) = Box::pin(support::both(
                        state.host.drive(
                            Duration::from_millis(1),
                            || nonce(&mut counter),
                            || {
                                tickets += 1;
                                Some(InputTicketId::from_raw(tickets))
                            },
                            block,
                        ),
                        state
                            .viewer
                            .session
                            .drive(Duration::from_millis(1), |_, bytes| {
                                if bytes.get(6..8)
                                    == Some(&(Kind::InputResult as u16).to_be_bytes())
                                {
                                    assert!(wire.is_none());
                                    wire = Some(bytes.to_vec());
                                    Ok(Disposition::Consumed)
                                } else {
                                    Ok(Disposition::Blocked)
                                }
                            }),
                    ))
                    .await;
                    left.unwrap();
                    right.unwrap();
                }
                assert_eq!(state.effects.lock().unwrap().keys, [true]);
                assert_eq!(state.viewer.pending_actions(), 1);
                let stamp = ClientInstant(now(&client_cx).unwrap());
                state.viewer.close();
                state.host.close();
                let event = state.viewer.retained_result(&wire.unwrap(), stamp).unwrap();
                assert_eq!(state.viewer.pending_actions(), 0);
                assert_eq!(state.viewer.last_result(), Some(event));
                assert_eq!(state.viewer.action(key(false)), Err(Error::Closed));
            },
            driver,
        ))
        .await;
        assert!(shutdown.handoff_safe());
        assert_eq!(state.effects.lock().unwrap().keys, [true, false]);
    });
}

#[test]
fn progress_callback_receiver_failure_cannot_enable_followup_input() {
    run(|client_cx, host_cx| async move {
        let mut state = Box::pin(fixture(&client_cx, &host_cx)).await;
        let driver = state.driver.take().unwrap();
        let ((),shutdown)=Box::pin(support::both(async {
            let mut counter=10000;let mut tickets=20000;let mut delivered=false;
            let until=now(&client_cx).unwrap()+200_000;
            let failure=loop {
                assert!(now(&client_cx).unwrap()<until);
                announce(&mut state.host,&state.host_media,&state.observation,&host_cx,state.descriptor,false);
                let (left,right)=Box::pin(support::both(
                    state.host.drive(Duration::from_millis(1),||nonce(&mut counter),||{tickets+=1;Some(InputTicketId::from_raw(tickets))},block),
                    state.viewer.drive(Duration::from_millis(1), |_|{}, |route,bytes|{
                        if matches!(route,Route::Stream(r) if r.messages==quic::Messages::Exact(Kind::Progress as u16)) {
                            state.receiver.receive(Channel::MediaConfig,bytes,now(&client_cx).unwrap()).unwrap();
                            state.receiver.close();delivered=true;Ok(Disposition::Consumed)
                        }else{Ok(Disposition::Blocked)}
                    }),
                )).await;left.unwrap();
                if let Err(error)=right {break error;}
            };
            assert!(delivered);assert_eq!(failure,Error::View(presentation::Error::Media(fr_media::freshness::Error::StaleBinding)));
            assert_eq!(state.viewer.action(key(true)),Err(Error::Closed));
            state.host.close();
        },driver)).await;
        assert!(shutdown.handoff_safe());
        assert_eq!(state.effects.lock().unwrap().keys, [] as [bool; 0]);
    });
}

mod viewport;

mod streaming;

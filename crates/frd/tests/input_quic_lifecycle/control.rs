//! Real UDP/TLS, with only the native blocking gate and responsive peer simulated.
use super::*;
use fr_transport::quic::{ControlRoutes, Disposition};
use fr_wire::{
    authority::{self, Binding, MAX_AUTHORITY_BYTES, Message, Scope},
    stream::RecordStream,
};
use frd::{
    input_quic::control::{ControlRenewal, Error as RenewalError},
    media::{ObservationControl, renewal::ObservationRenewal},
};

async fn connections(cx: &Cx) -> (NativeQuicUdpConnection, QuicRecords, Routes, ControlRoutes) {
    let (p, h) = network::native_pair(cx, "localhost", ALPN).await;
    let (mut p, mut h) = (p.unwrap(), h.unwrap());
    let actions = StreamRoute {
        stream: p.connection_mut().open_uni_stream(cx).unwrap(),
        binding: 7,
        messages: Messages::InputActions,
        priority: Priority::Critical,
        outbound: false,
        maximum: MAX_INPUT_RECORD_BYTES,
    };
    let results = StreamRoute {
        stream: h.connection_mut().open_uni_stream(cx).unwrap(),
        binding: 7,
        messages: Messages::Exact(0x48),
        priority: Priority::Critical,
        outbound: true,
        maximum: INPUT_RESULT_BYTES,
    };
    let inbound = StreamRoute {
        stream: p.connection_mut().open_uni_stream(cx).unwrap(),
        binding: 11,
        messages: Messages::SessionControl,
        priority: Priority::Critical,
        outbound: false,
        maximum: 512,
    };
    let outbound = StreamRoute {
        stream: h.connection_mut().open_uni_stream(cx).unwrap(),
        outbound: true,
        ..inbound
    };
    p.connection_mut()
        .configure_stream_receive_window(cx, outbound.stream, 65536)
        .unwrap();
    let h = QuicRecords::new(
        h,
        cx,
        &[actions, results, inbound, outbound],
        &[],
        Policy::default(),
    )
    .unwrap();
    (
        p,
        h,
        Routes::new(actions, results, None).unwrap(),
        ControlRoutes { outbound, inbound },
    )
}
fn shared_grant(cx: &Cx) -> (InputSession, InputCredentials, ObservationControl) {
    let (_unused, c) = grant(cx);
    let now = host_now(cx).unwrap();
    let mut a = SessionAuthority::new(c.session, AuthorityPolicy::plan_defaults());
    a.mark_capabilities_checked().unwrap();
    a.authorize_observation(now).unwrap();
    a.mark_view_ready(now).unwrap();
    a.grant_lease(c.lease, now).unwrap();
    a.issue_input_ticket(c.lease, c.ticket, now).unwrap();
    let observation = ObservationControl::new(cx.clone(), a).unwrap();
    let input = observation
        .input_session(
            c,
            InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
            Capabilities::default().with(Capability::Keys),
        )
        .unwrap();
    (input, c, observation)
}
async fn io(
    cx: &Cx,
    peer: &mut NativeQuicUdpConnection,
    wire: &mut QuicRecords,
    control: &mut ControlRenewal,
) -> Result<(), RenewalError> {
    let ((), r) = Box::pin(network::both(
        async {
            peer.flush(cx).await.unwrap();
            peer.drive_io_once(cx, Duration::from_millis(1))
                .await
                .unwrap();
        },
        control.drive(wire, Duration::from_millis(1)),
    ))
    .await;
    r
}
fn echo_challenges(
    cx: &Cx,
    peer: &mut NativeQuicUdpConnection,
    routes: ControlRoutes,
    framing: &mut RecordStream,
) -> usize {
    let now = network::clock(cx);
    let mut b = peer
        .connection_mut()
        .read_stream(cx, routes.outbound.stream, 512)
        .unwrap();
    let mut count = 0;
    loop {
        let n = framing.push(&b, now).unwrap();
        b = b.slice(n..);
        if let Some(frame) = framing.frame(now).unwrap() {
            let Message::Challenge { scope, nonce, .. } = authority::decode(
                frame,
                Binding {
                    channel: 11,
                    session: RemoteSessionId::from_raw(1),
                },
                &ProtocolLimits::ABSOLUTE,
                InputDirection::HostToViewer,
                InputDelivery::Reliable,
            )
            .unwrap() else {
                panic!("challenge required")
            };
            let mut response = [0; MAX_AUTHORITY_BYTES];
            let n = authority::encode(
                Message::Response { scope, nonce },
                Binding {
                    channel: 11,
                    session: RemoteSessionId::from_raw(1),
                },
                &ProtocolLimits::ABSOLUTE,
                &mut response,
                InputDirection::ViewerToHost,
                InputDelivery::Reliable,
            )
            .unwrap();
            peer.connection_mut()
                .write_stream(
                    cx,
                    routes.inbound.stream,
                    Bytes::copy_from_slice(&response[..n]),
                    false,
                )
                .unwrap();
            if matches!(scope, Scope::Control(_)) {
                count += 1;
            }
            framing.consume(now).unwrap();
        }
        if b.is_empty() {
            return count;
        }
    }
}
fn blocked_network(in_submit: bool, terminal: Option<bool>) {
    let rt = network::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let (mut peer, mut wire, routes, control_routes) = connections(&cx).await;
        let (session, c, observation) = shared_grant(&cx);
        let (tx, rx) = mpsc::sync_channel(1);
        let effects = Arc::new(Effects::default());
        let native_effects = effects.clone();
        let seat = Seat::default();
        let (agent, driver) = seat
            .start(
                cx.clone(),
                session,
                Route::new(7, ProtocolLimits::ABSOLUTE),
                move || {
                    Ok(Gate {
                        state: native_effects,
                        resume: rx,
                        in_submit,
                        waited: false,
                    })
                },
                |_| true,
            )
            .unwrap();
        let mut input = QuicInput::new(cx.clone(), agent, &wire, routes).unwrap();
        let mut control = input
            .control_renewal(observation.clone(), &wire, control_routes)
            .unwrap();
        let mut observe =
            ObservationRenewal::new(observation, &wire, control_routes, ProtocolLimits::ABSOLUTE)
                .unwrap();
        peer.connection_mut()
            .write_stream(
                &cx,
                routes.actions().stream,
                Bytes::from(key_bytes(c)),
                false,
            )
            .unwrap();
        let (shutdown, ()) = Box::pin(network::both(driver, async {
            let end = Instant::now() + Duration::from_secs(1);
            while !effects.entered.load(Ordering::Acquire) {
                assert!(Instant::now() < end);
                io(&cx, &mut peer, &mut wire, &mut control).await.unwrap();
                input
                    .receive(
                        &mut wire,
                        || true,
                        |_| false,
                        |_, _| Ok(Disposition::Blocked),
                    )
                    .unwrap();
            }
            if let Some(reset) = terminal {
                close_control(
                    &cx,
                    &mut peer,
                    &mut wire,
                    &mut control,
                    control_routes,
                    reset,
                )
                .await;
            } else {
                maintain_blocked(
                    &cx,
                    &mut peer,
                    &mut wire,
                    &mut control,
                    &mut observe,
                    control_routes,
                    (&input, &seat),
                )
                .await;
            }
            assert!(input.control().is_stopped());
            assert!(seat.is_occupied());
            assert_eq!(effects.pressed.load(Ordering::Relaxed), 0);
            tx.send(()).unwrap();
            let end = Instant::now() + Duration::from_secs(1);
            while seat.is_occupied() || input.last_reply().is_none() {
                assert!(Instant::now() < end);
                let _ = input.service(&mut wire, || true);
                asupersync::time::sleep(cx.now(), Duration::from_millis(1)).await;
            }
            check_effects(&input, &effects, in_submit);
        }))
        .await;
        assert!(shutdown.handoff_safe());
    });
}
#[test]
fn renewal_crosses_initial_expiry_while_native_preparation_is_blocked() {
    blocked_network(false, None);
}
#[test]
fn renewal_crosses_initial_expiry_while_irreversible_call_is_blocked() {
    blocked_network(true, None);
}
#[test]
fn control_fin_fences_blocked_preparation_before_return() {
    blocked_network(false, Some(false));
}
#[test]
fn control_reset_preserves_late_irreversible_effect_and_cleanup() {
    blocked_network(true, Some(true));
}

async fn maintain_blocked(
    cx: &Cx,
    peer: &mut NativeQuicUdpConnection,
    wire: &mut QuicRecords,
    control: &mut ControlRenewal,
    observe: &mut ObservationRenewal,
    control_routes: ControlRoutes,
    owners: (&QuicInput, &Seat),
) {
    let mut framing = RecordStream::new(512, 11, 1_000_000).unwrap();
    let until = network::clock(cx) + 3_250_000;
    let mut nonce = 10;
    let mut replies = 0;
    while network::clock(cx) < until {
        observe
            .service(wire, || {
                nonce += 1;
                Ok(nonce)
            })
            .unwrap();
        control
            .service(wire, || {
                nonce += 1;
                Ok(nonce)
            })
            .unwrap();
        io(cx, peer, wire, control).await.unwrap();
        replies += echo_challenges(cx, peer, control_routes, &mut framing);
        observe
            .receive(wire, |_, _| Ok(Disposition::Blocked))
            .unwrap();
        control
            .receive(wire, |_, _| Ok(Disposition::Blocked))
            .unwrap();
        assert!(!owners.0.control().is_stopped());
        assert!(owners.0.status().outstanding && owners.1.is_occupied());
    }
    assert!(replies >= 4);
    assert!(control.renewed_until().unwrap().as_micros() > until);
    control.stop();
}
fn check_effects(input: &QuicInput, effects: &Effects, in_submit: bool) {
    let Some(InputReply::Record(r)) = input.last_reply() else {
        panic!("actual receipt required")
    };
    assert_eq!(
        r.outcome,
        if in_submit {
            InputOutcome::SubmittedToOs
        } else {
            InputOutcome::CancelledBeforeSubmission
        }
    );
    assert_eq!(
        effects.pressed.load(Ordering::Relaxed),
        usize::from(in_submit)
    );
    assert_eq!(
        effects.released.load(Ordering::Relaxed),
        usize::from(in_submit)
    );
}

async fn close_control(
    cx: &Cx,
    peer: &mut NativeQuicUdpConnection,
    wire: &mut QuicRecords,
    control: &mut ControlRenewal,
    control_routes: ControlRoutes,
    reset: bool,
) {
    if reset {
        peer.connection_mut()
            .reset_stream(cx, control_routes.inbound.stream, 7)
            .unwrap();
    } else {
        peer.connection_mut()
            .write_stream(cx, control_routes.inbound.stream, Bytes::new(), true)
            .unwrap();
    }
    let end = Instant::now() + Duration::from_millis(500);
    let error = loop {
        assert!(
            Instant::now() < end,
            "control FIN/RESET must be visible while native input is blocked"
        );
        if let Err(e) = io(cx, peer, wire, control).await {
            break e;
        }
    };
    assert_eq!(error, RenewalError::PeerClosed);
}

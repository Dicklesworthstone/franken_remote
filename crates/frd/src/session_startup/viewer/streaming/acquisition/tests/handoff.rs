//! Coalesced control records and cross-stream feedback use the real original
//! UDP/TLS routes. The early ticket uses codec-valid fixture bytes without an
//! issued grant; grant, renewal and revocation use the real broker and driver.
//! Native sink and presentation retain the parent module's documented fixtures.
use super::*;
use crate::input_watchdog::StopReason as HostStop;
use fr_transport::quic::{QuicRecords, StreamRoute};
use fr_wire::{
    authority::{self, Binding, Message, Scope},
    input::{InputDelivery, InputDirection},
    input_ticket::{self, Ticket},
    lease_revoked::{CleanupStage, EffectStage, Reason},
};

struct Requesting {
    stream: StreamingViewer,
    host: HostSession,
    broker: GrantBroker,
    seat: Seat,
    request: Request,
    feedback: Route,
    // Keep the original completed media owners alive through the handoff.
    media: crate::media_quic::NegotiatedMedia,
}
struct GrantedPair {
    stream: StreamingViewer,
    host: ControlledHost,
    driver: Driver,
    seat: Seat,
    request: Request,
    effects: Arc<Mutex<Vec<InputOperation>>>,
    media: crate::media_quic::NegotiatedMedia,
}
fn pending(
    stream: &mut StreamingViewer,
) -> (&mut ViewerSession, &crate::media_quic::NegotiatedMedia, &mut PendingControl) {
    let Peer::Acquiring { session, media, request } = &mut stream.peer else {
        panic!("the original request is no longer pending");
    };
    (session, media, request)
}
async fn raw_turn(c: &Cx, h: &Cx, host: &mut QuicRecords, viewer: &mut QuicRecords) {
    let (host, viewer) = Box::pin(support::both(
        host.drive(h, Duration::from_millis(1), || true),
        viewer.drive(c, Duration::from_millis(1), || true),
    ))
    .await;
    host.unwrap();
    viewer.unwrap();
}
async fn drain_host(c: &Cx, h: &Cx, host: &mut QuicRecords, viewer: &mut QuicRecords) {
    let until = now(h).unwrap() + 500_000;
    while host.usage().retained_send_records != 0 {
        assert!(now(h).unwrap() < until, "native send was not acknowledged");
        raw_turn(c, h, host, viewer).await;
    }
    assert_eq!(host.usage().retained_send_upper_bound, 0);
}
fn nonce(value: &mut u128) -> Result<u128, ()> {
    *value = value.checked_add(1).ok_or(())?;
    Ok(*value)
}
async fn poll_driver(driver: &mut Driver) {
    std::future::poll_fn(|task| {
        assert!(Pin::new(&mut *driver).poll(task).is_pending());
        Poll::Ready(())
    })
    .await;
}
async fn requesting(c: &Cx, h: &Cx) -> Requesting {
    let Fixture {
        mut stream, host, broker, channels, seat, request, feedback, media, ..
    } = Box::pin(fixture(c, h)).await;
    let Peer::Observe { session, media: viewer_media } =
        std::mem::replace(&mut stream.peer, Peer::Closed)
    else {
        panic!("fixture did not retain observation");
    };
    let mut control = PendingControl::new(
        &session, channels, request,
        Policy { view_age_us: 1_500_000, ..Policy::default() },
    ).unwrap();
    control.decoded(stream.initial.take().unwrap()).unwrap();
    stream.peer = Peer::Acquiring {
        session,
        media: viewer_media,
        request: Box::new(control),
    };
    let (session, _, control) = pending(&mut stream);
    control.service(session).unwrap();
    Requesting { stream, host, broker, seat, request, feedback, media }
}
async fn grant(c: &Cx, h: &Cx) -> GrantedPair {
    let mut f = Box::pin(requesting(c, h)).await;
    let mut n = 80_000;
    let until = now(h).unwrap() + 1_000_000;
    while f.broker.request().is_none() {
        assert!(now(h).unwrap() < until);
        let (session, _, request) = pending(&mut f.stream);
        request.service(session).unwrap();
        let (host, viewer) = Box::pin(support::both(
            f.host.drive(Duration::from_millis(1), || nonce(&mut n), block),
            session.transport.drive(c, Duration::from_millis(1), || true),
        ))
        .await;
        host.unwrap();
        viewer.unwrap();
        f.broker.receive(f.host.io().unwrap().0, block).unwrap();
    }
    let effects = Arc::new(Mutex::new(Vec::new()));
    let native_effects = effects.clone();
    let mut driver = f.broker.approve(
        f.host.io().unwrap().0,
        target(),
        || Some((InputLeaseId::from_raw(19), InputTicketId::from_raw(23))),
        move || Ok(Sink(native_effects)),
        |_| true,
    ).unwrap();
    loop {
        assert!(now(h).unwrap() < until);
        poll_driver(&mut driver).await;
        if f.broker.service(f.host.io().unwrap().0, Some(target())).unwrap() == Event::GrantQueued {
            break;
        }
        let (session, _, _) = pending(&mut f.stream);
        raw_turn(c, h, f.host.io().unwrap().0, &mut session.transport).await;
    }
    let input = f.broker.finish(f.host.io().unwrap().0, Some(target())).unwrap();
    GrantedPair {
        stream: f.stream,
        host: f.host.into_controlled(input).unwrap(),
        driver,
        seat: f.seat,
        request: f.request,
        effects,
        media: f.media,
    }
}
fn front(q: &mut QuicRecords, cx: &Cx, route: StreamRoute) -> Vec<u8> {
    let mut retained = None;
    q.receive_ready(
        cx,
        || true,
        |candidate| candidate == Route::Stream(route),
        |candidate, bytes| {
            assert_eq!(candidate, Route::Stream(route));
            assert!(retained.replace(bytes.to_vec()).is_none());
            Ok(Disposition::Blocked)
        },
    ).unwrap();
    retained.expect("record must remain in its original transport slot")
}
async fn pending_turn(stream: &mut StreamingViewer) -> Result<(), Error> {
    let (session, media, request) = pending(stream);
    request.drive(session, media, Duration::from_millis(1), |_, _| Err(())).await
}
async fn reap_granted(mut f: GrantedPair, cleanup: &Cx) {
    f.media.check(f.host.io().unwrap().0).unwrap();
    f.stream.close();
    f.host.close();
    assert!(f.driver.await.handoff_safe());
    assert!(!f.seat.is_occupied());
    f.stream.reap_media(
        cleanup,
        Deadline::after(cleanup, Duration::from_secs(1)).unwrap(),
    ).await.unwrap();
}
async fn stage_challenge(f: &mut GrantedPair, c: &Cx, h: &Cx) {
    let (session, _, _) = pending(&mut f.stream);
    drain_host(c, h, f.host.io().unwrap().0, &mut session.transport).await;
    let mut n = 90_000;
    let (host, viewer) = Box::pin(support::both(
        f.host.drive(
            Duration::from_millis(1), || nonce(&mut n),
            || Some(InputTicketId::from_raw(24)), block,
        ),
        session.transport.drive(c, Duration::from_millis(1), || true),
    )).await;
    host.unwrap();
    viewer.unwrap();
    drain_host(c, h, f.host.io().unwrap().0, &mut session.transport).await;
    assert!(n > 90_000, "the real control renewal must issue a challenge");
    assert!(f.host.control_renewed_until().is_none());
    // Both the grant and real challenge were natively ACKed while the pending
    // viewer has consumed neither, so later dispatch does not rely on timing.
    session.tick(block).unwrap();
    let before = front(&mut session.transport, c, session.routes.inbound);
    assert_eq!(&before[6..8], &(Kind::LeaseGranted as u16).to_be_bytes());
    assert!(pending(&mut f.stream).2.grant.is_none());
}
fn promote(f: &mut GrantedPair) {
    notify(
        &mut f.stream.peer, &f.stream.receiver, &mut f.stream.control, None,
        &mut |state, _| {
            let Dispatch::Existing(State::Requesting(request)) = state else {
                panic!("control was exposed before local acknowledgement");
            };
            request.confirm_mapping(f.request.parent, f.request.target.view).unwrap();
            request.visible(request.presentation().unwrap().frame.as_raw()).unwrap();
            Ok(())
        },
    ).unwrap();
    assert!(matches!(f.stream.peer, Peer::Control(_)));
}
async fn renew(f: &mut GrantedPair, c: &Cx, h: &Cx) {
    let until = now(h).unwrap() + 500_000;
    let mut n = 100_000;
    let mut ticket = 30_000;
    while f.host.control_renewed_until().is_none() {
        assert!(now(h).unwrap() < until);
        poll_driver(&mut f.driver).await;
        let (host, viewer) = Box::pin(support::both(
            f.host.drive(
                Duration::from_millis(1), || nonce(&mut n),
                || {
                    ticket += 1;
                    Some(InputTicketId::from_raw(ticket))
                },
                block,
            ),
            f.stream.peer.controlled().unwrap().drive(
                Duration::from_millis(1), |_| panic!("no input was submitted"),
                |_, _| Err(()),
            ),
        )).await;
        host.unwrap();
        viewer.unwrap();
    }
    assert!(c.checkpoint().is_ok());
}
#[test]
fn acknowledged_grant_and_immediate_challenge_wait_for_the_original_control_owner() {
    run(|c, h| async move {
        let mut f = Box::pin(grant(&c, &h)).await;
        let connection = f.stream.peer.parts().unwrap().0.transport.binding();
        Box::pin(stage_challenge(&mut f, &c, &h)).await;
        pending_turn(&mut f.stream).await.unwrap();
        let (session, _, request) = pending(&mut f.stream);
        assert!(request.grant.is_some());
        let challenge = front(&mut session.transport, &c, session.routes.inbound);
        assert!(matches!(
            authority::decode(
                &challenge,
                Binding { channel: f.request.parent.id, session: f.request.parent.remote_session },
                &request.channels.limits(), InputDirection::HostToViewer, InputDelivery::Reliable,
            ).unwrap(),
            Message::Challenge { scope: Scope::Control(lease), .. }
                if lease == InputLeaseId::from_raw(19)
        ));
        pending_turn(&mut f.stream).await.unwrap();
        let (session, _, request) = pending(&mut f.stream);
        assert_eq!(front(&mut session.transport, &c, session.routes.inbound), challenge);
        assert!(!request.local.mapped);
        assert_eq!(*f.effects.lock().unwrap(), [] as [InputOperation; 0]);
        promote(&mut f);
        Box::pin(renew(&mut f, &c, &h)).await;
        assert!(f.stream.peer.parts().unwrap().0.transport.is_bound_to(&connection));
        assert_eq!(*f.effects.lock().unwrap(), [] as [InputOperation; 0]);
        Box::pin(reap_granted(f, &h)).await;
    });
}
#[test]
fn input_feedback_before_the_grant_is_retained_without_inventing_control() {
    run(|c, h| async move {
        let mut f = Box::pin(requesting(&c, &h)).await;
        let Route::Stream(feedback) = f.feedback else { panic!("feedback must be reliable") };
        let (session, _, _) = pending(&mut f.stream);
        drain_host(&c, &h, f.host.io().unwrap().0, &mut session.transport).await;
        let at = now(&h).unwrap();
        let ticket = Ticket {
            credentials: InputCredentials {
                session: f.request.parent.remote_session,
                lease: InputLeaseId::from_raw(19),
                ticket: InputTicketId::from_raw(24),
                view: f.request.target.view,
            },
            sequence: 1, issued_at_us: at, expires_at_us: at + 500_000,
        };
        let mut bytes = [0; input_ticket::INPUT_TICKET_BYTES];
        let count = input_ticket::encode(
            ticket, &mut bytes, &f.host.selection().limits, feedback.binding,
            InputDirection::HostToViewer, InputDelivery::Reliable,
        ).unwrap();
        f.host.io().unwrap().0.send(&h, f.feedback, &bytes[..count], at + 500_000, || true).unwrap();
        let (session, _, request) = pending(&mut f.stream);
        let original_deadline = request.deadline();
        let (_, inbound, _) = request.channels.viewer_routes(&session.transport).unwrap();
        drain_host(&c, &h, f.host.io().unwrap().0, &mut session.transport).await;
        for _ in 0..2 {
            pending_turn(&mut f.stream).await.unwrap();
            let (session, _, request) = pending(&mut f.stream);
            assert!(request.grant.is_none());
            assert_eq!(request.deadline(), original_deadline);
            assert_eq!(front(&mut session.transport, &c, inbound), bytes[..count]);
        }
        assert!(!f.seat.is_occupied());
        f.stream.close();
        f.host.close();
        drop(f.broker);
        f.stream.reap_media(&h, Deadline::after(&h, Duration::from_secs(1)).unwrap()).await.unwrap();
    });
}
#[test]
fn a_real_host_revocation_ends_the_received_grant_before_mapping_or_visibility() {
    run(|c, h| async move {
        let mut f = Box::pin(grant(&c, &h)).await;
        let (session, _, _) = pending(&mut f.stream);
        drain_host(&c, &h, f.host.io().unwrap().0, &mut session.transport).await;
        pending_turn(&mut f.stream).await.unwrap();
        let control = f.host.control();
        let ((delivery, report), shutdown) = Box::pin(support::both(
            support::both(
                f.host.revoke_and_close(HostStop::LocalRevoke),
                async {
                    loop {
                        match pending_turn(&mut f.stream).await {
                            Err(Error::Control(ControlError::LeaseRevoked(report))) => break report,
                            result => result.unwrap(),
                        }
                    }
                },
            ),
            f.driver,
        )).await;
        assert!(matches!(delivery, Ok(()) | Err(quic::Error::Expired)));
        assert_eq!(report.lease, InputLeaseId::from_raw(19));
        assert_eq!(report.reason, Reason::LocalRevoke);
        assert_eq!(report.cleanup, CleanupStage::Fenced);
        assert_eq!(report.effects, EffectStage::Unknown);
        assert!(control.is_stopped());
        let (_, _, request) = pending(&mut f.stream);
        assert_eq!(
            request.grant.as_ref().unwrap().1.stopped(),
            Some(fr_client::input::StopReason::Disconnected)
        );
        assert!(!request.local.mapped);
        assert_eq!(*f.effects.lock().unwrap(), [] as [InputOperation; 0]);
        assert!(shutdown.handoff_safe());
        assert!(!f.seat.is_occupied());
        f.stream.reap_media(&h, Deadline::after(&h, Duration::from_secs(1)).unwrap()).await.unwrap();
    });
}

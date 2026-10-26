//! Actual pending viewer dispatch on the original TLS control route. The host
//! terminal and late-grant bytes below are explicit codec-valid fixtures, not
//! evidence that a native input lease was created. The host GrantServices test
//! separately exercises the real queued-grant reporting boundary.
use super::*;
use fr_core::limits::ProtocolLimits;
use fr_wire::lease_revoked::{self, Revoked};

fn report() -> Revoked {
    Revoked {
        lease: InputLeaseId::from_raw(19),
        reason: Reason::HostFailure,
        cleanup: CleanupStage::Fenced,
        effects: EffectStage::Unknown,
    }
}
fn report_bytes(request: Request, limits: &ProtocolLimits) -> [u8; lease_revoked::REVOKED_BYTES] {
    let mut bytes = [0; lease_revoked::REVOKED_BYTES];
    let length = lease_revoked::encode(
        report(),
        Binding {
            channel: request.parent.id,
            session: request.parent.remote_session,
        },
        limits,
        &mut bytes,
        InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
    .unwrap();
    assert_eq!(length, bytes.len());
    bytes
}
fn late_grant_bytes(
    request: Request,
    input_channel: u32,
    at: u64,
    limits: &ProtocolLimits,
) -> [u8; control::GRANTED_BYTES] {
    let mut bytes = [0; control::GRANTED_BYTES];
    let length = control::encode_granted(
        Granted {
            request,
            input_channel,
            lease: report().lease,
            ticket: InputTicketId::from_raw(23),
            issued_at_us: at,
            lease_until_us: at + 3_000_000,
            ticket_until_us: at + 500_000,
            first_action: 0,
            first_pointer: 0,
        },
        &mut bytes,
        limits,
        InputDirection::HostToViewer,
        InputDelivery::Reliable,
    )
    .unwrap();
    assert_eq!(length, bytes.len());
    bytes
}
fn assert_no_owner_or_revival(stream: &mut StreamingViewer, until: ClientInstant) {
    assert!(stream.control.input.is_none());
    let (_, _, request) = pending(stream);
    assert!(request.terminal);
    assert!(request.grant.is_none());
    assert!(!request.local.mapped);
    assert_eq!(request.deadline(), until);
    assert_eq!(
        request.pending.pending(until),
        Err(fr_client::control_grant::Error::Stopped)
    );
    assert_eq!(request.current(), Err(Error::Closed));
    assert_eq!(
        request.confirm_mapping(request.request.parent, request.request.target.view),
        Err(Error::Closed)
    );
    assert_eq!(request.visible(0), Err(Error::Closed));
    assert_eq!(
        notify(
            &mut stream.peer,
            &stream.receiver,
            &mut stream.control,
            None,
            &mut |_, _| panic!("terminal request reached the UI or input promotion"),
        ),
        Err(Error::Closed)
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn serving_a_request_preserves_pregrant_revocation_and_refuses_the_following_grant() {
    run(|c, h| async move {
        let Fixture {
            mut stream,
            mut host,
            mut broker,
            channels,
            media,
            seat,
            request,
            ..
        } = Box::pin(fixture_with_transport(
            &c,
            &h,
            quic::Policy {
                // Only this fixture needs both ordered records admitted before
                // dispatch. The existing fixtures keep their one-slot budget.
                critical_send_records: 2,
                ..quic::Policy::default()
            },
        ))
        .await;
        media.check(host.io().unwrap().0).unwrap();
        let input_channel = channels.channel_binding();
        let limits = channels.limits();
        let clock = stream
            .peer
            .parts()
            .unwrap()
            .0
            .clock_correlation()
            .unwrap()
            .unwrap();
        let original_deadline = Cell::new(None);
        let done = Cell::new(false);
        let bytes = report_bytes(request, &limits);
        let (result, late_grant) = Box::pin(support::both(
            async {
                let result = stream
                    .serve_requesting_control(
                        channels,
                        request,
                        Policy::default(),
                        |state, _| {
                            let State::Requesting(pending) = state else {
                                panic!("no grant or input owner was received")
                            };
                            assert!(pending.grant.is_none());
                            if let Some(until) = original_deadline.get() {
                                assert_eq!(pending.deadline(), until);
                            } else {
                                original_deadline.set(Some(pending.deadline()));
                            }
                            Ok(())
                        },
                        |_| panic!("terminal metadata cannot create input receipts"),
                        |_, _| Err(()),
                    )
                    .await;
                done.set(true);
                result
            },
            async {
                let mut nonce_value = 70_000;
                let until = now(&h).unwrap() + 500_000;
                let mut late_grant = None;
                while !done.get() {
                    assert!(now(&h).unwrap() < until, "pending terminal did not arrive");
                    let (q, routes) = host.io().unwrap();
                    broker.receive(q, block).unwrap();
                    if late_grant.is_none()
                        && broker.request() == Some(request)
                        && q.usage().critical_send_records == 0
                    {
                        let grant =
                            late_grant_bytes(request, input_channel, now(&h).unwrap(), &limits);
                        // Same original reliable stream, deliberately ordered:
                        // the terminal must stop dispatch before this late grant.
                        for record in [bytes.as_slice(), grant.as_slice()] {
                            q.send(&h, Route::Stream(routes.outbound), record, until, || true)
                                .unwrap();
                        }
                        late_grant = Some(grant);
                    }
                    if host
                        .drive(Duration::from_millis(1), || nonce(&mut nonce_value), block)
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                late_grant
            },
        ))
        .await;
        assert_eq!(
            result,
            Err(Error::Control(ControlError::LeaseRevoked(report())))
        );
        let until = original_deadline
            .get()
            .expect("original request was exposed");
        let late_grant = late_grant.expect("host received this original explicit request");
        assert_no_owner_or_revival(&mut stream, until);
        let (session, _, pending) = pending(&mut stream);
        let inbound = Route::Stream(session.routes.inbound);
        assert_eq!(pending.revoked(inbound, inbound, &bytes), Error::Closed);
        assert_eq!(
            pending
                .pending
                .accept(&late_grant, clock, pending.policy, until)
                .err(),
            Some(fr_client::control_grant::Error::Stopped)
        );
        assert!(pending.grant.is_none());
        assert_eq!(pending.deadline(), until);
        assert!(broker.native_status().is_none());
        assert!(!seat.is_occupied());
        host.close();
        drop(broker);
        stream
            .reap_media(&h, Deadline::after(&h, Duration::from_secs(1)).unwrap())
            .await
            .unwrap();
    });
}

#[test]
fn wrong_route_stale_binding_zero_lease_and_malformed_terminal_cannot_create_authority() {
    for case in 0..6 {
        run(move |c, h| async move {
            let mut f = Box::pin(requesting(&c, &h)).await;
            let (session, _, request) = pending(&mut f.stream);
            let inbound = Route::Stream(session.routes.inbound);
            let mut route = inbound;
            let mut bytes = report_bytes(f.request, &request.channels.limits());
            let expected = match case {
                0 => {
                    route = Route::Stream(
                        request
                            .channels
                            .viewer_routes(&session.transport)
                            .unwrap()
                            .1,
                    );
                    Error::Control(ControlError::WrongBinding)
                }
                1 | 2 => {
                    bytes[if case == 1 { 19 } else { 39 }] ^= 1;
                    input_error(fr_client::input::Error::Wire(WireError::InvalidBinding))
                }
                3 => {
                    bytes[40..56].fill(0);
                    input_error(fr_client::input::Error::Wire(WireError::InvalidBinding))
                }
                4 => {
                    bytes[57] = 0;
                    input_error(fr_client::input::Error::Wire(WireError::InvalidValue))
                }
                5 => {
                    bytes[8] = 1;
                    input_error(fr_client::input::Error::Wire(WireError::InvalidFlags))
                }
                _ => unreachable!(),
            };
            let until = request.deadline();
            assert_eq!(request.revoked(route, inbound, &bytes), expected);
            assert_no_owner_or_revival(&mut f.stream, until);
            let (_, _, request) = pending(&mut f.stream);
            assert_eq!(request.revoked(inbound, inbound, &bytes), Error::Closed);
            assert!(!f.seat.is_occupied());
            f.stream.close();
            f.host.close();
            drop(f.broker);
            f.stream
                .reap_media(&h, Deadline::after(&h, Duration::from_secs(1)).unwrap())
                .await
                .unwrap();
        });
    }
}

#[test]
fn an_existing_grant_still_requires_its_exact_lease_and_fences_its_original_input() {
    run(|c, h| async move {
        let mut f = Box::pin(grant(&c, &h)).await;
        let (session, _, _) = pending(&mut f.stream);
        drain_host(&c, &h, f.host.io().unwrap().0, &mut session.transport).await;
        pending_turn(&mut f.stream).await.unwrap();
        let (session, _, request) = pending(&mut f.stream);
        let expected_lease = request.grant.as_ref().unwrap().0.lease;
        assert_eq!(expected_lease, report().lease);
        let mut bytes = report_bytes(f.request, &request.channels.limits());
        bytes[55] ^= 1;
        let inbound = Route::Stream(session.routes.inbound);
        let until = request.deadline();
        assert_eq!(
            request.revoked(inbound, inbound, &bytes),
            input_error(fr_client::input::Error::Wire(WireError::InvalidBinding))
        );
        let (granted, input) = request.grant.as_ref().unwrap();
        assert_eq!(granted.lease, expected_lease);
        assert_eq!(
            input.stopped(),
            Some(fr_client::input::StopReason::Disconnected)
        );
        assert_eq!(request.deadline(), until);
        assert_eq!(request.current(), Err(Error::Closed));
        assert!(request.terminal);
        assert!(!request.local.mapped);
        assert_eq!(*f.effects.lock().unwrap(), [] as [InputOperation; 0]);
        Box::pin(reap_granted(f, &h)).await;
    });
}

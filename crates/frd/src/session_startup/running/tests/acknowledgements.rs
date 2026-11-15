//! Real UDP/TLS ACK evidence across a local scheduling pause. Identity remains
//! the explicit local fixture; these tests do not qualify native input or a tailnet.
use super::*;

async fn quiet_pair(c: &Cx, h: &Cx) -> (HostSession, ViewerSession) {
    let (mut host, mut viewer) = pair(c, h).await;
    let mut n = 0;
    let until = now(h).unwrap() + 2_000_000;
    loop {
        assert!(now(h).unwrap() < until, "initial exchange did not drain");
        let (a, b) = Box::pin(support::both(
            host.drive(Duration::from_millis(1), || nonce(&mut n), no_other),
            viewer.drive(Duration::from_millis(1), no_other),
        ))
        .await;
        a.unwrap();
        b.unwrap();
        if host.renewed_until().is_some()
            && host.opened.transport.usage().retained_send_records == 0
            && viewer.io().unwrap().0.usage().retained_send_records == 0
        {
            return (host, viewer);
        }
    }
}

fn host_record(host: &HostSession, issued: u64) -> Vec<u8> {
    let mut bytes = [0; authority::MAX_AUTHORITY_BYTES];
    let len = authority::encode(
        Message::Challenge {
            scope: Scope::Control(InputLeaseId::from_raw(19)),
            nonce: 77,
            deadline_micros: issued + 100_000,
        },
        Binding {
            channel: host.opened.routes.outbound.binding,
            session: host.binding().remote_session,
        },
        &ProtocolLimits::ABSOLUTE,
        &mut bytes,
        D::HostToViewer,
        T::Reliable,
    )
    .unwrap();
    bytes[..len].to_vec()
}

async fn park_sender(
    cx: &Cx,
    sender: &mut QuicRecords,
    peer: &mut QuicRecords,
    route: Route,
    bytes: &[u8],
) {
    let issued = now(cx).unwrap();
    sender
        .send(cx, route, bytes, issued + 100_000, || true)
        .unwrap();
    sender.drive(cx, Duration::ZERO, || true).await.unwrap();
    // Only the receiving peer progresses. Its real ACK waits in the sender's
    // socket while the sender's task is not scheduled.
    let mut received = 0;
    let receive_until = now(cx).unwrap() + 100_000;
    while now(cx).unwrap() < receive_until {
        peer.drive(cx, Duration::from_millis(1), || true)
            .await
            .unwrap();
        peer.receive(
            cx,
            || true,
            |_, record| {
                assert_eq!(record, bytes);
                received += 1;
                Ok(Disposition::Consumed)
            },
        )
        .unwrap();
    }
    assert_eq!(received, 1);
    assert_eq!(sender.usage().retained_send_records, 1);
    // Use the public maximum rather than guessing the admitted path allowance.
    // The original three-second observation/silence limits remain unchanged.
    asupersync::time::sleep(
        cx.now(),
        Duration::from_micros(quic::MAX_DELIVERY_ALLOWANCE_US + 100_000),
    )
    .await;
    assert!(now(cx).unwrap() >= issued + quic::MAX_DELIVERY_ALLOWANCE_US);
}

async fn parked_host(c: &Cx, h: &Cx) -> (HostSession, ViewerSession) {
    let (mut host, mut viewer) = quiet_pair(c, h).await;
    let bytes = host_record(&host, now(h).unwrap());
    let route = Route::Stream(host.opened.routes.outbound);
    park_sender(
        h,
        &mut host.opened.transport,
        viewer.io().unwrap().0,
        route,
        &bytes,
    )
    .await;
    assert!(host.opened.control.check().is_ok());
    (host, viewer)
}

#[test]
fn host_reconciles_a_timely_peer_ack_before_ordinary_synchronous_maintenance() {
    run(|c, h| async move {
        let (mut host, mut viewer) = parked_host(&c, &h).await;
        let binding = host.binding();
        let connection = host.opened.transport.binding();
        let mut n = 0;
        host.drive(Duration::ZERO, || nonce(&mut n), no_other)
            .await
            .unwrap();
        assert_eq!(host.binding(), binding);
        assert!(host.opened.transport.is_bound_to(&connection));
        assert!(host.check().is_ok());
        let renewed = host.renewed_until().unwrap();
        while host.renewed_until() == Some(renewed) {
            let (a, b) = Box::pin(support::both(
                host.drive(Duration::from_millis(1), || nonce(&mut n), no_other),
                viewer.drive(Duration::from_millis(1), no_other),
            ))
            .await;
            a.unwrap();
            b.unwrap();
        }
    });
}

#[test]
fn admission_refresh_reconciles_a_timely_peer_ack_before_synchronous_maintenance() {
    run(|c, h| async move {
        let (mut host, _viewer) = parked_host(&c, &h).await;
        let control = host.observation().unwrap();
        let mut n = 0;
        pump_refresh(
            &mut host.renewal,
            &mut host.opened.transport,
            async { Ok(()) },
            RefreshTurn {
                cx: &h,
                control: &control,
                until: now(&h).unwrap() + 500_000,
                wait: Duration::ZERO,
            },
            &mut || nonce(&mut n),
            &mut no_other,
        )
        .await
        .unwrap();
        assert!(host.check().is_ok());
    });
}

#[test]
fn host_revoke_precedes_a_pending_ack_reconciliation() {
    run(|c, h| async move {
        let (mut host, _viewer) = parked_host(&c, &h).await;
        let control = host.observation().unwrap();
        control.revoke();
        assert_eq!(
            host.drive(
                Duration::ZERO,
                || panic!("revoked host issued a nonce"),
                no_other
            )
            .await,
            Err(Error::Cancelled)
        );
        assert!(host.opened.transport.is_closed());
        assert!(control.check().is_err());
    });
}

#[test]
fn viewer_reconciles_a_timely_peer_ack_before_ordinary_synchronous_maintenance() {
    run(|c, h| async move {
        let (mut host, mut viewer) = quiet_pair(&c, &h).await;
        let (_, routes) = viewer.io().unwrap();
        let bytes = control_reply(host.binding().remote_session, routes.outbound.binding);
        let connection = viewer.io().unwrap().0.binding();
        park_sender(
            &c,
            viewer.io().unwrap().0,
            &mut host.opened.transport,
            Route::Stream(routes.outbound),
            &bytes,
        )
        .await;
        viewer.drive(Duration::ZERO, no_other).await.unwrap();
        assert!(viewer.io().unwrap().0.is_bound_to(&connection));
        assert_eq!(viewer.io().unwrap().0.usage().retained_send_records, 0);
    });
}

#[test]
fn reconciliation_without_overdue_sends_preserves_application_maintenance_order() {
    run(|c, h| async move {
        let (mut host, mut viewer) = quiet_pair(&c, &h).await;
        host.renewal
            .reconcile_checked(&mut host.opened.transport, || {
                panic!("host maintenance must precede ordinary application permission")
            })
            .await
            .unwrap();
        viewer
            .reconcile_acknowledgements(|| {
                panic!("viewer maintenance must precede ordinary application permission")
            })
            .await
            .unwrap();
        assert!(host.check().is_ok());
        assert!(viewer.check().is_ok());
    });
}

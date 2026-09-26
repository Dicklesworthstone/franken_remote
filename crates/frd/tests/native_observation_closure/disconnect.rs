//! The public client API reaches the ordinary protected-host `CloseRequest`
//! dispatcher and its deferred Closed reporter; no raw client transport loan.
use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Report,
    MissingReport,
    Abandon,
    Cancel,
    ControlIntent,
}
async fn client_open(cx: &Cx, mode: Mode) -> frd::session_startup::ViewerSession {
    let native = fixture::client(cx, address()).await;
    let mut offer = fixture::offer();
    if mode == Mode::ControlIntent {
        offer.role = Role::RequestControl;
    }
    let mut opening = Viewer::new(
        cx.clone(),
        native,
        offer,
        quic::Policy::default(),
        Duration::from_secs(3),
    )
    .unwrap();
    while !opening.is_complete() {
        opening.drive(Duration::from_millis(1)).await.unwrap();
    }
    let mut viewer = opening.finish().unwrap();
    // ACK original startup before a terminal exchange; never drain application
    // writes under the terminal permit or pretend that packet count is credit.
    for _ in 0..24 {
        viewer
            .drive(Duration::from_millis(1), |_, _| {
                panic!("unexpected application record before close")
            })
            .await
            .unwrap();
    }
    viewer
}
async fn disconnect_scenario(broker: Cx, host_cx: Cx, client_cx: Cx, mode: Mode) {
    let tools = Tools::new();
    let api = fixture::Api::new();
    let identity = api.identity(&broker).await;
    let mut server = Server::new(api.client.clone(), identity.clone())
        .bind_linux(
            &broker,
            tools.configuration(),
            native_accept::Configuration::default(),
        )
        .await
        .unwrap();
    let stop = Cell::new(false);
    let observed: Mutex<Option<ObservationControl>> = Mutex::new(None);
    let mut request = fixture::request();
    let ending = match mode {
        Mode::Report => Ending::Requested,
        Mode::MissingReport => Ending::SecurityLoss,
        Mode::ControlIntent => {
            request.session.offer.role = Role::RequestControl;
            Ending::ControlIntent
        }
        Mode::Abandon | Mode::Cancel => Ending::HostStop,
    };
    let serving = server.run(&host_cx, request, |host| {
        Box::pin(host_service(host, &stop, &observed, &identity, ending))
    });
    let client = disconnect_client(&client_cx, &stop, &observed, mode);
    let (result, outcome) = Box::pin(network::both(serving, client)).await;
    let status = server.observation_closure();
    if matches!(
        mode,
        Mode::Report | Mode::MissingReport | Mode::ControlIntent
    ) {
        let reports = outcome
            .and_then(|o| o.report)
            .into_iter()
            .collect::<Vec<_>>();
        verify(result, &reports, status, ending);
    } else {
        assert_eq!(result.unwrap(), Ok(()));
        assert!(status.unwrap().delivery.is_err());
    }
    assert!(host_cx.is_cancel_requested());
    assert!(client_cx.is_cancel_requested());
    assert!(!broker.is_cancel_requested());
    server.stop(&broker).await.unwrap();
}

async fn disconnect_client(
    client_cx: &Cx,
    stop: &Cell<bool>,
    observed: &Mutex<Option<ObservationControl>>,
    mode: Mode,
) -> Option<quic::CloseOutcome> {
    let mut viewer = Box::pin(client_open(client_cx, mode)).await;
    let control = viewer.control();
    assert!(!control.is_stopped());
    let pending = Box::pin(viewer.disconnect(Reason::ClientStopping));
    let outcome = match mode {
        Mode::Abandon => {
            drop(pending); // Unpolled future still completes local teardown.
            stop.set(true);
            None
        }
        Mode::Cancel => {
            control.stop(); // Same original context, not a new cleanup clock.
            let outcome = pending.await.unwrap();
            assert_eq!(outcome.transport, Err(quic::Error::Cancelled));
            assert_eq!(outcome.report, None);
            assert!(!outcome.request_acknowledged);
            stop.set(true);
            Some(outcome)
        }
        Mode::ControlIntent => {
            assert_eq!(pending.await, Err(SessionError::Order));
            stop.set(true);
            None
        }
        Mode::Report | Mode::MissingReport => Some(pending.await.unwrap()),
    };
    assert!(viewer.is_closed());
    assert!(control.is_stopped());
    assert!(viewer.io().is_err(), "terminal exchange cannot restore I/O");
    if mode == Mode::Report {
        let report = Closed {
            reason: ClosedReason::ClientRequested,
            cleanup: Cleanup::Unconfirmed,
            effects: OutstandingEffects::Unknown,
        };
        assert_eq!(outcome.unwrap().report, Some(report));
        assert_eq!(outcome.unwrap().transport, Ok(()));
        assert_eq!(viewer.closed_report(), Some(report));
        assert!(observed.lock().unwrap().as_ref().unwrap().check().is_err());
        // The exact report survives another local close and a rejected retry.
        viewer.close();
        assert_eq!(
            Box::pin(viewer.disconnect(Reason::Requested)).await,
            Err(SessionError::RemoteClosed(report))
        );
        assert_eq!(viewer.closed_report(), Some(report));
    } else {
        assert_eq!(viewer.closed_report(), None);
        if mode == Mode::MissingReport {
            let outcome = outcome.unwrap();
            assert_eq!(outcome.report, None);
            assert!(outcome.transport.is_err());
        }
    }
    outcome
}

#[test]
#[ignore = "requires isolated synthetic ingress fixture via test_linux_serial_lifecycle.sh"]
fn client_disconnect_reaches_automatic_host_report_and_retains_exact_uncertainty() {
    run(async |b, h, c| Box::pin(disconnect_scenario(b, h, c, Mode::Report)).await);
}
#[test]
#[ignore = "requires isolated synthetic ingress fixture via test_linux_serial_lifecycle.sh"]
fn client_disconnect_keeps_absent_report_unknown_after_host_security_loss() {
    run(async |b, h, c| Box::pin(disconnect_scenario(b, h, c, Mode::MissingReport)).await);
}
#[test]
#[ignore = "requires isolated synthetic ingress fixture via test_linux_serial_lifecycle.sh"]
fn abandoned_client_disconnect_still_stops_the_original_local_session() {
    run(async |b, h, c| Box::pin(disconnect_scenario(b, h, c, Mode::Abandon)).await);
}
#[test]
#[ignore = "requires isolated synthetic ingress fixture via test_linux_serial_lifecycle.sh"]
fn original_stop_handle_can_cancel_the_terminal_client_exchange() {
    run(async |b, h, c| Box::pin(disconnect_scenario(b, h, c, Mode::Cancel)).await);
}
#[test]
#[ignore = "requires isolated synthetic ingress fixture via test_linux_serial_lifecycle.sh"]
fn control_intent_cannot_bypass_its_native_input_cleanup_using_disconnect() {
    run(async |b, h, c| Box::pin(disconnect_scenario(b, h, c, Mode::ControlIntent)).await);
}

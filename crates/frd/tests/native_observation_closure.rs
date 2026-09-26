#![cfg(target_os = "linux")]
//! Real TLS/UDP, root-credential `LocalAPI` and protected-listener owner paths.
//! Metadata, approval, interface and firewall evidence are explicit fixtures.
//! Execute only through `scripts/test_linux_serial_lifecycle.sh` in its fresh
//! mount/user/network namespace; these tests do not qualify installed Tailscale.
#[allow(dead_code)]
#[path = "native_host_accept/fixture.rs"]
mod fixture;
#[allow(dead_code)]
#[path = "../../fr-transport/tests/support/mod.rs"]
mod network;
use asupersync::{cx::Cx, time::timeout, types::Budget};
use fr_tailnet::{LocalApi, ingress};
use fr_transport::{
    native_accept::{self, Listener},
    quic::{self, Disposition, Route},
};
use fr_wire::{
    authority::Binding,
    closure::{self, Cleanup, CloseRequest, Closed, ClosedReason, OutstandingEffects, Reason},
    input::{InputDelivery, InputDirection},
    negotiation::Role,
};
use frd::{
    media::{ObservationControl, renewal},
    native_connection::host::{IngressCheck, Request, Server},
    session_startup::{Error as SessionError, Viewer},
};
use std::{
    cell::Cell,
    fs,
    net::SocketAddr,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

struct Tools(PathBuf);
impl Tools {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        assert_eq!(
            fs::read_to_string("/run/fr-synthetic-ingress").unwrap(),
            "synthetic-only\n"
        );
        let dir = PathBuf::from(format!(
            "/run/fr-closure-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        for name in ["ip", "nft"] {
            let path = dir.join(name);
            fs::write(&path, include_str!("native_host_linux_serial/tools.py")).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        Self(dir)
    }
    fn configuration(&self) -> ingress::Configuration {
        ingress::Configuration::new(address(), "fr-fixture")
            .unwrap()
            .executables(&self.0.join("nft"), &self.0.join("ip"))
            .unwrap()
    }
}
fn address() -> SocketAddr {
    "100.64.0.1:4795".parse().unwrap()
}
fn now(cx: &Cx) -> u64 {
    frd::media::host_now(cx).unwrap().as_micros()
}
fn run(body: impl AsyncFnOnce(Cx, Cx, Cx)) {
    let runtime = network::runtime();
    let broker = runtime.request_cx_with_budget(Budget::INFINITE);
    let session = runtime.request_cx_with_budget(Budget::INFINITE);
    let client = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(Box::pin(async {
        timeout(
            client.now(),
            Duration::from_secs(8),
            body(broker, session, client),
        )
        .await
        .unwrap();
    }));
}
#[derive(Clone, Copy, PartialEq)]
enum Ending {
    Requested,
    HostStop,
    Malformed,
    SecurityLoss,
    ControlIntent,
}
async fn scenario(broker: Cx, host_cx: Cx, client_cx: Cx, ending: Ending) {
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
    let done = Cell::new(false);
    let observed: Mutex<Option<ObservationControl>> = Mutex::new(None);
    let mut request = fixture::request();
    if ending == Ending::ControlIntent {
        request.session.offer.role = Role::RequestControl;
    }
    let serving = server.run(&host_cx, request, |host| {
        Box::pin(host_service(host, &stop, &observed, &identity, ending))
    });
    let host = async {
        let result = serving.await;
        done.set(true);
        result
    };
    let client = Box::pin(client_service(&client_cx, &stop, &done, &observed, ending));
    let (result, reports) = Box::pin(network::both(host, client)).await;
    verify(result, &reports, server.observation_closure(), ending);
    assert!(host_cx.is_cancel_requested());
    assert!(!broker.is_cancel_requested());
    server.stop(&broker).await.unwrap();
}

async fn host_service(
    host: frd::session_startup::Host,
    stop: &Cell<bool>,
    observed: &Mutex<Option<ObservationControl>>,
    identity: &fr_tailnet::NativeServerIdentity,
    ending: Ending,
) -> Result<(), SessionError> {
    let mut session = host
        .open(Duration::from_millis(1), |approval, _| {
            approval.decide(true).unwrap();
            Ok(())
        })
        .await
        .unwrap();
    *observed.lock().unwrap() = Some(session.observation().unwrap());
    let mut nonce = 700_u128;
    let result = loop {
        if stop.get() {
            session.close();
            break Ok(());
        }
        let result = session
            .drive(
                Duration::from_millis(1),
                &mut || {
                    nonce += 1;
                    Ok(nonce)
                },
                |_, _| panic!("terminal request escaped the original dispatcher"),
            )
            .await;
        if let Err(error) = result {
            break Err(error);
        }
    };
    assert!(session.observation().is_err());
    if ending == Ending::SecurityLoss {
        identity.stop();
    }
    result
}
async fn client_service(
    client_cx: &Cx,
    stop: &Cell<bool>,
    done: &Cell<bool>,
    observed: &Mutex<Option<ObservationControl>>,
    ending: Ending,
) -> Vec<Closed> {
    let native = fixture::client(client_cx, address()).await;
    let mut offer = fixture::offer();
    if ending == Ending::ControlIntent {
        offer.role = Role::RequestControl;
    }
    let mut opening = Viewer::new(
        client_cx.clone(),
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
    // Let ordinary startup/renewal records be ACKed before testing the
    // no-native-backlog terminal path. No production credit is manufactured.
    for _ in 0..24 {
        viewer
            .drive(Duration::from_millis(1), |_, _| Err(()))
            .await
            .unwrap();
    }
    let parent = viewer.metadata().binding;
    let binding = Binding {
        channel: parent.id,
        session: parent.remote_session,
    };
    let (transport, routes) = viewer.io().unwrap();
    if matches!(ending, Ending::HostStop | Ending::ControlIntent) {
        stop.set(true);
    } else {
        request_close(transport, routes.outbound, binding, ending, client_cx).await;
    }
    let mut reports = Vec::new();
    let until = now(client_cx) + 1_000_000;
    while !done.get() {
        assert!(now(client_cx) < until, "terminal owner failed to finish");
        transport
            .drive(client_cx, Duration::from_millis(1), || true)
            .await
            .unwrap();
        transport
            .receive_ready(
                client_cx,
                || true,
                |_| true,
                |route, bytes| {
                    assert_eq!(route, Route::Stream(routes.inbound));
                    let report = closure::decode_closed(
                        bytes,
                        binding,
                        &fr_core::limits::ProtocolLimits::ABSOLUTE,
                        InputDirection::HostToViewer,
                        InputDelivery::Reliable,
                    )
                    .unwrap();
                    assert!(
                        observed.lock().unwrap().as_ref().unwrap().check().is_err(),
                        "report before fence"
                    );
                    reports.push(report);
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
    }
    reports
}
async fn request_close(
    transport: &mut quic::QuicRecords,
    outbound: quic::StreamRoute,
    binding: Binding,
    ending: Ending,
    cx: &Cx,
) {
    let mut bytes = [0; closure::REQUEST_BYTES];
    let n = closure::encode_request(
        CloseRequest {
            reason: Reason::Requested,
        },
        binding,
        &fr_core::limits::ProtocolLimits::ABSOLUTE,
        &mut bytes,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .unwrap();
    if ending == Ending::Malformed {
        bytes[n - 1] = 255;
    }
    let until = now(cx) + 500_000;
    loop {
        match transport.send(cx, Route::Stream(outbound), &bytes[..n], until, || true) {
            Ok(()) => break,
            Err(quic::Error::Backpressure) => {
                transport
                    .drive(cx, Duration::from_millis(1), || true)
                    .await
                    .unwrap();
            }
            error => panic!("request send: {error:?}"),
        }
    }
}
fn verify(
    result: Result<Result<(), SessionError>, frd::native_connection::host::LinuxError>,
    reports: &[Closed],
    status: Option<frd::native_connection::host::ObservationClosure>,
    ending: Ending,
) {
    let app = result.unwrap();
    match ending {
        Ending::HostStop | Ending::ControlIntent => assert_eq!(app, Ok(())),
        Ending::Requested | Ending::SecurityLoss => {
            assert_eq!(app, Err(SessionError::Renewal(renewal::Error::PeerClosed)));
        }
        Ending::Malformed => assert!(matches!(
            app,
            Err(SessionError::Renewal(renewal::Error::Wire(_)))
        )),
    }
    match ending {
        Ending::ControlIntent => {
            assert_eq!(status, None);
            assert_eq!(reports, [] as [Closed; 0]);
        }
        Ending::SecurityLoss => {
            assert_eq!(status.unwrap().delivery, Err(quic::Error::Unauthorized));
            assert_eq!(reports, [] as [Closed; 0]);
        }
        _ => {
            let report = Closed {
                reason: match ending {
                    Ending::Requested => ClosedReason::ClientRequested,
                    Ending::Malformed => ClosedReason::ProtocolError,
                    _ => ClosedReason::HostStopping,
                },
                cleanup: Cleanup::Unconfirmed,
                effects: OutstandingEffects::Unknown,
            };
            assert_eq!(reports, [report]);
            let status = status.unwrap();
            assert_eq!(status.report, report);
            assert_eq!(status.delivery, Ok(()));
        }
    }
}

#[test]
#[ignore = "requires isolated synthetic ingress fixture via test_linux_serial_lifecycle.sh"]
fn validated_close_request_automatically_reports_on_the_original_listener() {
    run(async |b, h, c| Box::pin(scenario(b, h, c, Ending::Requested)).await);
}
#[test]
#[ignore = "requires isolated synthetic ingress fixture via test_linux_serial_lifecycle.sh"]
fn ordinary_observation_stop_does_not_claim_native_cleanup_or_empty_effects() {
    run(async |b, h, c| Box::pin(scenario(b, h, c, Ending::HostStop)).await);
}
#[test]
#[ignore = "requires isolated synthetic ingress fixture via test_linux_serial_lifecycle.sh"]
fn malformed_close_request_is_not_misreported_as_a_client_requested_close() {
    run(async |b, h, c| Box::pin(scenario(b, h, c, Ending::Malformed)).await);
}
#[test]
#[ignore = "requires isolated synthetic ingress fixture via test_linux_serial_lifecycle.sh"]
fn credential_revocation_after_capture_prevents_the_final_report() {
    run(async |b, h, c| Box::pin(scenario(b, h, c, Ending::SecurityLoss)).await);
}
#[test]
#[ignore = "requires isolated synthetic ingress fixture via test_linux_serial_lifecycle.sh"]
fn control_intent_does_not_occupy_the_future_lease_reporters_slot() {
    run(async |b, h, c| Box::pin(scenario(b, h, c, Ending::ControlIntent)).await);
}

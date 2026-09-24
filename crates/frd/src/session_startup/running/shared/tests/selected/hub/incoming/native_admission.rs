//! Actual UDP/TLS and credential-checked `LocalAPI` joined to the shared hub.
//! Synthetic metadata, ingress lifetime, source pictures and decoder acks are
//! explicit fixtures. Run ignored cases in the isolated namespace used by
//! `crates/frd/tests/native_host_accept.rs`, never on the host network.
use super::support as network;
use super::*;
use crate as frd;
use crate::native_connection::host::{Error as NativeError, IngressCheck, Request, Server};
use fr_tailnet::LocalApi;
use fr_transport::native_accept::{self, Listener};
use std::net::SocketAddr;
#[allow(dead_code)]
#[path = "../../../../../../../../tests/native_host_accept/fixture.rs"]
mod api_fixture;
use api_fixture::{Api, Mode, boundary, listener};

fn bounded(rt: &Runtime, body: impl Future<Output = ()>) {
    rt.block_on(async {
        let clock = Cx::current().unwrap();
        asupersync::time::timeout(clock.now(), Duration::from_secs(8), Box::pin(body))
            .await
            .expect("bounded native/shared fixture expired");
    });
}
fn request() -> Request {
    let mut request = api_fixture::request();
    request.session.binding.host_boot = HostBootId::from_raw(11);
    request.session.binding.os_session = OsSessionId::from_raw(12);
    request.session.binding.remote_session = RemoteSessionId::from_raw(14);
    let mut capabilities: Vec<_> = [
        decoder::CAPABILITY,
        attachment::CAPABILITY,
        attachment::DELIVERY_CAPABILITY,
        fr_wire::display::CAPABILITY,
    ]
    .into_iter()
    .map(|name| Capability {
        name: name.into(),
        version: 1,
        required: true,
    })
    .collect();
    capabilities.sort_by(|a, b| a.name.cmp(&b.name));
    request.session.offer.capabilities = capabilities;
    request
}
async fn connecting(cx: &Cx, address: SocketAddr) -> Viewer {
    let native = api_fixture::client(cx, address).await;
    Viewer::new(
        cx.clone(),
        native,
        request().session.offer,
        request().session.transport,
        Duration::from_secs(3),
    )
    .unwrap()
}

#[test]
#[ignore = "requires isolated root user/network namespace with two assigned fixture addresses"]
fn cold_native_observer_joins_existing_hub_after_identity_and_approval() {
    let rt = support::runtime();
    bounded(&rt, async {
        let hc = rt.request_cx_with_budget(Budget::INFINITE);
        let cc = rt.request_cx_with_budget(Budget::INFINITE);
        let broker = rt.request_cx_with_budget(Budget::INFINITE);
        let api = Api::new();
        let identity = api.identity(&broker).await;
        let mut server = Server::new(api.client.clone(), identity.clone());
        let socket = listener(&hc).await;
        let address = socket.local_addr();
        let (mut publisher, owner, mut first, mut hub, admission, initial) =
            Box::pin(fixture(&rt, service::Policy::default(), entropy())).await;
        let pid = publisher.worker_id();
        let local = Arc::new(Mutex::new(None));
        let copy = local.clone();
        let whois = api.whois.clone();
        let operation = server.serve_shared_observer(
            &hc,
            socket,
            request(),
            boundary(address, Arc::new(AtomicBool::new(true))),
            admission.clone(),
            move |approval, role| {
                assert_eq!(role, Role::Observe);
                assert!(whois.load(Ordering::Acquire) > 0);
                *copy.lock().unwrap() = Some(approval);
                Ok(())
            },
        );
        is_send(&operation);
        assert!(operation.ticket().unwrap().is_none());
        Box::pin(together(
            &mut hub,
            &mut publisher,
            with_client(&mut first.peer, async {
                let client = async {
                    let mut viewer = connecting(&cc, address).await;
                    prompt(&mut viewer, &local).await;
                    assert!(hc.checkpoint().is_ok());
                    assert_eq!(admission.statistics().unwrap().admitted, 2);
                    for _ in 0..3 {
                        viewer.drive(Duration::from_millis(1)).await.unwrap();
                    }
                    assert!(!viewer.is_complete());
                    local.lock().unwrap().take().unwrap().decide(true).unwrap();
                    let mut client = Box::pin(Client::start(cc.clone(), viewer)).await;
                    client.ready().await;
                    assert_ne!(client.frames.len(), 0);
                    assert!(hc.checkpoint().is_ok());
                    assert!(owner.check().is_ok());
                    assert_eq!(initial.state(), State::Serving);
                    client.viewer.close();
                };
                let (result, ()) = Box::pin(support::both(operation, client)).await;
                assert!(
                    matches!(result, Err(NativeError::Shared(service::Error::Session(_)))),
                    "{result:?}"
                );
                assert!(hc.checkpoint().is_err());
                assert!(identity.status(&broker).is_ok());
                assert!(owner.check().is_ok());
                assert_eq!(initial.state(), State::Serving);
            }),
        ))
        .await;
        assert_eq!(publisher.worker_id(), pid);
        hub.close();
        reap(&mut publisher).await;
    });
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stop {
    Denied,
    Ingress,
    Credentials,
    Cancelled,
    Abandoned,
}
// Keep the client half alive until after the host boundary under test fires.
enum Connected {
    Opening(Box<Viewer>),
    Streaming(Box<Client>),
}
impl Connected {
    fn close(mut self) {
        match &mut self {
            Self::Opening(viewer) => viewer.close(),
            Self::Streaming(client) => client.viewer.close(),
        }
    }
}
#[allow(clippy::too_many_lines)]
fn scoped_stop(stop: Stop) {
    let rt = support::runtime();
    bounded(&rt, async {
        let hc = rt.request_cx_with_budget(Budget::INFINITE);
        let cc = rt.request_cx_with_budget(Budget::INFINITE);
        let broker = rt.request_cx_with_budget(Budget::INFINITE);
        let api = Api::new();
        let identity = api.identity(&broker).await;
        let mut server = Server::new(api.client.clone(), identity.clone());
        let socket = listener(&hc).await;
        let address = socket.local_addr();
        let (mut publisher, owner, mut first, mut hub, admission, initial) =
            Box::pin(fixture(&rt, service::Policy::default(), entropy())).await;
        let local = Arc::new(Mutex::new(None));
        let copy = local.clone();
        let live = Arc::new(AtomicBool::new(true));
        let mut operation = Box::pin(server.serve_shared_observer(
            &hc,
            socket,
            request(),
            boundary(address, live.clone()),
            admission.clone(),
            move |approval, _| {
                *copy.lock().unwrap() = Some(approval);
                Ok(())
            },
        ));
        Box::pin(together(
            &mut hub,
            &mut publisher,
            with_client(&mut first.peer, async {
                let mut connecting = Box::pin(async {
                    let mut viewer = connecting(&cc, address).await;
                    prompt(&mut viewer, &local).await;
                    assert!(hc.checkpoint().is_ok());
                    if matches!(stop, Stop::Ingress | Stop::Abandoned) {
                        local.lock().unwrap().take().unwrap().decide(true).unwrap();
                        let mut client = Box::pin(Client::start(cc.clone(), viewer)).await;
                        client.ready().await;
                        assert_ne!(client.frames.len(), 0);
                        Connected::Streaming(client)
                    } else {
                        Connected::Opening(Box::new(viewer))
                    }
                });
                let peer = poll_fn(|task| {
                    assert!(
                        operation.as_mut().poll(task).is_pending(),
                        "native owner ended during handoff"
                    );
                    connecting.as_mut().poll(task)
                })
                .await;
                assert_eq!(admission.statistics().unwrap().admitted, 2);
                assert!(hc.checkpoint().is_ok());
                let expected = match stop {
                    Stop::Denied => {
                        local.lock().unwrap().take().unwrap().decide(false).unwrap();
                        NativeError::Shared(service::Error::Session(OpenError::Denied))
                    }
                    Stop::Ingress => {
                        live.store(false, Ordering::Release);
                        NativeError::IngressUnavailable
                    }
                    Stop::Credentials => {
                        identity.stop();
                        NativeError::Tailnet(fr_tailnet::Error::Revoked)
                    }
                    Stop::Cancelled | Stop::Abandoned => NativeError::Cancelled,
                };
                if stop == Stop::Cancelled {
                    hc.cancel_fast(asupersync::types::CancelKind::User);
                }
                if stop != Stop::Abandoned {
                    let outcome = operation.as_mut().await;
                    if stop == Stop::Denied {
                        let State::Finished(result) = operation.ticket().unwrap().unwrap().state()
                        else {
                            panic!("denied observer did not finish");
                        };
                        assert!(result.is_err());
                        assert_eq!(outcome, result.map_err(NativeError::Shared));
                    } else {
                        assert_eq!(outcome, Err(expected));
                    }
                    let mut context = Context::from_waker(Waker::noop());
                    assert_eq!(operation.as_mut().poll(&mut context), Poll::Ready(outcome));
                }
                drop(operation);
                assert!(hc.checkpoint().is_err());
                if let Some(approval) = local.lock().unwrap().take() {
                    assert!(approval.decide(true).is_err());
                }
                assert!(owner.check().is_ok());
                assert_eq!(initial.state(), State::Serving);
                assert!(broker.checkpoint().is_ok());
                if stop != Stop::Credentials {
                    assert!(identity.status(&broker).is_ok());
                }
                // The hub drains the original cancelled slot, not the shared source.
                for _ in 0..20 {
                    if admission.statistics().unwrap().finished == 1 {
                        break;
                    }
                    asupersync::runtime::yield_now().await;
                }
                assert_eq!(admission.statistics().unwrap().finished, 1);
                assert_eq!(initial.state(), State::Serving);
                peer.close();
            }),
        ))
        .await;
        hub.close();
        reap(&mut publisher).await;
    });
}
#[test]
#[ignore = "requires isolated root user/network namespace with two assigned fixture addresses"]
fn native_shared_denial_preserves_original_terminal_result() {
    scoped_stop(Stop::Denied);
}
#[test]
#[ignore = "requires isolated root user/network namespace with two assigned fixture addresses"]
fn native_shared_ingress_loss_stops_streaming_without_stopping_siblings() {
    scoped_stop(Stop::Ingress);
}
#[test]
#[ignore = "requires isolated root user/network namespace with two assigned fixture addresses"]
fn native_shared_credential_stop_fences_pending_approval() {
    scoped_stop(Stop::Credentials);
}
#[test]
#[ignore = "requires isolated root user/network namespace with two assigned fixture addresses"]
fn native_shared_external_cancellation_is_not_misreported_as_prior_hub_completion() {
    scoped_stop(Stop::Cancelled);
}
#[test]
#[ignore = "requires isolated root user/network namespace with two assigned fixture addresses"]
fn native_shared_abandonment_fences_the_original_streaming_slot() {
    scoped_stop(Stop::Abandoned);
}

#[test]
#[ignore = "requires isolated root user/network namespace with two assigned fixture addresses"]
fn native_shared_refuses_bad_identity_before_any_hub_slot_or_approval() {
    for (mode, error) in [
        (Mode::OtherUser, fr_tailnet::Error::ScopeDenied),
        (
            Mode::Unverifiable,
            fr_tailnet::Error::TailnetMembershipUnverifiable,
        ),
        (Mode::ChangeAfterWhoIs, fr_tailnet::Error::IdentityChanged),
    ] {
        let rt = support::runtime();
        bounded(&rt, async {
            let hc = rt.request_cx_with_budget(Budget::INFINITE);
            let cc = rt.request_cx_with_budget(Budget::INFINITE);
            let broker = rt.request_cx_with_budget(Budget::INFINITE);
            let api = Api::new();
            let identity = api.identity(&broker).await;
            *api.mode.lock().unwrap() = mode;
            let mut server = Server::new(api.client.clone(), identity);
            let socket = listener(&hc).await;
            let address = socket.local_addr();
            let (mut publisher, owner, mut first, mut hub, admission, initial) =
                Box::pin(fixture(&rt, service::Policy::default(), entropy())).await;
            let operation = server.serve_shared_observer(
                &hc,
                socket,
                api_fixture::request_for(mode),
                boundary(address, Arc::new(AtomicBool::new(true))),
                admission.clone(),
                |_, _| panic!("no notification before identity"),
            );
            Box::pin(together(
                &mut hub,
                &mut publisher,
                with_client(&mut first.peer, async {
                    let (result, peer) =
                        Box::pin(support::both(operation, api_fixture::client(&cc, address))).await;
                    assert_eq!(result, Err(NativeError::Tailnet(error)));
                    assert_eq!(admission.statistics().unwrap().admitted, 1);
                    assert_eq!(initial.state(), State::Serving);
                    assert!(owner.check().is_ok());
                    assert!(hc.checkpoint().is_err());
                    assert!(broker.checkpoint().is_ok());
                    drop(peer);
                }),
            ))
            .await;
            hub.close();
            reap(&mut publisher).await;
        });
    }
}
#[test]
#[ignore = "requires isolated root user/network namespace with two assigned fixture addresses"]
fn unpolled_native_shared_attempt_never_touches_hub_or_localapi() {
    let rt = support::runtime();
    bounded(&rt, async {
        let hc = rt.request_cx_with_budget(Budget::INFINITE);
        let broker = rt.request_cx_with_budget(Budget::INFINITE);
        let api = Api::new();
        let identity = api.identity(&broker).await;
        let mut server = Server::new(api.client.clone(), identity.clone());
        let socket = listener(&hc).await;
        let address = socket.local_addr();
        let (mut publisher, owner, _first, mut hub, admission, initial) =
            Box::pin(fixture(&rt, service::Policy::default(), entropy())).await;
        let before = api.calls.load(Ordering::Acquire);
        drop(server.serve_shared_observer(
            &hc,
            socket,
            request(),
            boundary(address, Arc::new(AtomicBool::new(true))),
            admission.clone(),
            |_, _| panic!("unpolled"),
        ));
        assert!(hc.checkpoint().is_err());
        assert!(identity.status(&broker).is_ok());
        assert!(std::net::UdpSocket::bind(address).is_ok());
        assert_eq!(api.calls.load(Ordering::Acquire), before);
        assert_eq!(admission.statistics().unwrap().admitted, 1);
        assert_eq!(initial.state(), State::Serving);
        assert!(owner.check().is_ok());
        hub.close();
        reap(&mut publisher).await;
    });
}

#[test]
#[ignore = "requires isolated root user/network namespace with two assigned fixture addresses"]
fn native_shared_preserves_scope_duplicate_and_capacity_admission() {
    for (viewers, session_id, os_id, expected) in [
        (1, 14, 12, service::Error::Full),
        (2, 13, 12, service::Error::DuplicateSession),
        (2, 14, 99, service::Error::ForeignScope),
    ] {
        let rt = support::runtime();
        bounded(&rt, async {
            let hc = rt.request_cx_with_budget(Budget::INFINITE);
            let cc = rt.request_cx_with_budget(Budget::INFINITE);
            let broker = rt.request_cx_with_budget(Budget::INFINITE);
            let api = Api::new();
            let identity = api.identity(&broker).await;
            let mut server = Server::new(api.client.clone(), identity);
            let socket = listener(&hc).await;
            let address = socket.local_addr();
            let (mut publisher, owner, mut first, mut hub, admission, initial) = Box::pin(fixture(
                &rt,
                service::Policy {
                    viewers,
                    ..Default::default()
                },
                entropy(),
            ))
            .await;
            let mut req = request();
            req.session.binding.remote_session = RemoteSessionId::from_raw(session_id);
            req.session.binding.os_session = OsSessionId::from_raw(os_id);
            let operation = server.serve_shared_observer(
                &hc,
                socket,
                req,
                boundary(address, Arc::new(AtomicBool::new(true))),
                admission.clone(),
                |_, _| panic!("no notification without a slot"),
            );
            Box::pin(together(
                &mut hub,
                &mut publisher,
                with_client(&mut first.peer, async {
                    let (result, peer) =
                        Box::pin(support::both(operation, api_fixture::client(&cc, address))).await;
                    assert_eq!(result, Err(NativeError::Shared(expected)));
                    assert_eq!(admission.statistics().unwrap().admitted, 1);
                    assert_eq!(initial.state(), State::Serving);
                    assert!(owner.check().is_ok());
                    assert!(hc.checkpoint().is_err());
                    assert!(broker.checkpoint().is_ok());
                    drop(peer);
                }),
            ))
            .await;
            hub.close();
            reap(&mut publisher).await;
        });
    }
}

#[test]
#[ignore = "requires isolated root user/network namespace with two assigned fixture addresses"]
fn native_shared_acquisition_budget_starts_at_call_time_not_first_poll() {
    let rt = support::runtime();
    bounded(&rt, async {
        let hc = rt.request_cx_with_budget(Budget::INFINITE);
        let broker = rt.request_cx_with_budget(Budget::INFINITE);
        let api = Api::new();
        let identity = api.identity(&broker).await;
        let mut server = Server::new(api.client.clone(), identity.clone());
        let socket = Listener::bind(
            &hc,
            "100.64.0.1:0".parse().unwrap(),
            native_accept::Configuration {
                initial_timeout: Duration::from_millis(30),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let address = socket.local_addr();
        let (mut publisher, owner, _first, mut hub, admission, initial) =
            Box::pin(fixture(&rt, service::Policy::default(), entropy())).await;
        let before = api.calls.load(Ordering::Acquire);
        let operation = server.serve_shared_observer(
            &hc,
            socket,
            request(),
            boundary(address, Arc::new(AtomicBool::new(true))),
            admission.clone(),
            |_, _| panic!("expired attempt"),
        );
        asupersync::time::sleep(broker.now(), Duration::from_millis(60)).await;
        assert_eq!(
            operation.await,
            Err(NativeError::Accept(native_accept::Error::InitialTimeout))
        );
        assert!(hc.checkpoint().is_err());
        assert!(identity.status(&broker).is_ok());
        assert!(std::net::UdpSocket::bind(address).is_ok());
        assert_eq!(api.calls.load(Ordering::Acquire), before);
        assert_eq!(admission.statistics().unwrap().admitted, 1);
        assert_eq!(initial.state(), State::Serving);
        assert!(owner.check().is_ok());
        hub.close();
        reap(&mut publisher).await;
    });
}

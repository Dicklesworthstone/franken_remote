//! Existing Host callback -> bound UI -> actual `XTest` choice, no alternate grant.
use super::*;
use crate::logind::{
    Selection, Status as SessionStatus, Watch,
    approval::{
        Status, WORKER,
        tests::{fixture, interaction, network, until},
    },
    bus::fixture::{Data, Peer as Login, SERIAL, uid},
};
use asupersync::{
    cx::Cx,
    runtime::Runtime,
    time::{sleep, timeout},
    types::Budget,
};
use fr_core::{
    input::{DesktopPoint, InputBounds},
    time::HostInstant,
};
use frd::{
    native_connection::host::Server,
    session_agent::{ApprovalMode, SessionAgent},
    session_startup::Viewer,
};
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

fn agent(id: u32) -> SessionAgent {
    SessionAgent::new(
        ApprovalMode::PromptAlways,
        PlatformKind::LinuxX11,
        id,
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
    )
}
fn run(test: impl AsyncFnOnce(&Runtime, &Cx, Control, String)) {
    let _serial = SERIAL.lock().unwrap();
    let display = std::env::var("DISPLAY").expect("explicit isolated Xvfb required");
    let login = Login::new(Data {
        uid: uid(),
        display: display.clone(),
        ..Data::default()
    });
    let selected = Selection {
        uid: uid(),
        display: display.clone(),
        ..crate::logind::bus::fixture::selection()
    };
    let mut watch = Watch::spawn(selected, login.address.clone(), uid()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while watch.control().status() == SessionStatus::Opening {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(watch.control().status(), SessionStatus::Active);
    let rt = network::runtime();
    let broker = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        timeout(
            broker.now(),
            Duration::from_secs(22),
            test(&rt, &broker, watch.control(), display),
        )
        .await
        .unwrap();
        until(&broker, || !WORKER.load(Ordering::Acquire)).await;
    });
    watch.stop();
    while !watch.try_finish().unwrap() {
        std::thread::sleep(Duration::from_millis(2));
    }
}
async fn attempt(
    rt: &Runtime,
    broker: &Cx,
    mut notify: impl FnMut(Approval, Role) -> Result<(), ()> + Send + 'static,
    act: impl AsyncFnOnce(Approval),
) -> bool {
    let hc = rt.request_cx_with_budget(Budget::INFINITE);
    let vc = rt.request_cx_with_budget(Budget::INFINITE);
    let api = fixture::Api::new();
    let identity = api.identity(broker).await;
    let mut server = Server::new(api.client.clone(), identity.clone());
    let listener = fixture::listener(&hc).await;
    let address = listener.local_addr();
    let mut request = fixture::request();
    request.session.startup_timeout = Duration::from_secs(6);
    let observed = Arc::new(Mutex::new(None));
    let copy = observed.clone();
    let done = AtomicBool::new(false);
    let host = server.run_on_protected_listener(
        &hc,
        listener,
        request,
        fixture::boundary(address, Arc::new(AtomicBool::new(true))),
        |host| async {
            let result = host
                .open(Duration::from_millis(5), move |approval, role| {
                    assert_eq!(approval.binding().os_session.as_raw(), 2);
                    *copy.lock().unwrap() = Some(approval.clone());
                    notify(approval, role)
                })
                .await;
            done.store(true, Ordering::Release);
            result.map(drop)
        },
    );
    let client = async {
        let native = fixture::client(&vc, address).await;
        let mut viewer = Viewer::new(
            vc.clone(),
            native,
            fixture::offer(),
            fr_transport::quic::Policy::default(),
            Duration::from_secs(6),
        )
        .unwrap();
        loop {
            if viewer.is_complete() {
                return Some(viewer.finish().unwrap());
            }
            if done.load(Ordering::Acquire) || viewer.drive(Duration::from_millis(5)).await.is_err()
            {
                return None;
            }
        }
    };
    let local = async {
        until(broker, || observed.lock().unwrap().is_some()).await;
        let approval = observed.lock().unwrap().as_ref().unwrap().clone();
        act(approval).await;
    };
    let (result, (client, ())) = Box::pin(network::both(host, network::both(client, local))).await;
    assert!(
        observed
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .check_pending()
            .is_err()
    );
    assert!(identity.status(broker).is_ok());
    let allowed = matches!(result, Ok(Ok(())));
    if allowed {
        assert!(client.is_some());
    }
    allowed
}
async fn mapped(cx: &Cx, ui: &ApprovalUi) -> PromptControl {
    until(cx, || {
        ui.current().is_some_and(|c| c.status() != Status::Opening)
    })
    .await;
    let control = ui.current().unwrap();
    assert_eq!(control.status(), Status::Mapped);
    control
}
async fn collect(cx: &Cx, ui: &mut ApprovalUi) -> Outcome {
    loop {
        if let Some(outcome) = ui.collect() {
            assert_eq!(ui.collect(), None);
            return outcome;
        }
        sleep(cx.now(), Duration::from_millis(2)).await;
    }
}
#[test]
#[ignore = "isolated network and Xvfb"]
fn notifications_require_original_retirement_collection_before_next_session() {
    run(async |rt, broker, session, display| {
        let original = agent(2);
        let mut ui = ApprovalUi::new(session, &original).unwrap();
        assert!(ui.current().is_none());
        assert!(
            attempt(rt, broker, ui.callback(), async |_| {
                let c = mapped(broker, &ui).await;
                interaction(broker, &display, c.window().unwrap(), "allow").await;
            })
            .await
        );
        assert_eq!(
            ui.current().unwrap().status(),
            Status::Finished(Outcome::Allowed(Role::Observe))
        );
        // No implicit old receipt collection and no positive-decision reuse.
        assert!(
            !attempt(rt, broker, ui.callback(), async |cap| {
                assert!(cap.check_pending().is_err());
            })
            .await
        );
        assert_eq!(
            collect(broker, &mut ui).await,
            Outcome::Allowed(Role::Observe)
        );
        assert!(ui.current().is_none());
        assert!(
            attempt(rt, broker, ui.callback(), async |_| {
                let c = mapped(broker, &ui).await;
                interaction(broker, &display, c.window().unwrap(), "allow").await;
            })
            .await
        );
        assert_eq!(
            collect(broker, &mut ui).await,
            Outcome::Allowed(Role::Observe)
        );
        assert!(!original.is_revoked());
    });
}
#[test]
#[ignore = "isolated network and Xvfb"]
fn weak_callback_after_ui_drop_denies_without_native_work() {
    run(async |rt, broker, session, _| {
        let original = agent(2);
        let ui = ApprovalUi::new(session, &original).unwrap();
        let notify = ui.callback();
        drop(ui);
        assert!(
            !attempt(rt, broker, notify, async |cap| {
                assert!(cap.check_pending().is_err());
            })
            .await
        );
        assert!(!WORKER.load(Ordering::Acquire));
        assert!(!original.is_revoked());
    });
}
#[test]
#[ignore = "isolated network and Xvfb"]
fn notification_role_cannot_mislabel_original_intent() {
    run(async |rt, broker, session, _| {
        let original = agent(2);
        let ui = ApprovalUi::new(session, &original).unwrap();
        let mut notify = ui.callback();
        assert!(
            !attempt(
                rt,
                broker,
                move |a, _| notify(a, Role::RequestControl),
                async |_| {}
            )
            .await
        );
        assert!(ui.current().is_none());
        assert!(!WORKER.load(Ordering::Acquire));
    });
}
#[test]
#[ignore = "isolated network and Xvfb"]
fn foreign_os_binding_never_opens_consent_on_another_desktop() {
    run(async |rt, broker, session, _| {
        let different = agent(3);
        let ui = ApprovalUi::new(session, &different).unwrap();
        assert!(!attempt(rt, broker, ui.callback(), async |_| {}).await);
        assert!(ui.current().is_none());
        assert!(!different.is_revoked());
    });
}
#[test]
#[ignore = "isolated network and Xvfb"]
fn original_agent_revocation_denies_without_revoking_equal_id_replacement() {
    run(async |rt, broker, session, _| {
        let mut original = agent(2);
        let replacement = agent(2);
        let mut ui = ApprovalUi::new(session, &original).unwrap();
        assert!(
            !attempt(rt, broker, ui.callback(), async |_| {
                let _c = mapped(broker, &ui).await;
                original.immediate_revoke(
                    HostInstant::from_micros(0),
                    frd::input_watchdog::StopReason::AuthorityEnded,
                );
                assert!(!replacement.is_revoked());
            })
            .await
        );
        assert!(matches!(
            collect(broker, &mut ui).await,
            Outcome::Refused(Error::Cancelled | Error::AgentUnavailable)
        ));
        assert!(!replacement.is_revoked());
        assert!(!attempt(rt, broker, ui.callback(), async |_| {}).await);
    });
}
#[test]
#[ignore = "isolated network and Xvfb"]
fn dropped_original_agent_cannot_be_replaced_by_equal_numeric_identity() {
    run(async |rt, broker, session, _| {
        let original = agent(2);
        let replacement = agent(2);
        let ui = ApprovalUi::new(session, &original).unwrap();
        drop(original);
        assert!(!attempt(rt, broker, ui.callback(), async |_| {}).await);
        assert!(!replacement.is_revoked());
        assert!(ui.current().is_none());
    });
}
#[test]
#[ignore = "isolated network and Xvfb"]
fn closing_ui_denies_pending_request_and_keeps_native_cleanup_collectable() {
    run(async |rt, broker, session, _| {
        let original = agent(2);
        let mut ui = ApprovalUi::new(session, &original).unwrap();
        assert!(
            !attempt(rt, broker, ui.callback(), async |_| {
                let _c = mapped(broker, &ui).await;
                ui.stop();
            })
            .await
        );
        assert_eq!(
            collect(broker, &mut ui).await,
            Outcome::Refused(Error::Cancelled)
        );
        assert!(!original.is_revoked());
        assert!(!attempt(rt, broker, ui.callback(), async |_| {}).await);
    });
}

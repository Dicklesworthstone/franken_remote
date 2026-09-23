//! Real TLS/UDP approval, private login1 bus and XCB/XTest. Peer identity,
//! logind metadata and user actions are explicit fixtures, not installed proof.
use super::*;
use crate::logind::{
    Selection, Watch,
    bus::fixture::{Data, Event, Peer as Login, SERIAL, uid},
};
use asupersync::{
    cx::Cx,
    time::{sleep, timeout},
    types::{Budget, CancelKind},
};
use fr_tailnet::LocalApi;
use fr_transport::native_accept::{self, Listener};
use frd::{
    native_connection::host::{IngressCheck, Request, Server},
    session_startup::{Host, Viewer},
};
use std::net::SocketAddr;
#[allow(dead_code)]
#[path = "../../../../frd/tests/native_host_accept/fixture.rs"]
pub(super) mod fixture;
#[allow(dead_code)]
#[path = "../../../../fr-transport/tests/support/mod.rs"]
pub(super) mod network;

pub(super) async fn until(cx: &Cx, ready: impl Fn() -> bool) {
    while !ready() {
        sleep(cx.now(), Duration::from_millis(2)).await;
    }
}
pub(super) async fn interaction(cx: &Cx, display: &str, window: u32, op: &str) {
    let mut child = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/sharing_indicator/peer.py"
        ))
        .arg(display)
        .arg(window.to_string())
        .arg(op)
        .spawn()
        .unwrap();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "independent X11 peer failed");
            return;
        }
        sleep(cx.now(), Duration::from_millis(2)).await;
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    Allow,
    Deny,
    Hide,
    Lock,
    Cancel,
    Drop,
    Expire,
    Synthetic,
    Duplicate,
    WrongUser,
    Deadline,
}
#[allow(clippy::too_many_lines)]
fn exercise(action: Action, role: Role) {
    let _serial = SERIAL.lock().unwrap();
    let display = std::env::var("DISPLAY").expect("isolated Xvfb display required");
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
    let wait_until = Instant::now() + Duration::from_secs(3);
    while watch.control().status() == SessionStatus::Opening {
        assert!(Instant::now() < wait_until);
        thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(watch.control().status(), SessionStatus::Active);
    let runtime = network::runtime();
    let broker = runtime.request_cx_with_budget(Budget::INFINITE);
    let hc = runtime.request_cx_with_budget(Budget::INFINITE);
    let vc = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        timeout(broker.now(), Duration::from_secs(12), async {
            let api = fixture::Api::new();
            let identity = api.identity(&broker).await;
            let mut server = Server::new(api.client.clone(), identity.clone());
            let listener = fixture::listener(&hc).await;
            let address = listener.local_addr();
            let mut request = fixture::request();
            request.session.offer.role = role;
            request.session.require_approval = true;
            request.session.startup_timeout = Duration::from_secs(6);
            if action == Action::Deadline {
                request.session.startup_timeout = Duration::from_millis(650);
            }
            let prompt: Arc<Mutex<Option<Prompt>>> = Arc::new(Mutex::new(None));
            let decision: Arc<Mutex<Option<Approval>>> = Arc::new(Mutex::new(None));
            let slot = prompt.clone();
            let approval_slot = decision.clone();
            let session = watch.control();
            let notify = move |approval: Approval, requested: Role| {
                assert_eq!(requested, role);
                assert_eq!(approval.role(), role);
                assert_eq!(approval.check_pending(), Ok(()));
                *approval_slot.lock().unwrap() = Some(approval.clone());
                if action == Action::WrongUser {
                    // Explicit wrong-user evidence fixture: no production escape.
                    let wrong = Control(Arc::new(super::super::Shared {
                        selection: Selection {
                            uid: uid().wrapping_add(1),
                            ..session.0.selection.clone()
                        },
                        state: std::sync::atomic::AtomicU8::new(1),
                        deadline: std::sync::atomic::AtomicU64::new(u64::MAX),
                        waker: Mutex::new(None),
                    }));
                    assert!(matches!(
                        Prompt::start(wrong, approval),
                        Err(Error::WrongSession)
                    ));
                } else {
                    *slot.lock().unwrap() = Some(Prompt::start(session.clone(), approval).unwrap());
                }
                Ok(())
            };
            let finished = AtomicBool::new(false);
            let serving = server.run_on_protected_listener(
                &hc,
                listener,
                request,
                fixture::boundary(address, Arc::new(AtomicBool::new(true))),
                |host: Host| async {
                    let result = host.open(Duration::from_millis(5), notify).await;
                    finished.store(true, Ordering::Release);
                    result.map(drop)
                },
            );
            let client = async {
                let native = fixture::client(&vc, address).await;
                let mut offer = fixture::offer();
                offer.role = role;
                let mut viewer = Viewer::new(
                    vc.clone(),
                    native,
                    offer,
                    fr_transport::quic::Policy::default(),
                    Duration::from_secs(6),
                )
                .unwrap();
                loop {
                    if viewer.is_complete() {
                        return Some(viewer.finish().unwrap());
                    }
                    if finished.load(Ordering::Acquire) {
                        return None;
                    }
                    if viewer.drive(Duration::from_millis(5)).await.is_err() {
                        return None;
                    }
                }
            };
            let user = async {
                until(&broker, || decision.lock().unwrap().is_some()).await;
                if action == Action::WrongUser {
                    return None;
                }
                let control = prompt.lock().unwrap().as_ref().unwrap().control();
                until(&broker, || control.status() != Status::Opening).await;
                assert_eq!(control.status(), Status::Mapped);
                let approval = decision.lock().unwrap().as_ref().unwrap().clone();
                assert_eq!(approval.check_pending(), Ok(()), "mapping is not consent");
                let window = control.window().unwrap();
                match action {
                    Action::Allow => interaction(&broker, &display, window, "allow").await,
                    Action::Deny => interaction(&broker, &display, window, "click").await,
                    Action::Hide => interaction(&broker, &display, window, "unmap").await,
                    Action::Lock => login.emit(Event::LockUnlock),
                    Action::Cancel => control.cancel(),
                    Action::Drop => drop(prompt.lock().unwrap().take()),
                    Action::Expire => hc.cancel_fast(CancelKind::User),
                    Action::Synthetic => {
                        for op in ["synthetic-allow", "release-allow", "drag-out"] {
                            interaction(&broker, &display, window, op).await;
                            sleep(broker.now(), Duration::from_millis(30)).await;
                            assert_eq!(control.status(), Status::Mapped, "{op} cannot approve");
                            assert_eq!(approval.check_pending(), Ok(()));
                        }
                        interaction(&broker, &display, window, "allow").await;
                    }
                    Action::Duplicate => {
                        assert!(matches!(
                            Prompt::start(watch.control(), approval.clone()),
                            Err(Error::Busy)
                        ));
                    }
                    Action::Deadline => {}
                    Action::WrongUser => unreachable!(),
                }
                until(&broker, || matches!(control.status(), Status::Finished(_))).await;
                Some(control)
            };
            let (result, (complete, control)) =
                Box::pin(network::both(serving, network::both(client, user))).await;
            let allowed = matches!(action, Action::Allow | Action::Synthetic);
            if allowed {
                assert!(
                    matches!(result, Ok(Ok(()))),
                    "host={result:?} ui={:?}",
                    control.as_ref().map(PromptControl::status)
                );
                assert!(complete.is_some());
            } else {
                assert!(result.is_err() || result.unwrap().is_err());
            }
            assert!(
                decision
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .check_pending()
                    .is_err()
            );
            if let Some(control) = control {
                let expected = match action {
                    Action::Allow | Action::Synthetic => Outcome::Allowed(role),
                    Action::Deny => Outcome::Denied,
                    Action::Hide => Outcome::Refused(Error::Hidden),
                    Action::Lock => Outcome::Refused(Error::SessionUnavailable),
                    Action::Cancel => Outcome::Refused(Error::Cancelled),
                    Action::Drop => Outcome::Refused(Error::OwnerDropped),
                    Action::Expire => Outcome::Refused(Error::Approval(ApprovalError::Cancelled)),
                    Action::Duplicate => Outcome::Refused(Error::Approval(ApprovalError::Order)),
                    Action::Deadline => {
                        let Status::Finished(Outcome::Refused(Error::Approval(error))) =
                            control.status()
                        else {
                            panic!("expired approval was not refused");
                        };
                        assert!(matches!(
                            error,
                            ApprovalError::Expired
                                | ApprovalError::Cancelled
                                | ApprovalError::Closed
                        ));
                        Outcome::Refused(Error::Approval(error))
                    }
                    Action::WrongUser => unreachable!(),
                };
                assert_eq!(control.status(), Status::Finished(expected));
                // Successful consent is stable; closing its UI is not another decision.
                control.cancel();
                assert_eq!(control.status(), Status::Finished(expected));
                let owner = prompt.lock().unwrap().take();
                if let Some(mut owner) = owner {
                    loop {
                        if let Some(outcome) = owner.try_finish() {
                            assert_eq!(outcome, expected);
                            break;
                        }
                        sleep(broker.now(), Duration::from_millis(2)).await;
                    }
                    assert_eq!(owner.try_finish(), Some(expected));
                }
            }
            until(&broker, || !WORKER.load(Ordering::Acquire)).await;
            assert!(identity.status(&broker).is_ok());
        })
        .await
        .unwrap();
    });
    watch.stop();
    while !watch.try_finish().unwrap() {
        thread::sleep(Duration::from_millis(2));
    }
}
macro_rules! case {
    ($name:ident, $action:ident) => {
        #[test]
        #[ignore = "requires isolated user/network namespace and explicit Xvfb"]
        fn $name() {
            exercise(Action::$action, Role::Observe);
        }
    };
}
case!(
    local_allow_completes_the_original_observation_handshake,
    Allow
);
case!(native_deny_never_completes_observation, Deny);
case!(hiding_the_window_denies_the_original_request, Hide);
case!(
    logind_lock_unlock_cannot_reauthorize_the_pending_prompt,
    Lock
);
case!(
    explicit_cancel_is_terminal_without_native_cleanup_wait,
    Cancel
);
case!(
    owner_drop_denies_even_while_native_resources_are_retiring,
    Drop
);
case!(original_host_cancellation_ends_idle_prompt, Expire);
case!(
    send_event_release_only_and_drag_out_cannot_approve,
    Synthetic
);
case!(
    busy_native_owner_refuses_instead_of_spawning_another_thread,
    Duplicate
);
case!(
    wrong_local_process_uid_refuses_before_native_work,
    WrongUser
);
#[test]
#[ignore = "requires isolated user/network namespace and explicit Xvfb"]
fn original_control_role_is_preserved_in_native_decision() {
    exercise(Action::Allow, Role::RequestControl);
}

case!(
    original_startup_deadline_denies_without_another_remote_packet,
    Deadline
);

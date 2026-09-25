//! Cold/warm routing when the agent carries a control profile. Real TLS/UDP and
//! supervised capture IPC; monitor/codec/permission replies are fixtures. The
//! full controlled share (real capture, input child, shipped client) is covered
//! by the namespace e2e, not here.
use super::super::super::super::preparation;
use super::*;
use crate::{
    input_agent::Seat,
    session_agent::source::desktop::{
        ControlProfile, Error as DesktopError,
        dispatch::{Driver, Error as DispatchError, Incoming},
    },
};
use fr_core::input_submission::{Capabilities, Capability};
use fr_wire::refusal::{Reason, Refused};

fn controlled_pair(rt: &Runtime) -> (Incoming, Driver, Cx) {
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let profile = ControlProfile::new(
        std::path::Path::new("/nonexistent/fr-input-agent"),
        ":0",
        None,
        Seat::default(),
        Capabilities::default()
            .with(Capability::Keys)
            .with(Capability::Absolute)
            .with(Capability::Buttons),
        30,
        2_000_000,
        fr_media::worker::Backend::SoftwareExplicit,
    )
    .unwrap();
    let (incoming, driver) = preparation::agent()
        .with_control(profile)
        .native_incoming(
            cx.clone(),
            service::Policy::default(),
            Duration::from_millis(50),
            entropy(),
        )
        .unwrap();
    (incoming, driver, cx)
}
/// Drive a viewer until its terminal startup error, then park.
async fn until_refused(mut viewer: Viewer, seen: Arc<Mutex<Option<OpenError>>>) {
    loop {
        if let Err(error) = viewer.drive(Duration::from_millis(1)).await {
            *seen.lock().unwrap() = Some(error);
            std::future::pending::<()>().await;
        }
        asupersync::runtime::yield_now().await;
    }
}

#[test]
fn control_enabled_cold_controller_is_exclusive_and_needs_the_full_control_profile() {
    let rt = support::runtime();
    rt.block_on(async {
        let (incoming, mut driver, independent) = controlled_pair(&rt);
        // The fixture offer carries only the observation capabilities; the
        // viewer asks to control with exactly those.
        let Fresh {
            host, viewer, h, ..
        } = *fresh(&rt, 13, false, Role::RequestControl, Duration::from_secs(2)).await;
        let first = incoming
            .serve_host(host, |_, _| panic!("unattended"))
            .unwrap();
        // While the controller's cold share is starting, later viewers are
        // refused Busy instead of queueing behind or joining it.
        let Fresh {
            host: extra,
            h: refused,
            viewer: _unused,
            ..
        } = *fresh(&rt, 14, false, Role::Observe, Duration::from_secs(2)).await;
        assert!(matches!(
            incoming.serve_host(extra, |_, _| Ok(())),
            Err(DispatchError::Busy)
        ));
        assert!(refused.is_cancel_requested());
        let factories = Arc::new(AtomicU64::new(0));
        let counted = factories.clone();
        let mut run = Box::pin(driver.serve(
            move || {
                counted.fetch_add(1, Ordering::SeqCst);
                Err(())
            },
            preparation::choose,
            |_, _| Ok(LocalAction::Continue),
        ));
        let seen = Arc::new(Mutex::new(None));
        let mut client = Box::pin(until_refused(viewer, seen.clone()));
        let mut peer = Box::pin(first);
        let mut peer_result = None;
        let result = poll_fn(|task| {
            if peer_result.is_none()
                && let Poll::Ready(outcome) = peer.as_mut().poll(task)
            {
                peer_result = Some(outcome);
            }
            if let Poll::Ready(result) = run.as_mut().poll(task) {
                return Poll::Ready(result);
            }
            assert!(client.as_mut().poll(task).is_pending());
            Poll::Pending
        })
        .await;
        // Negotiation was NOT restricted to observation (no ControlUnavailable
        // refusal); the missing control profile is the peer's own requirement.
        let error = result.unwrap_err();
        assert!(
            matches!(
                &error,
                DispatchError::Desktop(e) if matches!(
                    **e,
                    DesktopError::Startup(OpenError::Protocol(
                        fr_wire::negotiation::Error::RequiredCapability
                    ))
                )
            ),
            "{error:?}"
        );
        assert!(error.is_peer_outcome());
        assert_eq!(
            factories.load(Ordering::SeqCst),
            0,
            "no capture before the profile"
        );
        assert!(h.is_cancel_requested());
        assert!(!independent.is_cancel_requested());
        drop((run, client, peer));
        assert!(driver.worker_id().is_none());
    });
}

#[test]
#[allow(clippy::too_many_lines)]
fn control_enabled_observer_first_share_still_refuses_a_later_controller() {
    let rt = support::runtime();
    rt.block_on(Box::pin(async {
        let (incoming, mut driver, _) = controlled_pair(&rt);
        let Fresh {
            c, host, viewer, ..
        } = *fresh(&rt, 13, false, Role::Observe, Duration::from_secs(4)).await;
        let first_connection = incoming
            .serve_host(host, |_, _| panic!("unattended"))
            .unwrap();
        let (setup, source, mut retirement, _trace) = preparation::setup(&rt, "normal");
        let stop = Arc::new(AtomicBool::new(false));
        let local_stop = stop.clone();
        let mut running = Box::pin(driver.serve(
            move || Ok(setup),
            preparation::choose,
            move |_, _| {
                Ok(if local_stop.load(Ordering::Acquire) {
                    LocalAction::Stop
                } else {
                    LocalAction::Continue
                })
            },
        ));
        let seen = Arc::new(Mutex::new(None));
        let observed = seen.clone();
        let mut clients = Box::pin(async {
            let _first_connection = first_connection;
            let mut first = Box::pin(Client::start(c, viewer)).await;
            first.ready().await;
            // The share is warm and observation-only: a controller joining it
            // is refused before approval, never silently downgraded.
            let Fresh {
                host, viewer, h, ..
            } = *fresh(&rt, 15, false, Role::RequestControl, Duration::from_secs(4)).await;
            let second = incoming
                .serve_host(host, |_, _| panic!("refused before approval"))
                .unwrap();
            let mut second = Box::pin(second);
            let mut refused = Box::pin(until_refused(viewer, observed.clone()));
            let mut keep = Box::pin(async {
                loop {
                    first.turn().await;
                }
            });
            let peer = poll_fn(|task| {
                assert!(keep.as_mut().poll(task).is_pending());
                let _ = refused.as_mut().poll(task);
                if observed.lock().unwrap().is_some()
                    && let Poll::Ready(result) = second.as_mut().poll(task)
                {
                    return Poll::Ready(result);
                }
                let _ = second.as_mut().poll(task);
                Poll::Pending
            })
            .await;
            assert!(peer.is_err(), "{peer:?}");
            assert!(h.is_cancel_requested());
            assert!(source.check().is_ok(), "the observer share continues");
            stop.store(true, Ordering::Release);
            std::future::pending::<()>().await;
        });
        let report = poll_fn(|task| {
            if let Poll::Ready(result) = running.as_mut().poll(task) {
                return Poll::Ready(result);
            }
            assert!(clients.as_mut().poll(task).is_pending());
            Poll::Pending
        })
        .await
        .unwrap();
        assert_eq!(
            *seen.lock().unwrap(),
            Some(OpenError::ClientStartup(
                fr_client::startup::Error::Protocol(fr_wire::negotiation::Error::Refused(
                    Refused::connection(Reason::ControlUnavailable)
                ))
            ))
        );
        assert!(report.viewers.admitted >= 1);
        drop(clients);
        drop(running);
        let cx = Cx::current().unwrap();
        assert!(
            driver
                .reap(&cx, Deadline::after(&cx, Duration::from_secs(1)).unwrap())
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            retirement
                .reap(&cx, Deadline::after(&cx, Duration::from_secs(1)).unwrap())
                .await
                .unwrap()
                .is_some()
        );
    }));
}

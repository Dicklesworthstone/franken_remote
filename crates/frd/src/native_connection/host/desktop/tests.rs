//! Supervisor ownership/order tests. The futures are explicit lifecycle fixtures,
//! not protocol, native-input, media or live-tailnet qualification.
use super::*;
use crate::session_agent::{ApprovalMode, PlatformKind};
use asupersync::{
    runtime::{Runtime, RuntimeBuilder},
    types::Budget,
};
use fr_core::input::{DesktopPoint, InputBounds};
use std::{
    future::{pending, poll_fn},
    sync::Mutex,
    time::Duration,
};

fn fixture() -> (Runtime, Cx, dispatch::Driver) {
    let runtime = RuntimeBuilder::current_thread()
        .enable_platform_reactor(true)
        .build()
        .unwrap();
    let supervisor = runtime.request_cx_with_budget(Budget::INFINITE);
    let source = runtime.request_cx_with_budget(Budget::INFINITE);
    let agent = SessionAgent::new(
        ApprovalMode::Unattended,
        PlatformKind::LinuxX11,
        1,
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
    );
    let (_, driver) = agent
        .native_incoming(
            source,
            crate::session_startup::shared_viewers::Policy::default(),
            Duration::from_millis(50),
            Arc::new(|| panic!("supervisor fixture never grants admission")),
        )
        .unwrap();
    (runtime, supervisor, driver)
}
fn report() -> desktop::Report {
    desktop::Report {
        source_renewals: 0,
        viewers: crate::session_startup::shared_viewers::Statistics::default(),
    }
}
fn owner<D, N>(supervisor: &Cx, driver: &dispatch::Driver, desktop: D, listener: N) -> Run<D, N> {
    Run {
        fence: Fence {
            incoming: driver.incoming(),
            supervisor: supervisor.clone(),
        },
        completion: Arc::new(Completion::default()),
        desktop: Some(Box::pin(desktop)),
        listener: Some(Box::pin(listener)),
        result: None,
        ending: None,
    }
}
struct Dropped {
    supervisor: Cx,
    trace: Arc<Mutex<Vec<&'static str>>>,
    name: &'static str,
}
impl Drop for Dropped {
    fn drop(&mut self) {
        assert!(
            self.supervisor.is_cancel_requested(),
            "fence before destruction"
        );
        self.trace.lock().unwrap().push(self.name);
    }
}

#[test]
fn cancelled_supervisor_drains_desktop_without_polling_or_retiring_listener_early() {
    let (runtime, supervisor, driver) = fixture();
    let trace = Arc::new(Mutex::new(Vec::new()));
    let listener_owner = Dropped {
        supervisor: supervisor.clone(),
        trace: trace.clone(),
        name: "listener",
    };
    runtime.block_on(async {
        let cleanup = Cx::current().unwrap();
        let desktop = async {
            assert!(supervisor.is_cancel_requested());
            asupersync::time::sleep(cleanup.now(), Duration::from_millis(5)).await;
            assert_eq!(*trace.lock().unwrap(), [] as [&str; 0]);
            trace.lock().unwrap().push("desktop_cleanup");
            Ok(report())
        };
        let listener = async move {
            let _owned = listener_owner;
            panic!("cancelled listener cannot admit, dispatch or retire during desktop drain");
            #[allow(unreachable_code)]
            Ok::<_, LinuxError>(serial::Statistics::default())
        };
        let serving = owner(&supervisor, &driver, desktop, listener);
        let completion = serving.completion.clone();
        supervisor.cancel_fast(CancelKind::User);
        assert_eq!(serving.await, End::Cancelled);
        assert!(completion.ending.load(Ordering::Acquire));
        assert!(!cleanup.is_cancel_requested());
    });
    assert_eq!(*trace.lock().unwrap(), ["desktop_cleanup", "listener"]);
}

#[test]
fn stalled_desktop_reaches_one_fixed_drain_deadline_and_remains_uncertain() {
    let (runtime, supervisor, driver) = fixture();
    runtime.block_on(async {
        let clock = supervisor.timer_driver().unwrap();
        let started = clock.now();
        let serving = owner(
            &supervisor,
            &driver,
            pending::<Result<desktop::Report, dispatch::Error>>(),
            pending::<Result<serial::Statistics, LinuxError>>(),
        );
        supervisor.cancel_fast(CancelKind::User);
        assert_eq!(serving.await, End::DrainExpired);
        assert!(clock.now().as_nanos() >= started.as_nanos() + 2_000_000_000);
        assert!(supervisor.is_cancel_requested());
    });
}

#[test]
fn completed_listener_is_never_repolled_while_original_desktop_drains() {
    let (runtime, supervisor, driver) = fixture();
    runtime.block_on(async {
        let cleanup = Cx::current().unwrap();
        let mut listener_polls = 0;
        let desktop = async {
            poll_fn(|_| {
                if supervisor.is_cancel_requested() {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            })
            .await;
            asupersync::time::sleep(cleanup.now(), Duration::from_millis(5)).await;
            Ok(report())
        };
        let listener = poll_fn(|_| {
            listener_polls += 1;
            assert_eq!(listener_polls, 1, "never repoll completed network future");
            Poll::Ready(Ok(serial::Statistics::default()))
        });
        let end = owner(&supervisor, &driver, desktop, listener).await;
        assert_eq!(end, End::Listener(Ok(serial::Statistics::default())));
        assert!(!cleanup.is_cancel_requested());
    });
}

#[test]
fn requested_stop_does_not_hide_the_native_owners_uncertain_cleanup() {
    let (runtime, supervisor, driver) = fixture();
    runtime.block_on(async {
        let cleanup = dispatch::Error::Desktop(Box::new(desktop::Error::InputCleanup));
        let serving = owner(
            &supervisor,
            &driver,
            async { Err(cleanup.clone()) },
            pending::<Result<serial::Statistics, LinuxError>>(),
        );
        supervisor.cancel_fast(CancelKind::User);
        assert_eq!(serving.await, End::Desktop(Err(cleanup)));
    });
}

#[test]
fn unpolled_abandonment_still_drops_both_owners_immediately_after_fencing() {
    let (_runtime, supervisor, driver) = fixture();
    let trace = Arc::new(Mutex::new(Vec::new()));
    let desktop_owner = Dropped {
        supervisor: supervisor.clone(),
        trace: trace.clone(),
        name: "desktop",
    };
    let listener_owner = Dropped {
        supervisor: supervisor.clone(),
        trace: trace.clone(),
        name: "listener",
    };
    let serving = owner(
        &supervisor,
        &driver,
        async move {
            let _owned = desktop_owner;
            pending::<Result<desktop::Report, dispatch::Error>>().await
        },
        async move {
            let _owned = listener_owner;
            pending::<Result<serial::Statistics, LinuxError>>().await
        },
    );
    drop(serving);
    assert_eq!(*trace.lock().unwrap(), ["desktop", "listener"]);
}

#[test]
fn panic_during_drain_fences_and_drops_before_repoll_returns_cancelled() {
    let (runtime, supervisor, driver) = fixture();
    runtime.block_on(async {
        let desktop = async {
            panic!("explicit cleanup panic fixture");
            #[allow(unreachable_code)]
            Ok::<_, dispatch::Error>(report())
        };
        let mut serving = Box::pin(owner(
            &supervisor,
            &driver,
            desktop,
            pending::<Result<serial::Statistics, LinuxError>>(),
        ));
        supervisor.cancel_fast(CancelKind::User);
        let task = &mut Context::from_waker(std::task::Waker::noop());
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| serving.as_mut().poll(task)))
                .is_err()
        );
        assert!(serving.desktop.is_none() && serving.listener.is_none());
        assert_eq!(serving.as_mut().poll(task), Poll::Ready(End::Cancelled));
    });
}

use super::*;
use crate::session_agent::{ApprovalMode, PlatformKind};
use asupersync::{
    channel::oneshot,
    runtime::{Runtime, RuntimeBuilder},
    types::{Budget, CancelKind},
};
use fr_core::input::{DesktopPoint, InputBounds};
use std::{cell::Cell, sync::Mutex, task::Waker};

fn fixture() -> (Runtime, Cx, Arc<Shared>, SessionAgent) {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    let (send, _) = oneshot::channel();
    let shared = Arc::new(Shared {
        route: Mutex::new(Route::Cold(send)),
        os_session: 1,
        control: false,
    });
    let agent = SessionAgent::new(
        ApprovalMode::Unattended,
        PlatformKind::LinuxX11,
        1,
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
    );
    (runtime, cx, shared, agent)
}

#[test]
fn continue_and_local_stop_are_not_failures() {
    let (_runtime, cx, shared, mut agent) = fixture();
    let calls = Cell::new(0);
    let mut local = Events::new(cx, shared, |_: &mut SessionAgent, _: &mut Context<'_>| {
        calls.set(calls.get() + 1);
        Ok(if calls.get() == 1 {
            LocalAction::Continue
        } else {
            LocalAction::Stop
        })
    });
    let mut task = Context::from_waker(Waker::noop());
    assert_eq!(local.turn(&mut agent, &mut task), Ok(LocalAction::Continue));
    assert_eq!(local.turn(&mut agent, &mut task), Ok(LocalAction::Stop));
    assert_eq!(calls.get(), 2);
    assert_eq!(local.failure, None);
}

#[test]
fn source_cancellation_and_dispatcher_close_refuse_before_local_work() {
    for cancel in [true, false] {
        let (_runtime, cx, shared, mut agent) = fixture();
        let expected = if cancel {
            cx.cancel_fast(CancelKind::User);
            LocalFailure::SourceLifetimeCancelled
        } else {
            shared.close();
            LocalFailure::DispatcherClosed
        };
        let calls = Cell::new(0);
        let mut local = Events::new(
            cx.clone(),
            shared.clone(),
            |_: &mut SessionAgent, _: &mut Context<'_>| {
                calls.set(calls.get() + 1);
                Ok(LocalAction::Continue)
            },
        );
        let mut task = Context::from_waker(Waker::noop());
        assert_eq!(local.turn(&mut agent, &mut task), Err(expected));
        // Later teardown cannot replace the originally observed local cause.
        cx.cancel_fast(CancelKind::User);
        shared.close();
        assert_eq!(local.turn(&mut agent, &mut task), Err(expected));
        assert_eq!(calls.get(), 0);
    }
}

#[test]
fn poisoned_dispatcher_is_not_mistaken_for_cancellation_or_callback_failure() {
    let (_runtime, cx, shared, mut agent) = fixture();
    let poisoning = shared.clone();
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _held = poisoning.route.lock().unwrap();
            panic!("injected dispatcher poison");
        }))
        .is_err()
    );
    let mut local = Events::new(
        cx,
        shared,
        |_: &mut SessionAgent, _: &mut Context<'_>| -> Result<LocalAction, ()> {
            panic!("poisoned dispatcher must not call local work")
        },
    );
    assert_eq!(
        local.turn(&mut agent, &mut Context::from_waker(Waker::noop())),
        Err(LocalFailure::DispatcherPoisoned)
    );
}

#[test]
fn failed_local_monitor_keeps_its_cause_and_cannot_mask_uncertain_cleanup() {
    let (_runtime, cx, shared, mut agent) = fixture();
    let closing = shared.clone();
    let mut local = Events::new(
        cx,
        shared,
        move |_: &mut SessionAgent, _: &mut Context<'_>| {
            closing.close();
            Err(())
        },
    );
    assert_eq!(
        local.turn(&mut agent, &mut Context::from_waker(Waker::noop())),
        Err(LocalFailure::CallbackFailed)
    );
    assert_eq!(
        local.annotate(Error::LocalEvent),
        Error::LocalFailure {
            cause: LocalFailure::CallbackFailed,
            session: None,
        }
    );
    assert_eq!(local.annotate(Error::InputCleanup), Error::InputCleanup);
    assert_eq!(
        local.annotate(Error::GeometryChanged),
        Error::GeometryChanged
    );
}

#[test]
fn checked_lifetime_and_route_are_rechecked_after_the_callback() {
    for cancel in [true, false] {
        let (_runtime, cx, shared, mut agent) = fixture();
        let stopping = cx.clone();
        let closing = shared.clone();
        let mut local = Events::new(
            cx,
            shared,
            move |_: &mut SessionAgent, _: &mut Context<'_>| {
                if cancel {
                    stopping.cancel_fast(CancelKind::User);
                } else {
                    closing.close();
                }
                Ok(LocalAction::Continue)
            },
        );
        assert_eq!(
            local.turn(&mut agent, &mut Context::from_waker(Waker::noop())),
            Err(if cancel {
                LocalFailure::SourceLifetimeCancelled
            } else {
                LocalFailure::DispatcherClosed
            })
        );
    }
}

//! Production managed-service termination with an explicit pending drain stand-in.
//! No network, native input, or cleanup completion is inferred from the witness.
use super::*;
use asupersync::{
    cx::Cx,
    runtime::{Runtime, RuntimeBuilder},
    types::Budget,
};
use fr_core::{authority::AuthorityPolicy, ids::RemoteSessionId};
use std::{cell::Cell, future::poll_fn, task::Waker};

fn fixture() -> (Runtime, Cx, ObservationControl) {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    let mut authority = fr_core::authority::SessionAuthority::new(
        RemoteSessionId::from_raw(1),
        AuthorityPolicy::plan_defaults(),
    );
    authority.mark_capabilities_checked().unwrap();
    authority
        .authorize_observation(crate::media::host_now(&cx).unwrap())
        .unwrap();
    let control = ObservationControl::new(cx.clone(), authority).unwrap();
    (runtime, cx, control)
}

#[test]
fn terminal_witness_retains_the_result_while_the_same_service_drains() {
    for original in [
        Ok(()),
        Err(Error::Transport(fr_transport::quic::Error::Expired)),
    ] {
        let (_runtime, cx, control) = fixture();
        let polls = Cell::new(0);
        let drained = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let gate = drained.clone();
        let mut service = Service {
            control,
            inner: Some(Box::pin(poll_fn(|_| {
                polls.set(polls.get() + 1);
                Poll::Ready(original)
            }))),
            native: Arc::new(Mutex::new(Native::default())),
            shutdown_started: false,
            session: None,
            done: None,
            reporting: None,
            // A bounded drain's completion is controlled explicitly here. The
            // production end/poll/witness logic remains the object under test.
            draining: Some(Box::pin(poll_fn(move |_| {
                if gate.load(std::sync::atomic::Ordering::Acquire) {
                    Poll::Ready(Some(Ok(())))
                } else {
                    Poll::Pending
                }
            }))),
            revocation: None,
        };
        let mut task = Context::from_waker(Waker::noop());
        assert!(!service.shutdown_started());
        assert_eq!(service.terminal_session(), None);
        assert!(Pin::new(&mut service).poll(&mut task).is_pending());
        assert!(cx.is_cancel_requested(), "termination must fence first");
        assert!(service.shutdown_started());
        assert_eq!(service.terminal_session(), Some(original));
        assert!(Pin::new(&mut service).poll(&mut task).is_pending());
        assert_eq!(polls.get(), 1, "terminal session work cannot resume");
        assert_eq!(service.terminal_session(), Some(original));
        drained.store(true, std::sync::atomic::Ordering::Release);
        let Poll::Ready(report) = Pin::new(&mut service).poll(&mut task) else {
            panic!("the original drain finished");
        };
        assert_eq!(report.session, original);
        assert_eq!(report.input, None);
        assert_eq!(report.revocation, Some(Ok(())));
        assert_eq!(service.terminal_session(), Some(original));
    }
}

#[test]
fn native_stop_after_the_session_poll_fences_before_pending_io_returns() {
    let (_runtime, cx, control) = fixture();
    let cleanup = fr_core::input_submission::Cleanup {
        submitted_releases: 0,
        remaining: 0,
    };
    let shutdown = Shutdown {
        reason: StopReason::LocalRevoke,
        exit: Some(crate::input_agent::Exit {
            cleanup: Some(cleanup),
            native_clean: true,
            panicked: false,
        }),
        last_cleanup: Some(cleanup),
    };
    let native = Arc::new(Mutex::new(Native::default()));
    let finished = native.clone();
    let polls = Cell::new(0);
    let can_return = Cell::new(false);
    let original = Err(Error::Transport(fr_transport::quic::Error::Expired));
    let mut service = Service {
        control,
        inner: Some(Box::pin(poll_fn(|_| {
            polls.set(polls.get() + 1);
            if polls.get() == 1 {
                // Explicit native-completion fixture: it becomes visible only
                // after the first native_stopped check in this same poll.
                lock(&finished).report = Some(shutdown);
            }
            if can_return.get() {
                Poll::Ready(original)
            } else {
                Poll::Pending
            }
        }))),
        native,
        shutdown_started: false,
        session: None,
        done: None,
        // A real, unarmed reporting owner exercises the cooperative-I/O path
        // without inventing a granted lease or delivered revocation report.
        reporting: Some(fr_transport::quic::RevocationReport::default()),
        draining: None,
        revocation: None,
    };
    let mut task = Context::from_waker(Waker::noop());
    assert!(!service.shutdown_started());
    assert_eq!(service.terminal_session(), None);
    assert!(Pin::new(&mut service).poll(&mut task).is_pending());
    assert!(cx.is_cancel_requested());
    assert!(service.shutdown_started());
    assert_eq!(
        service.terminal_session(),
        None,
        "original I/O is still pending"
    );
    assert!(
        service.inner.is_some(),
        "do not abandon the original operation"
    );
    assert!(Pin::new(&mut service).poll(&mut task).is_pending());
    assert_eq!(polls.get(), 2, "the same operation keeps draining");
    assert!(service.shutdown_started());
    assert_eq!(service.terminal_session(), None);
    can_return.set(true);
    let Poll::Ready(report) = Pin::new(&mut service).poll(&mut task) else {
        panic!("the original operation returned");
    };
    assert_eq!(report.session, original);
    assert_eq!(report.input, Some(shutdown));
    assert_eq!(
        report.revocation, None,
        "no registered lease, no delivery claim"
    );
    assert!(service.shutdown_started());
    assert_eq!(service.terminal_session(), Some(original));
}

use super::*;

#[test]
fn incomplete_cleanup_dominates_success_and_cancellation() {
    for result in [
        Ok(()),
        Err(reconnect::Failure::Cancelled),
        Err(reconnect::Failure::Cleanup),
        Err(reconnect::Failure::CleanupExpired),
    ] {
        for stopped in [false, true] {
            let error = completion_gate(result, false, stopped, false).unwrap_err();
            assert_eq!(error.code, "native_cleanup_incomplete");
        }
    }
    let error = completion_gate(
        Err(reconnect::Failure::Cleanup),
        true,
        false,
        false,
    )
    .unwrap_err();
    assert_eq!(error.code, "native_cleanup_incomplete");
}

#[test]
fn cancellation_and_failures_cannot_be_relabelled_as_observation_success() {
    assert!(completion_gate(Ok(()), true, false, false).is_ok());
    let error = completion_gate(Ok(()), true, true, false).unwrap_err();
    assert_eq!(error.code, "cancelled");
    assert_eq!(error.exit, 130);
    let error = completion_gate(Ok(()), true, false, true).unwrap_err();
    assert_eq!(error.code, "display_not_offered");
    let error = completion_gate(
        Err(reconnect::Failure::Notification),
        true,
        false,
        false,
    )
    .unwrap_err();
    assert_eq!(error.code, "native_observation_failed");
}

#[test]
fn signal_fences_the_supervisor_but_keeps_polling_its_original_cleanup() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let cx = runtime
        .handle()
        .try_request_cx_with_budget(Budget::INFINITE)
        .unwrap();
    let stopped = Cell::new(false);
    let mut polls = 0;
    let operation = poll_fn(|task| {
        assert!(cx.is_cancel_requested());
        polls += 1;
        if polls == 1 {
            task.waker().wake_by_ref();
            Poll::Pending
        } else {
            Poll::Ready(Err::<(), _>(reconnect::Failure::Cleanup))
        }
    });
    let result = runtime.block_on(drain_on_shutdown(&cx, &stopped, operation, async {}));
    assert_eq!(result, Err(reconnect::Failure::Cleanup));
    assert!(stopped.get());
    assert_eq!(polls, 2);
}

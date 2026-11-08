//! Shared CLI runtime and retained cancellation for installed-authority reads.
use super::Failure;
use asupersync::{
    cx::Cx,
    runtime::{Runtime, RuntimeBuilder},
    signal::{Signal, SignalKind, signal},
    types::{Budget, CancelKind},
};
use std::{
    cell::Cell,
    future::{Future, poll_fn},
    pin::pin,
    task::Poll,
};

fn failure(code: &'static str, next: &'static str) -> Failure {
    Failure::new(code, next, 1)
}
pub(super) fn create() -> Result<(Runtime, Cx), Failure> {
    let runtime = RuntimeBuilder::current_thread()
        .enable_platform_reactor(true)
        .build()
        .map_err(|_| {
            failure(
                "runtime_unavailable",
                "Check local process/thread resources.",
            )
        })?;
    let cx = runtime
        .handle()
        .try_request_cx_with_budget(Budget::INFINITE)
        .map_err(|_| failure("runtime_unavailable", "Check local runtime resources."))?;
    Ok((runtime, cx))
}
pub(super) struct Shutdown {
    interrupt: Signal,
    terminate: Signal,
    hangup: Signal,
}
impl Shutdown {
    pub(super) fn new() -> Result<Self, Failure> {
        let open = |kind| {
            signal(kind).map_err(|_| {
                failure(
                    "signal_unavailable",
                    "Resolve local signal registration before starting a session.",
                )
            })
        };
        Ok(Self {
            interrupt: open(SignalKind::interrupt())?,
            terminate: open(SignalKind::terminate())?,
            hangup: open(SignalKind::hangup())?,
        })
    }
    pub(super) async fn run<T>(
        &mut self,
        cx: &Cx,
        stopped: &Cell<bool>,
        operation: impl Future<Output = T>,
    ) -> T {
        await_shutdown(cx, stopped, operation, self.requested()).await
    }
    pub(super) async fn requested(&mut self) {
        let mut interrupt = pin!(self.interrupt.recv());
        let mut terminate = pin!(self.terminate.recv());
        let mut hangup = pin!(self.hangup.recv());
        poll_fn(|task| {
            if interrupt.as_mut().poll(task).is_ready()
                || terminate.as_mut().poll(task).is_ready()
                || hangup.as_mut().poll(task).is_ready()
            {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
    }
}
/// Signal readiness cancels the supervisor but NEVER drops the reconnect future:
/// its independent bounded cleanup still runs and its actual result is retained.
pub(super) async fn await_shutdown<T>(
    cx: &Cx,
    stopped: &Cell<bool>,
    operation: impl Future<Output = T>,
    shutdown: impl Future<Output = ()>,
) -> T {
    let mut shutdown = pin!(shutdown);
    let mut operation = pin!(operation);
    poll_fn(|task| {
        if !stopped.get() && shutdown.as_mut().poll(task).is_ready() {
            stopped.set(true);
            cx.cancel_fast(CancelKind::User);
        }
        operation.as_mut().poll(task)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn signal_cancels_without_abandoning_the_original_cleanup_future() {
        let runtime = RuntimeBuilder::current_thread()
            .enable_platform_reactor(true)
            .build()
            .unwrap();
        let cx = runtime.request_cx_with_budget(Budget::INFINITE);
        let stopped = Cell::new(false);
        let collected = Cell::new(false);
        let result = runtime.block_on(await_shutdown(
            &cx,
            &stopped,
            async {
                assert!(cx.is_cancel_requested());
                asupersync::time::sleep_until(
                    cx.timer_driver()
                        .unwrap()
                        .now()
                        .saturating_add_nanos(1_000_000),
                )
                .await;
                collected.set(true);
                42
            },
            async {},
        ));
        assert_eq!(result, 42);
        assert!(stopped.get() && collected.get());
    }
}

//! Original hosting resources retire only after their dedicated scope is fenced.
use super::Error;
use asupersync::{cx::Cx, types::CancelKind};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

// Fence BEFORE eagerly releasing pending native work, including on caught panic
// and when callers retain a completed future. Firewall deletion remains explicit.
pub(super) struct Serving<F> {
    pub(super) session: Cx,
    pub(super) inner: Option<Pin<Box<F>>>,
}
impl<F> Serving<F> {
    fn finish(&mut self) {
        self.session.cancel_fast(CancelKind::User);
        drop(self.inner.take());
    }
}
impl<T, F: Future<Output = Result<T, Error>>> Future for Serving<F> {
    type Output = Result<T, Error>;
    fn poll(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<Self::Output> {
        let mut turn = Turn {
            owner: self.get_mut(),
            complete: false,
        };
        let result = match turn.owner.inner.as_mut() {
            Some(inner) => inner.as_mut().poll(task),
            None => Poll::Ready(Err(Error::Spent)),
        };
        if result.is_ready() {
            turn.owner.finish();
        }
        turn.complete = true;
        result
    }
}
struct Turn<'a, F> {
    owner: &'a mut Serving<F>,
    complete: bool,
}
impl<F> Drop for Turn<'_, F> {
    fn drop(&mut self) {
        if !self.complete {
            self.owner.finish();
        }
    }
}
impl<F> Drop for Serving<F> {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::{runtime::RuntimeBuilder, types::Budget};
    use std::future::poll_fn;

    fn run(test: impl FnOnce(Cx, Cx)) {
        let rt = RuntimeBuilder::current_thread()
            .enable_platform_reactor(true)
            .build()
            .unwrap();
        let broker = rt
            .handle()
            .try_request_cx_with_budget(Budget::INFINITE)
            .unwrap();
        let session = rt
            .handle()
            .try_request_cx_with_budget(Budget::INFINITE)
            .unwrap();
        rt.block_on(async {
            test(broker, session);
        });
    }
    #[test]
    fn unpolled_protected_service_drop_fences_only_its_session() {
        run(|broker, session| {
            drop(Serving {
                session: session.clone(),
                inner: Some(Box::pin(std::future::pending::<Result<(), Error>>())),
            });
            assert!(session.is_cancel_requested());
            assert!(!broker.is_cancel_requested());
        });
    }
    #[test]
    fn ingress_failure_fences_while_completed_future_is_retained() {
        run(|broker, session| {
            let failure = Error::Ingress(fr_tailnet::ingress::Error::FirewallMismatch);
            let mut serving = Box::pin(Serving {
                session: session.clone(),
                inner: Some(Box::pin(std::future::ready(Err::<(), _>(failure)))),
            });
            let wake = std::task::Waker::noop();
            assert_eq!(
                serving.as_mut().poll(&mut Context::from_waker(wake)),
                Poll::Ready(Err(failure))
            );
            assert!(session.is_cancel_requested());
            assert!(!broker.is_cancel_requested());
            drop(serving);
        });
    }
    #[test]
    fn completion_keeps_the_actual_application_result_and_fences() {
        run(|broker, session| {
            let mut serving = Box::pin(Serving {
                session: session.clone(),
                inner: Some(Box::pin(std::future::ready(Ok(17)))),
            });
            assert_eq!(
                serving
                    .as_mut()
                    .poll(&mut Context::from_waker(std::task::Waker::noop())),
                Poll::Ready(Ok(17))
            );
            assert!(session.is_cancel_requested());
            assert!(!broker.is_cancel_requested());
        });
    }
    #[test]
    fn caught_protected_application_panic_does_not_retain_authority() {
        run(|broker, session| {
            let mut serving = Box::pin(Serving {
                session: session.clone(),
                inner: Some(Box::pin(poll_fn(|_| -> Poll<Result<(), Error>> {
                    panic!("application fixture")
                }))),
            });
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    serving
                        .as_mut()
                        .poll(&mut Context::from_waker(std::task::Waker::noop()))
                }))
                .is_err()
            );
            assert!(session.is_cancel_requested());
            assert!(!broker.is_cancel_requested());
            drop(serving);
        });
    }

    struct PendingWork {
        cx: Cx,
        dropped: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }
    impl Drop for PendingWork {
        fn drop(&mut self) {
            assert!(
                self.cx.is_cancel_requested(),
                "fence before native retirement"
            );
            self.dropped
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
    fn work(cx: &Cx) -> (PendingWork, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        (
            PendingWork {
                cx: cx.clone(),
                dropped: dropped.clone(),
            },
            dropped,
        )
    }
    #[test]
    fn serial_outcomes_release_work_even_while_terminal_future_is_retained() {
        use crate::native_connection::host::serial;
        for outcome in [
            Ok(serial::Statistics {
                attempts: 2,
                admitted: 1,
                refused: 1,
            }),
            Err(Error::Serial(serial::Error::RetainedTransport)),
        ] {
            run(|broker, supervisor| {
                let (work, dropped) = work(&supervisor);
                let mut service = Box::pin(Serving {
                    session: supervisor.clone(),
                    inner: Some(Box::pin(poll_fn(move |_| {
                        let _retained = &work;
                        Poll::Ready(outcome)
                    }))),
                });
                let mut task = Context::from_waker(std::task::Waker::noop());
                assert_eq!(service.as_mut().poll(&mut task), Poll::Ready(outcome));
                assert_eq!(dropped.load(std::sync::atomic::Ordering::SeqCst), 1);
                assert!(supervisor.is_cancel_requested());
                assert!(!broker.is_cancel_requested());
                assert_eq!(
                    service.as_mut().poll(&mut task),
                    Poll::Ready(Err(Error::Spent))
                );
                drop(service);
                assert_eq!(dropped.load(std::sync::atomic::Ordering::SeqCst), 1);
            });
        }
    }
    #[test]
    fn caught_panic_eagerly_retires_original_work_and_cannot_poll_it_again() {
        run(|broker, supervisor| {
            let (work, dropped) = work(&supervisor);
            let mut service = Box::pin(Serving {
                session: supervisor.clone(),
                inner: Some(Box::pin(poll_fn(move |_| -> Poll<Result<(), Error>> {
                    let _retained = &work;
                    panic!("local application panic");
                }))),
            });
            let mut task = Context::from_waker(std::task::Waker::noop());
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    service.as_mut().poll(&mut task)
                }))
                .is_err()
            );
            assert_eq!(dropped.load(std::sync::atomic::Ordering::SeqCst), 1);
            assert!(!broker.is_cancel_requested());
            assert_eq!(
                service.as_mut().poll(&mut task),
                Poll::Ready(Err(Error::Spent))
            );
        });
    }
    #[test]
    fn unpolled_service_cancels_before_destroying_original_resources() {
        run(|broker, supervisor| {
            let (work, dropped) = work(&supervisor);
            drop(Serving {
                session: supervisor.clone(),
                inner: Some(Box::pin(poll_fn(move |_| -> Poll<Result<(), Error>> {
                    let _retained = &work;
                    panic!("unpolled service must not run");
                }))),
            });
            assert_eq!(dropped.load(std::sync::atomic::Ordering::SeqCst), 1);
            assert!(!broker.is_cancel_requested());
        });
    }
    #[test]
    fn pending_service_keeps_resources_and_independent_contexts_live() {
        run(|broker, supervisor| {
            let (work, dropped) = work(&supervisor);
            let mut service = Box::pin(Serving {
                session: supervisor.clone(),
                inner: Some(Box::pin(poll_fn(move |_| -> Poll<Result<(), Error>> {
                    let _retained = &work;
                    Poll::Pending
                }))),
            });
            assert!(
                service
                    .as_mut()
                    .poll(&mut Context::from_waker(std::task::Waker::noop()))
                    .is_pending()
            );
            assert_eq!(dropped.load(std::sync::atomic::Ordering::SeqCst), 0);
            assert!(!supervisor.is_cancel_requested());
            assert!(!broker.is_cancel_requested());
            drop(service);
            assert_eq!(dropped.load(std::sync::atomic::Ordering::SeqCst), 1);
            assert!(!broker.is_cancel_requested());
        });
    }
}

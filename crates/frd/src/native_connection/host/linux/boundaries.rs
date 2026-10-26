//! The fixed ingress set for one capacity-one, multi-address native listener.
use super::super::IngressCheck;
use asupersync::cx::Cx;
use fr_tailnet::ingress;
use std::{
    future::{Future, pending, poll_fn},
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::Poll,
};

pub(super) struct Boundaries(pub(super) Vec<ingress::Boundary>);

// Keep the whole ingress fence ahead of application destruction on EVERY exit,
// including an unpolled future drop or a caught panic during a poll. An async
// block's capture order is not part of this ownership contract.
struct Protected<F, W> {
    watches: Vec<Pin<Box<W>>>,
    application: Pin<Box<F>>,
}

impl<F, W> Drop for Protected<F, W> {
    fn drop(&mut self) {
        self.watches.clear();
    }
}

impl Boundaries {
    pub(super) fn addresses(&self) -> Vec<SocketAddr> {
        self.0.iter().map(ingress::Boundary::address).collect()
    }

    pub(super) fn check(&self) -> Result<IngressCheck, ingress::Error> {
        let leases: Vec<_> = self
            .0
            .iter()
            .map(|boundary| Ok((boundary.address(), boundary.lease()?)))
            .collect::<Result<_, ingress::Error>>()?;
        Ok(Arc::new(move |address| {
            leases
                .iter()
                .find(|(bound, _)| *bound == address)
                .is_some_and(|(_, lease)| lease.check(address).is_ok())
        }))
    }

    pub(super) fn close(&self) {
        for boundary in &self.0 {
            boundary.close();
        }
    }

    /// Every original boundary keeps its existing refresh schedule and final
    /// checks. These watchdogs supervise ONE pinned application; they do not run that
    /// application once per address. A failed/abandoned layer fences the whole
    /// set before pending work is dropped, including unselected addresses.
    pub(super) fn supervise<'a, F: Future + 'a>(
        &'a mut self,
        operation: F,
    ) -> impl Future<Output = Result<F::Output, ingress::Error>> + 'a {
        let leases: Result<Vec<_>, _> = self
            .0
            .iter()
            .map(|boundary| Ok((boundary.address(), boundary.lease()?)))
            .collect::<Result<_, ingress::Error>>();
        // Keep these concrete futures rather than type-erasing the application:
        // callers retain its Send property when their original future is Send.
        let watches: Vec<_> = self
            .0
            .iter_mut()
            .map(|boundary| Box::pin(boundary.supervise(pending::<()>())))
            .collect();
        let mut operation = Protected {
            watches,
            application: Box::pin(operation),
        };
        async move {
            let result = match leases {
                Err(error) => Err(error),
                Ok(leases) => {
                    poll_fn(|task| {
                        for watch in &mut operation.watches {
                            if let Poll::Ready(result) = watch.as_mut().poll(task) {
                                return Poll::Ready(Err(result
                                    .err()
                                    .unwrap_or(ingress::Error::Closed)));
                            }
                        }
                        for (address, lease) in &leases {
                            lease.check(*address)?;
                        }
                        let output = operation.application.as_mut().poll(task);
                        for (address, lease) in &leases {
                            lease.check(*address)?;
                        }
                        output.map(Ok)
                    })
                    .await
                }
            };
            // Fence the complete protection set before releasing work, even
            // when a caller retains this already-completed future.
            drop(operation);
            result
        }
    }

    /// Fence all addresses at CALL time. Failure on one cleanup cannot skip
    /// cleanup of another. Original transports must release ALL their retained
    /// leases before any rule is removed; a failed removal remains retryable.
    pub(super) fn stop<'a>(
        &'a mut self,
        cleanup: &'a Cx,
    ) -> impl Future<Output = Result<(), ingress::Error>> + 'a {
        self.close();
        async move {
            let mut failure = None;
            for boundary in &mut self.0 {
                if let Err(error) = boundary.stop(cleanup).await {
                    failure.get_or_insert(error);
                }
            }
            failure.map_or(Ok(()), Err)
        }
    }
}

impl Drop for Boundaries {
    fn drop(&mut self) {
        self.close();
    }
}

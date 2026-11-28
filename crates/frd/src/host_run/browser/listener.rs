//! Protect every exact TCP address before bind, then retain the complete
//! boundary set until all original transports and capture receipts are gone.
use super::{ContextGuard, Gate, connection, context};
use crate::{
    browser_ingress::BrowserIngress,
    host_run::{Error, Event, LocalAction, Options, Reporter, approval},
    native_connection::host::{Boundaries, IngressCheck},
};
use asupersync::{cx::Cx, net::TcpListener, runtime::RuntimeHandle, time::sleep};
use fr_tailnet::{LocalApi, NativeServerIdentity, NodeIdentity, ingress};
use std::{
    future::{Future, poll_fn},
    pin::Pin,
    sync::Arc,
    task::Poll,
    time::Duration,
};

const MAX_CONNECTIONS: usize = 8;

/// A field-order fence: cancel the original socket before dropping its work.
struct Connection {
    _context: ContextGuard,
    work: Pin<Box<dyn Future<Output = ()>>>,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn serve(
    options: &Options,
    port: u16,
    report: &Reporter,
    runtime: &RuntimeHandle,
    api: &LocalApi,
    identity: &NativeServerIdentity,
    node: NodeIdentity,
    browser: Arc<BrowserIngress>,
    gate: Gate,
    broker: &Cx,
    cleanup: &Cx,
    consent: &mut approval::Owner,
) -> Result<(), Error> {
    let url = format!("https://{}:{port}", node.certificate_name());
    let mut boundaries = Boundaries(Vec::new());
    let installed = install(
        options,
        port,
        api,
        identity,
        node,
        &gate,
        broker,
        &mut boundaries,
    )
    .await;
    let outcome = match installed {
        Ok(listeners) => {
            for address in boundaries.addresses() {
                report(Event::BrowserListening {
                    address,
                    url: url.clone(),
                });
            }
            match boundaries.check() {
                Ok(check) => {
                    catch_serving(
                        boundaries.supervise(accept(
                            listeners,
                            runtime,
                            api,
                            identity,
                            browser.clone(),
                            &gate,
                            check,
                            cleanup,
                            consent,
                        )),
                        &browser,
                    )
                    .await
                }
                Err(error) => Err(Error::Ingress(error)),
            }
        }
        Err(error) => Err(error),
    };
    // The protected future has released all socket owners. The capture child
    // receipt lives in browser independently of that future and must be reaped.
    consent.close();
    browser.close_desktop();
    let capture = browser.retire_desktop(cleanup).await;
    drop(browser);
    let firewall = boundaries.stop(cleanup).await;
    if capture.is_err() {
        report(Event::CleanupFailed {
            stage: "browser_capture",
        });
        return Err(Error::Cleanup("browser_capture"));
    }
    firewall.map_err(|_| Error::Cleanup("browser_ingress"))?;
    match outcome {
        Err(Error::BrowserTransport("stopped")) if gate.stopped() => Ok(()),
        outcome => outcome,
    }
}

/// Never resume a panicked socket/supervisor future. Fence it, drop its
/// original sockets and reaper guard, then let the enclosing owner execute
/// the same retained capture/firewall cleanup as any other terminal result.
/// This protects unwinding builds. The workspace's panic=abort release profile
/// terminates the process and does not provide async panic-cleanup evidence.
async fn catch_serving(
    work: impl Future<Output = Result<Result<(), Error>, ingress::Error>>,
    browser: &BrowserIngress,
) -> Result<(), Error> {
    let mut work = Box::pin(work);
    let result = poll_fn(|task| {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work.as_mut().poll(task))) {
            Ok(output) => output.map(|result| {
                result
                    .map_err(Error::Ingress)
                    .and_then(std::convert::identity)
            }),
            Err(_) => {
                browser.close_desktop();
                Poll::Ready(Err(Error::BrowserTransport("panic")))
            }
        }
    })
    .await;
    drop(work);
    result
}

#[allow(clippy::too_many_arguments)]
async fn install(
    options: &Options,
    port: u16,
    api: &LocalApi,
    identity: &NativeServerIdentity,
    node: NodeIdentity,
    gate: &Gate,
    broker: &Cx,
    boundaries: &mut Boundaries,
) -> Result<Vec<TcpListener>, Error> {
    gate.check()?;
    // The certificate and planned listener must name the same original
    // installed node, even if a replacement daemon reuses its IP addresses.
    identity
        .check_bind_node(broker, &node)
        .map_err(Error::Tailnet)?;
    let addresses: Vec<_> = node
        .addresses()
        .iter()
        .map(|ip| std::net::SocketAddr::new(*ip, port))
        .collect();
    if addresses.is_empty() || addresses.len() > fr_transport::native_accept::MAX_ADDRESSES {
        return Err(Error::NoTailnetAddress);
    }
    // Preserve the first snapshot while acquiring a same-node sibling for each
    // rule. Matching IP addresses alone cannot authorize another daemon/node.
    let mut siblings = Vec::with_capacity(addresses.len() - 1);
    for _ in 1..addresses.len() {
        gate.check()?;
        siblings.push(
            api.revalidate_node(broker, &node)
                .await
                .map_err(Error::Tailnet)?,
        );
        gate.check()?;
    }
    let enforcement =
        ingress::Enforcement::detect(std::path::Path::new(ingress::helper::DEFAULT_SOCKET));
    for (&address, node) in addresses.iter().zip(std::iter::once(node).chain(siblings)) {
        gate.check()?;
        let mut config = ingress::Configuration::new(address, &options.interface)
            .map_err(Error::Ingress)?
            .protocols(ingress::Protocols::TCP)
            .enforcement(enforcement.clone())
            .map_err(Error::Ingress)?;
        if let Some((nft, ip)) = &options.ingress_tools {
            config = config.executables(nft, ip).map_err(Error::Ingress)?;
        }
        boundaries.0.push(
            ingress::Boundary::install(broker, api.clone(), node, config)
                .await
                .map_err(Error::Ingress)?,
        );
        gate.check()?;
    }
    let check = boundaries.check().map_err(Error::Ingress)?;
    let mut listeners = Vec::with_capacity(addresses.len());
    for address in addresses {
        gate.check()?;
        if !check(address) {
            return Err(Error::Ingress(ingress::Error::Closed));
        }
        let listener = TcpListener::bind(address)
            .await
            .map_err(|_| Error::BrowserTransport("bind"))?;
        let actual = listener
            .local_addr()
            .map_err(|_| Error::BrowserTransport("address"))?;
        gate.check()?;
        if actual != address || !check(actual) {
            return Err(Error::Ingress(ingress::Error::Closed));
        }
        identity.status(broker).map_err(Error::Tailnet)?;
        listeners.push(listener);
    }
    Ok(listeners)
}

#[allow(clippy::too_many_arguments)]
async fn accept(
    listeners: Vec<TcpListener>,
    runtime: &RuntimeHandle,
    api: &LocalApi,
    identity: &NativeServerIdentity,
    browser: Arc<BrowserIngress>,
    gate: &Gate,
    ingress: IngressCheck,
    cleanup: &Cx,
    consent: &mut approval::Owner,
) -> Result<(), Error> {
    let mut sockets: Vec<Connection> = Vec::with_capacity(MAX_CONNECTIONS);
    // The control socket's own admission ends as soon as it revokes its
    // observation. Keep reaping outside that socket, while the video owner
    // continues to receive polls and releases its original capture receipt.
    let mut maintenance: Option<Pin<Box<dyn Future<Output = std::io::Result<()>>>>> = None;
    let mut cursor = 0usize;
    let mut listener_cursor = 0usize;
    let mut pulse = Box::pin(sleep(gate.cx.now(), Duration::from_millis(20)));
    poll_fn(|task| {
        gate.register(task);
        if gate.stopped() {
            consent.close();
            browser.close_desktop();
            return Poll::Ready(Ok(()));
        }
        gate.check()?;
        if consent.turn(LocalAction::Continue) == LocalAction::Stop {
            return Poll::Ready(Err(Error::Cleanup("approval")));
        }
        gate.check()?;
        if maintenance.is_none() && browser.has_closed_desktop() {
            let original = browser.clone();
            let cleanup = cleanup.clone();
            maintenance = Some(Box::pin(async move {
                original.retire_closed_desktop(&cleanup).await
            }));
        }
        if let Some(reap) = maintenance.as_mut()
            && let Poll::Ready(result) = reap.as_mut().poll(task)
        {
            maintenance = None;
            if result.is_err() {
                return Poll::Ready(Err(Error::Cleanup("browser_capture")));
            }
        }
        // At most eight socket polls and one new accept per turn; rotate the
        // first socket so a ready video writer cannot starve control renewal.
        let count = sockets.len();
        let mut finished = Vec::with_capacity(count);
        for offset in 0..count {
            let index = (cursor + offset) % count;
            gate.check()?;
            if sockets[index].work.as_mut().poll(task).is_ready() {
                finished.push(index);
            }
            gate.check()?;
        }
        finished.sort_unstable();
        for index in finished.into_iter().rev() {
            drop(sockets.remove(index));
        }
        cursor = (cursor + 1) % sockets.len().max(1);
        if sockets.len() < MAX_CONNECTIONS {
            for offset in 0..listeners.len() {
                let index = (listener_cursor + offset) % listeners.len();
                match listeners[index].poll_accept(task) {
                    Poll::Ready(Ok((stream, _))) => {
                        gate.check()?;
                        let cx = context(runtime)?;
                        let connection = connection::serve(
                            stream,
                            cx.clone(),
                            api.clone(),
                            identity.clone(),
                            browser.clone(),
                            gate.clone(),
                            ingress.clone(),
                        );
                        sockets.push(Connection {
                            _context: ContextGuard(cx),
                            work: Box::pin(async move {
                                // A rejected or disconnected peer ends only its
                                // original transport; source retirement is held
                                // separately by BrowserIngress.
                                let _ = connection.await;
                            }),
                        });
                        listener_cursor = (index + 1) % listeners.len();
                        task.waker().wake_by_ref();
                        break;
                    }
                    Poll::Ready(Err(_)) => {
                        return Poll::Ready(Err(Error::BrowserTransport("accept")));
                    }
                    Poll::Pending => {}
                }
            }
        }
        gate.check()?;
        if pulse.as_mut().poll(task).is_ready() {
            pulse = Box::pin(sleep(gate.cx.now(), Duration::from_millis(20)));
            // Poll a fresh timer next turn; never discard an already-ready
            // replacement and then poll that completed timer again.
            task.waker().wake_by_ref();
        }
        Poll::Pending
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser_ingress::BrowserIngressConfig;
    use fr_core::{
        ids::{HostBootId, OsSessionId},
        limits::ProtocolLimits,
    };
    use std::sync::atomic::{AtomicBool, Ordering};

    struct PanickedSocket(Arc<AtomicBool>);
    impl Future for PanickedSocket {
        type Output = Result<Result<(), Error>, ingress::Error>;
        fn poll(self: Pin<&mut Self>, _: &mut std::task::Context<'_>) -> Poll<Self::Output> {
            panic!("original socket failed while polled");
        }
    }
    impl Drop for PanickedSocket {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    #[test]
    fn panicked_socket_is_dropped_before_the_caller_can_begin_retirement() {
        let browser = BrowserIngress::new(BrowserIngressConfig::https_tailnet(
            "host.example.ts.net",
            8444,
            HostBootId::from_raw(1),
            OsSessionId::from_raw(2),
            ProtocolLimits::ABSOLUTE,
        ));
        let dropped = Arc::new(AtomicBool::new(false));
        let mut work = Box::pin(catch_serving(PanickedSocket(dropped.clone()), &browser));
        let mut task = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(matches!(
            work.as_mut().poll(&mut task),
            Poll::Ready(Err(Error::BrowserTransport("panic"))),
        ));
        assert!(dropped.load(Ordering::Acquire));
    }
}

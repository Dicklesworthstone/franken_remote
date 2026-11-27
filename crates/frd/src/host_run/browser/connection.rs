//! One actual TCP endpoint, one original admission and one refresh schedule.
//! HTTP headers, IP labels and auxiliary tickets cannot replace this owner.
use super::Gate;
use crate::{
    browser_ingress::BrowserIngress, media::host_now, native_connection::host::IngressCheck,
};
use asupersync::{cx::Cx, net::TcpStream, time::sleep};
use fr_tailnet::{
    Admission, ConnectionAddresses, GrantPolicy, LocalApi, NativeServerIdentity, Scope,
};
use std::{
    future::{Future, poll_fn},
    io,
    pin::pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::Poll,
    time::Duration,
};

const OPEN_BUDGET_US: u64 = 3_000_000;

fn refused() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "original browser admission ended",
    )
}
fn current(cx: &Cx) -> io::Result<u64> {
    cx.checkpoint().map_err(|_| refused())?;
    host_now(cx)
        .map(|time| time.as_micros())
        .map_err(|_| refused())
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn serve(
    stream: TcpStream,
    cx: Cx,
    api: LocalApi,
    identity: NativeServerIdentity,
    browser: Arc<BrowserIngress>,
    gate: Gate,
    ingress: IngressCheck,
) -> io::Result<()> {
    let addresses = ConnectionAddresses {
        local: stream.local_addr()?,
        peer: stream.peer_addr()?,
    };
    let until = current(&cx)?
        .checked_add(OPEN_BUDGET_US)
        .ok_or_else(refused)?;
    let opening = AtomicBool::new(true);
    let operation = async {
        let local = gate.policy.check().map_err(|_| refused())?;
        let policy = GrantPolicy {
            scope: match local.sharing_scope {
                crate::host_policy::Sharing::OwnUser => Scope::OwnUser,
                crate::host_policy::Sharing::Tailnet => Scope::Tailnet,
            },
            ..GrantPolicy::default()
        };
        // A bounded busy wait is allowed only BEFORE any authority is issued.
        // Renewal keeps the original proof and never uses this retry path.
        let proof = loop {
            if current(&cx)? >= until {
                return Err(refused());
            }
            match api.authorize_membership(&cx, addresses, policy).await {
                Ok(proof) => break proof,
                Err(fr_tailnet::Error::Busy) => {
                    sleep(cx.now(), Duration::from_millis(5)).await;
                }
                Err(_) => return Err(refused()),
            }
        };
        if current(&cx)? >= until {
            return Err(refused());
        }
        let admission = Admission::new(api.clone(), cx.clone(), proof).map_err(|_| refused())?;
        let peer = admission.lease();
        let application = async {
            // Each accepted TCP socket needs fresh self evidence. Never reuse
            // the snapshot that protected the listener several minutes ago.
            let node = loop {
                if current(&cx)? >= until {
                    return Err(refused());
                }
                match api.node_identity(&cx).await {
                    Ok(node) => break node,
                    Err(fr_tailnet::Error::Busy) => {
                        sleep(cx.now(), Duration::from_millis(5)).await;
                    }
                    Err(_) => return Err(refused()),
                }
            };
            let tls = identity
                .https_accept(&cx, &node, stream)
                .await
                .map_err(|_| refused())?;
            if current(&cx)? >= until {
                return Err(refused());
            }
            opening.store(false, Ordering::Release);
            let sni = tls.sni_hostname().map(str::to_owned);
            // The stream and this lease name the same established local/peer
            // endpoints. The ingress requires same_peer for auxiliary attach.
            browser
                .serve_verified_stream(tls, peer.clone(), cx.clone(), sni.as_deref(), || {
                    host_now(&cx).unwrap_or(fr_core::time::HostInstant::from_micros(u64::MAX))
                })
                .await
        };
        with_refresh(admission, application).await
    };
    let mut operation = pin!(operation);
    let mut pulse = Box::pin(sleep(cx.now(), Duration::from_millis(20)));
    poll_fn(|task| {
        gate.register(task);
        gate.check().map_err(|_| refused())?;
        let now = current(&cx)?;
        if opening.load(Ordering::Acquire) && now >= until {
            return Poll::Ready(Err(refused()));
        }
        if !ingress(addresses.local) {
            return Poll::Ready(Err(refused()));
        }
        let output = operation.as_mut().poll(task);
        gate.check().map_err(|_| refused())?;
        let now = current(&cx)?;
        if opening.load(Ordering::Acquire) && now >= until {
            return Poll::Ready(Err(refused()));
        }
        if !ingress(addresses.local) {
            return Poll::Ready(Err(refused()));
        }
        if pulse.as_mut().poll(task).is_ready() {
            pulse = Box::pin(sleep(cx.now(), Duration::from_millis(20)));
            task.waker().wake_by_ref();
        }
        output
    })
    .await
}

/// Start refreshing halfway through the ORIGINAL remaining validity. A delayed
/// response consumes that validity; it never slides the old proof's deadline.
fn refresh_delay(now_us: u64, until_us: u64) -> io::Result<Duration> {
    let remaining = until_us
        .checked_sub(now_us)
        .filter(|value| *value > 0)
        .ok_or_else(refused)?;
    Ok(Duration::from_micros((remaining / 2).max(1)))
}

async fn with_refresh<T>(
    mut admission: Admission,
    work: impl Future<Output = io::Result<T>>,
) -> io::Result<T> {
    let peer = admission.lease();
    let cx = admission.context();
    let renewal = async {
        loop {
            let until = admission.lease().observe().map_err(|_| refused())?;
            sleep(cx.now(), refresh_delay(current(&cx)?, until)?).await;
            // Dropping a pending refresh revokes the original lease.
            admission.refresh().await.map_err(|_| refused())?;
        }
        #[allow(unreachable_code)]
        Ok::<(), io::Error>(())
    };
    let (mut renewal, mut work) = (pin!(renewal), pin!(work));
    let mut pulse = Box::pin(sleep(cx.now(), Duration::from_millis(20)));
    poll_fn(|task| {
        peer.observe().map_err(|_| refused())?;
        if let Poll::Ready(result) = renewal.as_mut().poll(task) {
            return Poll::Ready(Err(result.err().unwrap_or_else(refused)));
        }
        peer.observe().map_err(|_| refused())?;
        let output = work.as_mut().poll(task);
        peer.observe().map_err(|_| refused())?;
        if pulse.as_mut().poll(task).is_ready() {
            pulse = Box::pin(sleep(cx.now(), Duration::from_millis(20)));
            task.waker().wake_by_ref();
        }
        output
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_uses_remaining_original_validity_and_never_revives_expiry() {
        assert_eq!(
            refresh_delay(100, 1_000_100).unwrap(),
            Duration::from_millis(500)
        );
        assert_eq!(
            refresh_delay(900_100, 1_000_100).unwrap(),
            Duration::from_millis(50)
        );
        assert_eq!(refresh_delay(9, 10).unwrap(), Duration::from_micros(1));
        assert!(refresh_delay(10, 10).is_err());
        assert!(refresh_delay(11, 10).is_err());
    }
}

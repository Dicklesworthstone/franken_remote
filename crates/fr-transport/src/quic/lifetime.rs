//! A connection-wide destination/ingress guard cannot be replaced by a later
//! application caller's authorization closure. This adds no peer permission.
use super::{Error, QuicRecords, now};
use asupersync::cx::Cx;
use std::{
    future::{Future, poll_fn},
    pin::pin,
    sync::Arc,
    task::Poll,
};

impl QuicRecords {
    /// Install exactly once, before application traffic. Every later record
    /// admission, receive dispatch and native I/O poll also checks this guard.
    /// The callback must be bounded and nonblocking; do not do `LocalAPI` I/O here.
    /// A failed guard permanently closes this owner, never a replacement socket.
    pub fn retain_lifetime_check(
        &mut self,
        cx: &Cx,
        check: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Result<(), Error> {
        if self.lifetime_check.is_some() {
            return Err(Error::InvalidPolicy);
        }
        self.check(cx, &mut || true)?;
        if self.senders.iter().any(|s| s.bytes != 0) || self.read_bytes != 0 {
            return Err(Error::InvalidPolicy);
        }
        self.lifetime_check = Some(check);
        self.check(cx, &mut || true).map(|_| ())
    }
}

pub(super) async fn poll_io<T, E>(
    cx: &Cx,
    gate: Option<&(dyn Fn() -> bool + Send + Sync)>,
    authorize: &mut impl FnMut() -> bool,
    started: u64,
    until: Option<u64>,
    io: impl Future<Output = Result<T, E>>,
) -> Result<T, Error> {
    let mut io = pin!(io);
    poll_fn(|task| {
        if cx.checkpoint().is_err() {
            return Poll::Ready(Err(Error::Cancelled));
        }
        if gate.is_some_and(|check| !check()) || !authorize() {
            return Poll::Ready(Err(Error::Unauthorized));
        }
        match now(cx) {
            Err(error) => return Poll::Ready(Err(error)),
            Ok(at) if at < started => return Poll::Ready(Err(Error::Clock)),
            Ok(at) if until.is_some_and(|end| at >= end) => {
                return Poll::Ready(Err(Error::Expired));
            }
            Ok(_) => {}
        }
        io.as_mut()
            .poll(task)
            .map(|result| result.map_err(|_| Error::Native))
    })
    .await
}

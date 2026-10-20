//! A repair may become obsolete before admission without losing the session.
use super::{Cx, Error, Peer, ReceivePipeline, Repair, Route, now, quic};

enum Refusal {
    Obsolete,
    Fault(Error),
}
impl Repair {
    pub(super) fn prepare(
        &mut self,
        receiver: &mut ReceivePipeline,
        current: u64,
    ) -> Result<(), Error> {
        self.prepare_raw(receiver, current).map_err(Error::Delivery)
    }
}

/// True only when these exact bytes enter transport ownership. Backpressure
/// retains them unchanged. Obsolete unadmitted bytes are discarded; the caller
/// then services recovery/suspension before its next network turn. A retained
/// send expiry or actual connection-authority failure is never swallowed.
pub(super) fn transmit(
    peer: &mut Peer,
    receiver: &ReceivePipeline,
    repair: &mut Repair,
    cx: &Cx,
) -> Result<bool, Error> {
    if repair.len == 0 {
        return Ok(false);
    }
    let (session, media) = peer.parts()?;
    let route = Route::Stream(
        media
            .repair_stream(&session.transport)
            .map_err(Error::Routes)?,
    );
    let heard_until = session.heard_until;
    let admitted = session.transport.send_prepared(
        cx,
        route,
        &repair.bytes[..repair.len],
        repair.until,
        || now(cx).is_ok_and(|current| current < heard_until),
        || {
            let current = now(cx).map_err(|e| Refusal::Fault(Error::Session(e)))?;
            if repair
                .useful(receiver, current)
                .map_err(|e| Refusal::Fault(Error::Delivery(e)))?
            {
                Ok(())
            } else {
                Err(Refusal::Obsolete)
            }
        },
    );
    match admitted {
        Ok(quic::SendAdmission::Accepted) => {
            repair.clear();
            Ok(true)
        }
        Ok(quic::SendAdmission::Refused(Refusal::Obsolete)) => {
            repair.clear();
            Ok(false)
        }
        Ok(quic::SendAdmission::Refused(Refusal::Fault(error))) => Err(error),
        Err(quic::Error::Backpressure) => Ok(false),
        Err(quic::Error::Expired) if !session.transport.is_closed() => {
            repair.clear();
            Ok(false)
        }
        Err(error) => Err(Error::Transport(error)),
    }
}

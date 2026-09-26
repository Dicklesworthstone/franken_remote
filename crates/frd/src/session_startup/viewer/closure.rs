//! Terminal reports are consumed before any subsequent application dispatch.
use super::Error;
use fr_core::limits::ProtocolLimits;
use fr_transport::quic::{self, Route, StreamRoute};
use fr_wire::{
    Kind,
    authority::Binding,
    closure::{self, Closed},
    input::{InputDelivery, InputDirection},
};

pub(super) fn receive(
    retained: &mut Option<Closed>,
    route: Route,
    bytes: &[u8],
    expected: StreamRoute,
    binding: Binding,
    limits: &ProtocolLimits,
) -> Option<Error> {
    if bytes.get(6..8) != Some(&(Kind::Closed as u16).to_be_bytes()) {
        return None;
    }
    if route != Route::Stream(expected) {
        return Some(Error::Transport(quic::Error::WrongRoute));
    }
    Some(
        match closure::decode_closed(
            bytes,
            binding,
            limits,
            InputDirection::HostToViewer,
            InputDelivery::Reliable,
        ) {
            Ok(report) => {
                // Stop this receive batch by returning the existing dispatch error
                // path. SessionDrive/controlled ownership fences input, stops renewal,
                // and cancels local work. No later record or action can be dispatched
                // from this batch, and neither counters nor timestamps are fabricated.
                *retained = Some(report);
                Error::RemoteClosed(report)
            }
            Err(_) => Error::Transport(quic::Error::Malformed),
        },
    )
}

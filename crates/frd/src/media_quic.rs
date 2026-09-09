//! The existing media packet owner joined to the sole Asupersync QUIC adapter.
//! Session admission installs these routes. This module creates no listener,
//! identity, input authority, second packet queue, or independent congestion loop.
use crate::{
    media,
    media_egress::{Admission, Egress, EgressError, Lane, Progress},
};
use asupersync::cx::Cx;
use fr_media::{
    access_unit::EncodedAccessUnit,
    delivery::{BudgetUsage, MediaBindings, SendError},
};
use fr_transport::quic::{
    self, DatagramRoute, Messages, Priority, QuicRecords, Route, StreamRoute,
};
use fr_wire::Channel;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidRoutes,
    Media(media::Error),
    Transport(quic::Error),
    Allocation,
    Closed,
}
/// Host media routes installed by the authenticated session. Input and other
/// streams may coexist on the same connection; the session still owns dispatch.
#[derive(Debug, Clone, Copy)]
pub struct Routes {
    progress: StreamRoute,
    recovery: StreamRoute,
    video: DatagramRoute,
    repair: StreamRoute,
}
impl Routes {
    pub fn new(
        bindings: MediaBindings,
        progress: StreamRoute,
        recovery: StreamRoute,
        video: DatagramRoute,
        repair: StreamRoute,
    ) -> Result<Self, Error> {
        if !progress.outbound
            || !recovery.outbound
            || !video.outbound
            || repair.outbound
            || progress.priority != Priority::Critical
            || repair.priority != Priority::Critical
            || recovery.priority != Priority::Bulk
            || progress.messages != Messages::Exact(0x37)
            || recovery.messages != Messages::Exact(0x32)
            || video.kind != 0x34
            || repair.messages != Messages::Exact(0x35)
            || progress.binding != bindings.for_channel(Channel::MediaConfig)
            || recovery.binding != bindings.for_channel(Channel::Recovery)
            || video.binding != bindings.for_channel(Channel::Video)
            || repair.binding != bindings.for_channel(Channel::Control)
            || progress.stream == recovery.stream
            || progress.stream == repair.stream
            || recovery.stream == repair.stream
        {
            return Err(Error::InvalidRoutes);
        }
        Ok(Self {
            progress,
            recovery,
            video,
            repair,
        })
    }
    fn outbound(self, channel: Channel) -> Result<Route, Error> {
        match channel {
            Channel::MediaConfig => Ok(Route::Stream(self.progress)),
            Channel::Recovery => Ok(Route::Stream(self.recovery)),
            Channel::Video => Ok(Route::Datagram(self.video)),
            Channel::Control => Err(Error::InvalidRoutes),
        }
    }
    /// Exact inbound viewer route, not merely a peer-supplied record kind.
    pub fn viewer_channel(self, route: Route) -> Result<Channel, Error> {
        for (host, channel) in [
            (self.progress, Channel::MediaConfig),
            (self.recovery, Channel::Recovery),
        ] {
            if route
                == Route::Stream(StreamRoute {
                    outbound: false,
                    ..host
                })
            {
                return Ok(channel);
            }
        }
        if route
            == Route::Datagram(DatagramRoute {
                outbound: false,
                ..self.video
            })
        {
            return Ok(Channel::Video);
        }
        Err(Error::InvalidRoutes)
    }
    pub const fn repair_stream(self) -> StreamRoute {
        self.repair
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepairAdmission {
    Queued,
    /// A valid but stale, overlapping, or rate-limited request is consumed, not
    /// retried in a busy loop. No cache/repair deadline is renewed by refusal.
    Refused,
}
/// Retains the canonical Egress, not a competing media sender. The surrounding
/// session owns the QUIC connection, runs input/watchdog work independently, and
/// dispatches other channel kinds; media never gains input authority.
#[derive(Debug)]
pub struct QuicEgress {
    egress: Egress,
    routes: Routes,
}
impl QuicEgress {
    pub const fn new(egress: Egress, routes: Routes) -> Self {
        Self { egress, routes }
    }
    pub fn enqueue(&mut self, frame: EncodedAccessUnit) -> Result<(), Error> {
        self.tick()?;
        self.egress.enqueue(frame).map_err(Error::Media)
    }
    pub fn enqueue_capture(&mut self, update: media::CaptureUpdate) -> Result<(), Error> {
        self.tick()?;
        self.egress.enqueue_capture(update).map_err(Error::Media)
    }
    /// Includes a prepared, backpressured observation after its payload expired.
    pub fn next_deadline(&self) -> Option<fr_core::time::HostInstant> {
        self.egress.next_deadline()
    }
    pub fn tick(&mut self) -> Result<(), Error> {
        self.egress.tick().map_err(Error::Media)
    }
    pub fn close(&mut self) {
        self.egress.close();
    }
    pub fn is_closed(&self) -> bool {
        self.egress.is_closed()
    }
    pub fn cache_usage(&self) -> BudgetUsage {
        self.egress.cache_usage()
    }
    pub fn allocated_record_bytes(&self) -> usize {
        self.egress.allocated_bytes()
    }
    pub fn pending(&self) -> Option<&fr_media::delivery::PacketOffer> {
        self.egress.pending()
    }
    /// Exactly one record, respecting the same prepared record across every
    /// retry. QUIC rechecks the supplied authority guard after preparation and
    /// takes the original absolute deadline; admission is not remote delivery.
    pub fn transmit(
        &mut self,
        cx: &Cx,
        transport: &mut QuicRecords,
        lane: Lane,
    ) -> Result<Progress, Error> {
        let routes = self.routes;
        let result = self.egress.transmit(lane, |offer, bytes, guard| {
            let route = routes.outbound(offer.channel())?;
            match transport.send(cx, route, bytes, offer.send_by_micros(), || guard().is_ok()) {
                Ok(()) => Ok(Admission::Accepted),
                Err(quic::Error::Backpressure) => Ok(Admission::Backpressure),
                Err(error) => Err(Error::Transport(error)),
            }
        });
        match result {
            Ok(progress) => Ok(progress),
            Err(error) => {
                // An uncertain/partial reliable write is never continued on a
                // new lifetime. The session observes this close and revokes input.
                transport.close();
                self.close();
                Err(match error {
                    EgressError::Media(e) => Error::Media(e),
                    EgressError::Transport(e) => e,
                    EgressError::Allocation => Error::Allocation,
                    EgressError::Closed => Error::Closed,
                })
            }
        }
    }
    /// The session may use this bounded driver when media is active. Authority
    /// is checked before native writes and after waits, including on idle turns.
    /// Dropping this future closes both media and QUIC, never retries partial I/O.
    /// An independent session watchdog must still revoke input during the wait.
    pub async fn drive(
        &mut self,
        cx: &Cx,
        transport: &mut QuicRecords,
        wait: Duration,
    ) -> Result<(), Error> {
        let mut operation = DriveGuard {
            egress: &mut self.egress,
            transport,
            complete: false,
        };
        operation.egress.tick().map_err(Error::Media)?;
        operation
            .transport
            .drive(cx, wait, || operation.egress.tick().is_ok())
            .await
            .map_err(Error::Transport)?;
        operation.egress.tick().map_err(Error::Media)?;
        operation.complete = true;
        Ok(())
    }
    /// Invoke from the session's already-authorized synchronous dispatch, never
    /// by treating any control-channel payload as a repair request. The existing
    /// cache validates framing, generation, ranges, rate and reference lifetime.
    pub fn repair(&mut self, route: Route, bytes: &[u8]) -> Result<RepairAdmission, Error> {
        self.tick()?;
        if route != Route::Stream(self.routes.repair) {
            return Err(Error::InvalidRoutes);
        }
        match self.egress.queue_repair(bytes) {
            Ok(()) => Ok(RepairAdmission::Queued),
            Err(media::Error::Send(
                SendError::FrameUnavailable
                | SendError::RepairBusy
                | SendError::RepairNotReady
                | SendError::RepairRateLimited
                | SendError::RepairBudgetExceeded,
            )) => Ok(RepairAdmission::Refused),
            Err(error) => Err(Error::Media(error)),
        }
    }
}
struct DriveGuard<'a> {
    egress: &'a mut Egress,
    transport: &'a mut QuicRecords,
    complete: bool,
}
impl Drop for DriveGuard<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.egress.close();
            self.transport.close();
        }
    }
}

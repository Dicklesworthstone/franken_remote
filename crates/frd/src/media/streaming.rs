//! One dedicated capture source handed from completed decoder startup into
//! continuous service. This is not shared-encoder fanout or an input grant.
use super::{CaptureSource, ObservationControl, decoder_startup};
use crate::{media_quic::QuicEgress, worker};
use asupersync::process::ExitStatus;
use fr_transport::quic::QuicRecords;
use std::time::Duration;

/// Fixed local pacing. It never overrides encoder frame rate or congestion
/// admission. Missed capture opportunities are not accumulated for catch-up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    pub capture_interval: Duration,
    pub network_turn: Duration,
    pub records_per_turn: u8,
}
impl Default for Policy {
    fn default() -> Self {
        Self {
            capture_interval: Duration::from_micros(33_334),
            network_turn: Duration::from_millis(5),
            records_per_turn: 16,
        }
    }
}
impl Policy {
    pub(crate) fn validate(self, fps: u16) -> Result<(), super::Error> {
        if fps == 0 {
            return Err(super::Error::InvalidFrame);
        }
        let minimum = 1_000_000_u64.div_ceil(u64::from(fps));
        if self.capture_interval.as_micros() < u128::from(minimum)
            || self.capture_interval > Duration::from_secs(1)
            || self.network_turn < Duration::from_millis(1)
            || self.network_turn > Duration::from_millis(50)
            || !(1..=64).contains(&self.records_per_turn)
        {
            return Err(super::Error::InvalidFrame);
        }
        Ok(())
    }
}
/// Counts actual admissions, not delivered, decoded or displayed pictures.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Statistics {
    pub encoded_updates: u64,
    pub unchanged_observations: u64,
    pub admitted_records: u64,
    pub repair_requests: u64,
}
/// Consumes a UNIQUE capture source and the matching subscriber. Only an actual
/// completed decoder startup can construct it. A share-session encoder feeding
/// other viewers must remain with its separate fanout owner, not move here.
pub struct Stream {
    pub(crate) source: CaptureSource,
    pub(crate) sender: QuicEgress,
    pub(crate) control: ObservationControl,
    pub(crate) policy: Policy,
    pub(crate) capacity: usize,
    pub(crate) statistics: Statistics,
    pub(crate) served: bool,
}
impl std::fmt::Debug for Stream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeStream")
            .field("statistics", &self.statistics)
            .finish_non_exhaustive()
    }
}
impl Stream {
    /// The bootstrap update must already have passed through this exact sender.
    /// Equal frame/configuration IDs do not match a replacement capture source.
    pub fn new(
        startup: decoder_startup::Host,
        source: CaptureSource,
        mut sender: QuicEgress,
        connection: &QuicRecords,
        policy: Policy,
    ) -> Result<Self, crate::session_startup::Error> {
        use crate::session_startup::Error;
        policy
            .validate(source.configuration.fps)
            .map_err(Error::Media)?;
        let (control, binding) = startup
            .finish_stream(connection)
            .map_err(|_| Error::Order)?;
        sender
            .join_stream(connection, &source, &control, binding)
            .map_err(Error::MediaTransport)?;
        // parse_unit retains the original IPC allocation including its prefix.
        let capacity = usize::try_from(source.configuration.max_access_unit_bytes)
            .ok()
            .and_then(|n| n.checked_add(fr_media::worker::UNIT_PREFIX_BYTES))
            .ok_or(Error::InvalidConfiguration)?;
        if capacity > sender.maximum_capacity().map_err(Error::MediaTransport)? {
            return Err(Error::InvalidConfiguration);
        }
        Ok(Self {
            source,
            sender,
            control,
            policy,
            capacity,
            statistics: Statistics::default(),
            served: false,
        })
    }
    pub const fn statistics(&self) -> Statistics {
        self.statistics
    }
    pub fn worker_id(&self) -> Option<u32> {
        self.source.worker_id()
    }
    pub(crate) fn close(&mut self) {
        self.control.revoke();
        self.sender.close();
        self.source.worker.abort();
    }
    /// Call after terminal service, with a live cleanup context. Revocation
    /// precedes abort; this reports actual process exit, not merely kill intent.
    pub(crate) async fn reap(
        &mut self,
        cx: &asupersync::cx::Cx,
        deadline: worker::Deadline,
    ) -> Result<ExitStatus, worker::Error> {
        self.source.worker.reap(cx, deadline).await
    }
}
impl Drop for Stream {
    fn drop(&mut self) {
        self.close();
    }
}

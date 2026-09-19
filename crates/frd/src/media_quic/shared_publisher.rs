//! Shared publication still requires original, completed connection proofs.
use super::{Error, NegotiatedMedia, QuicEgress};
use crate::media::{CaptureSource, ObservationControl, PreparedSharedCapture};
use fr_media::delivery::MediaEpoch;
use fr_transport::quic::QuicRecords;
use fr_wire::decoder::Binding;
impl QuicEgress {
    pub(crate) fn join_shared_publisher(
        &self,
        q: &QuicRecords,
        media: &NegotiatedMedia,
        source: &CaptureSource,
        owner: &ObservationControl,
        control: &ObservationControl,
        view: Binding,
    ) -> Result<(), Error> {
        media.check_shared_publication(q, view)?;
        if self.connection.as_ref().is_none_or(|b| !q.is_bound_to(b)) || self.view != Some(view) {
            return Err(Error::ForeignConnection);
        }
        self.egress
            .stream_subscription()
            .map_err(Error::Media)?
            .join_shared_source(
                source,
                owner,
                control,
                MediaEpoch {
                    configuration: view.configuration,
                    recovery: view.recovery,
                },
            )
            .map_err(Error::Media)
    }
    pub(crate) fn shared_publisher_credit(
        &mut self,
        prepared: &PreparedSharedCapture<'_>,
    ) -> Result<bool, Error> {
        prepared
            .check_recipient(&mut self.egress)
            .map_err(Error::Media)
    }
}

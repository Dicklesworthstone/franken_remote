//! Explicit, bounded export of one retained presentation from its original worker.
//! No pathname, capture request, decoder admission, freshness or input authority.
use super::{Error, MediaOperation, Presenter};
use crate::worker;
use asupersync::cx::Cx;
use fr_media::worker::{
    Kind,
    presentation_snapshot::{self, Snapshot, Stamp},
};
use std::future::Future;

impl Presenter {
    /// Prepare before yielding, retaining the ORIGINAL native request deadline.
    /// The session owner drives renewal/revocation beside this request and checks
    /// its original receiver and media generation again before exposing pixels.
    pub(crate) fn read_presentation<'a>(
        &'a mut self,
        cx: &'a Cx,
        expected: Stamp,
    ) -> impl Future<Output = Result<Option<Snapshot>, Error>> + 'a {
        let prepared = (|| {
            if self.worker.state() != worker::State::Running
                || expected.configuration != self.configuration.generation
            {
                return Err(Error::InvalidFrame);
            }
            let deadline =
                worker::Deadline::after(cx, super::presentation::NATIVE_REQUEST_CEILING)?;
            let source_bytes = u64::from(self.configuration.width)
                .checked_mul(u64::from(self.configuration.height))
                .and_then(|pixels| pixels.checked_mul(4))
                .ok_or(Error::InvalidFrame)?;
            let pixels = source_bytes.min(presentation_snapshot::MAX_PIXEL_BYTES as u64);
            let maximum = usize::try_from(pixels)
                .map_err(|_| Error::InvalidFrame)?
                .checked_add(presentation_snapshot::HEADER_BYTES)
                .ok_or(Error::InvalidFrame)?;
            Ok((deadline, maximum))
        })();
        async move {
            let (deadline, maximum) = prepared?;
            let mut operation = MediaOperation::new(&mut self.worker);
            let reply = operation
                .worker
                .request_with_response_capacity(
                    cx,
                    Kind::ReadPresentation,
                    expected.encode().to_vec(),
                    Some(maximum),
                    deadline,
                )
                .await?;
            let snapshot = match reply.header.kind {
                Kind::PresentationAbsent => {
                    presentation_snapshot::Absent::decode(reply.body())?;
                    None
                }
                Kind::PresentationPixels => {
                    let snapshot = Snapshot::decode(reply.into_body())?;
                    if snapshot.stamp() != expected
                        || snapshot.width() > self.configuration.width
                        || snapshot.height() > self.configuration.height
                    {
                        return Err(Error::InvalidFrame);
                    }
                    Some(snapshot)
                }
                _ => return Err(Error::InvalidFrame),
            };
            operation.completed = true;
            Ok(snapshot)
        }
    }
}

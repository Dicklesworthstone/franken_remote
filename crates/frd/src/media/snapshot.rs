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
            Ok(worker::Deadline::after(
                cx,
                super::presentation::NATIVE_REQUEST_CEILING,
            )?)
        })();
        async move {
            let deadline = prepared?;
            let mut operation = MediaOperation::new(&mut self.worker);
            let reply = operation
                .worker
                .request_with_response_capacity(
                    cx,
                    Kind::ReadPresentation,
                    expected.encode().to_vec(),
                    // The composited viewport can exceed the decoded source
                    // (Retina, local resize or letterboxing). Only this explicit
                    // export gets its independent bounded reply reservation;
                    // encoded pictures and ordinary commands keep their limits.
                    Some(presentation_snapshot::MAX_BODY_BYTES),
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
                    if snapshot.stamp() != expected {
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

#[cfg(all(test, unix))]
#[path = "snapshot/tests.rs"]
mod tests;

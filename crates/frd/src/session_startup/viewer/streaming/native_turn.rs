//! One native operation alongside one ORIGINAL bounded network turn. Neither
//! successful native completion nor native failure abandons an in-flight drive.
use super::{Error, StreamingViewerControl, media, poll_fn};
use std::{
    future::Future,
    pin::{Pin, pin},
    task::Poll,
};

/// None leaves the same pending native future with its caller for the next
/// network turn. Some is one actual completion, not a presentation witness.
/// The caller's serving guard fences cancellation before dropping either owner.
pub(super) async fn drive<T>(
    mut native: Pin<&mut impl Future<Output = Result<T, media::Error>>>,
    network: impl Future<Output = Result<(), Error>>,
    control: &StreamingViewerControl,
) -> Result<Option<T>, Error> {
    let mut network = pin!(network);
    let mut completed = None;
    poll_fn(|task| {
        if completed.is_none()
            && let Poll::Ready(value) = native.as_mut().poll(task)
        {
            completed = Some(value);
        }
        let failure = match &completed {
            Some(Err(error)) => Some(*error),
            _ => None,
        };
        if failure.is_some() {
            // Fence FIRST, then let this exact drive observe cancellation and
            // retire its I/O. Returning here would drop the connection early.
            control.stop();
        }
        match network.as_mut().poll(task) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                if result.is_err() {
                    // A pending native borrow is dropped by the caller only
                    // after its authority is fenced, never the reverse order.
                    control.stop();
                }
                Poll::Ready(failure.map_or(result, |error| Err(Error::Media(error))))
            }
        }
    })
    .await?;
    completed.transpose().map_err(Error::Media)
}

#[cfg(test)]
mod tests;

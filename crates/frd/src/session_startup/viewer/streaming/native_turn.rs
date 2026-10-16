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
///
/// A plain function, not another async layer: the caller's future then holds
/// `checked`'s state once instead of also keeping `network` alive beside it,
/// which put every viewer serving future over Clippy's 16 KiB limit.
pub(super) fn drive<T>(
    native: Pin<&mut impl Future<Output = Result<T, media::Error>>>,
    network: impl Future<Output = Result<(), Error>>,
    control: &StreamingViewerControl,
) -> impl Future<Output = Result<Option<T>, Error>> {
    checked(native, network, control, Error::Media, || Ok(()))
}

/// Recovery uses the same join, with its ORIGINAL absolute budget and typed
/// startup errors. A budget or native failure fences authority immediately,
/// then drains the already-created network turn; it neither restarts that
/// drive nor polls native work again. Draining cancelled I/O buys no additional
/// decode, presentation, request or input lifetime.
///
/// Check both before native polling and after a successful network turn: a
/// completion held across that turn cannot escape an expired recovery budget.
/// The enclosing serving guard still owns abandonment, including unpolled work.
pub(super) async fn checked<T, E>(
    mut native: Pin<&mut impl Future<Output = Result<T, E>>>,
    network: impl Future<Output = Result<(), Error>>,
    control: &StreamingViewerControl,
    map_error: impl Fn(E) -> Error,
    mut permitted: impl FnMut() -> Result<(), Error>,
) -> Result<Option<T>, Error> {
    let mut network = pin!(network);
    let mut completed = None;
    let mut failure = None;
    poll_fn(|task| {
        // Retain the original cause. In particular, our own cancellation must
        // not replace a decoder refusal or budget expiry with plain Closed.
        if failure.is_none() {
            failure = permitted().err();
        }
        if failure.is_none()
            && completed.is_none()
            && let Poll::Ready(value) = native.as_mut().poll(task)
        {
            match value {
                Ok(value) => completed = Some(value),
                Err(error) => failure = Some(map_error(error)),
            }
        }
        if failure.is_some() {
            control.stop();
        }
        match network.as_mut().poll(task) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                let result = failure.map_or(result, Err).and_then(|()| permitted());
                if result.is_err() {
                    // Fence before either the retained completion or a pending
                    // native borrow can be dropped by this join's caller.
                    control.stop();
                }
                Poll::Ready(result)
            }
        }
    })
    .await?;
    Ok(completed)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod recovery_tests;

//! Optional audio on the original exclusive publication, never on its input
//! executor. The existing source is polled beside the existing session; audio
//! IPC cannot lend its wait to the authority or renewal path.
use super::super::{Attempt, Error, NativePublisher};
use super::{GrantError, HostControlState, Seat, Target};
use crate::{
    media::shared_publisher::{AudioFeed, AudioProfile, AudioSource},
    session_startup::{self, native_control, running::Services},
};
use fr_core::ids::InputTicketId;
use fr_transport::quic::{Disposition, QuicRecords, Route};
use std::{
    future::{Future, poll_fn},
    pin::pin,
    task::Poll,
};

struct AudioServices {
    feed: Option<AudioFeed>,
}
impl Services for AudioServices {
    fn maintain<N: FnMut() -> Result<u128, ()>>(
        &mut self,
        q: &mut QuicRecords,
        _: &mut N,
    ) -> Result<(), session_startup::Error> {
        if let Some(feed) = &self.feed {
            feed.service_exclusive(q)
                .map_err(session_startup::Error::SharedPublication)?;
        }
        Ok(())
    }
    fn receive(&mut self, route: Route, bytes: &[u8]) -> Result<Disposition, ()> {
        if let Some(feed) = &self.feed
            && let Some(disposition) = feed.receive_exclusive(route, bytes).map_err(|_| ())?
        {
            return Ok(disposition);
        }
        // Preserve the canonical publisher's refusal of unrelated records.
        Err(())
    }
}
impl Drop for AudioServices {
    fn drop(&mut self) {
        if let Some(feed) = &self.feed {
            feed.close_exclusive();
        }
    }
}

impl NativePublisher {
    pub(super) fn accepting_control_with_audio<'a>(
        &'a mut self,
        seat: Seat,
        mut local: impl FnMut(HostControlState<'_>) -> Result<Option<Target>, GrantError> + 'a,
        nonce: impl FnMut() -> Result<u128, ()> + 'a,
        ticket: impl FnMut() -> Option<InputTicketId> + 'a,
        profile: Option<AudioProfile>,
    ) -> impl Future<Output = Result<(), Error>> + 'a {
        let control = self.control.clone();
        let display = self.display;
        let binding = self.view;
        let audio = profile
            .map(|profile| self.host.prepare_audio(profile))
            .transpose()
            .map(Option::flatten)
            .map_err(Error::Session);
        // Consume the original attachment at CALL time, including setup errors.
        // The outer Attempt fences even an unpolled or abandoned operation.
        let input = self.input.take().ok_or(Error::InvalidConfiguration);
        let prepared = input.and_then(|input| audio.map(|audio| (input, audio)));
        let future = prepared.map(|(input, audio)| {
            let (feed, source) = match audio {
                Some((feed, source)) => (Some(feed), Some(source)),
                None => (None, None),
            };
            let service = self.host.serve_control_services(
                seat,
                input,
                move |state| {
                    let request = match &state {
                        HostControlState::Pending(pending) => pending.request(),
                        HostControlState::Active { request, .. } => Some(*request),
                    };
                    if let Some(request) = request {
                        native_control::check_target(display, binding, request.target)
                            .map_err(|_| GrantError::TargetChanged)?;
                    }
                    let current = local(state)?;
                    if let Some(target) = current {
                        native_control::check_target(display, binding, target)
                            .map_err(|_| GrantError::TargetChanged)?;
                    }
                    Ok(current)
                },
                nonce,
                ticket,
                AudioServices { feed },
            );
            alongside(service, source.map(AudioSource::serve))
        });
        Attempt {
            control,
            complete: false,
            inner: Box::pin(async move { future?.await.map_err(Error::Session) }),
        }
    }
}

/// Session progress has priority, including the poll which observes terminal
/// revocation. Source completion is audio-only; its own retiring owner publishes
/// a typed stop and aborts its child. Native cleanup receipts stay in the local
/// AudioProfile's retained slot, not discarded or relabelled as confirmed exit.
async fn alongside<F, S, T>(session: F, source: Option<S>) -> T
where
    F: Future<Output = T>,
    S: Future<Output = Result<(), crate::media::shared_publisher::Error>>,
{
    // Drop the session/AudioServices before the source on abandonment. Its
    // original outer Attempt/managed Service already revoked authority first.
    let mut source = source.map(Box::pin);
    let mut session = pin!(session);
    poll_fn(|task| {
        if let Poll::Ready(result) = session.as_mut().poll(task) {
            return Poll::Ready(result);
        }
        if let Some(work) = &mut source
            && work.as_mut().poll(task).is_ready()
        {
            source = None;
        }
        Poll::Pending
    })
    .await
}

#[cfg(test)]
mod tests;

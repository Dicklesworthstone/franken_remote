//! Concrete native adapter for the original `ControlledViewer`, without an
//! frd -> fr-native dependency or a second transport/capture implementation.
use super::{Error, Options, State, Uplink, transmit};
use fr_core::audio::{MicPermission, MicTalkMode};
use fr_transport::quic::{QuicRecords, Route};
use frd::session_startup::{
    ControlledViewer,
    viewer_microphone::{
        self as viewer, AttachError, CaptureFailure, NativeMicrophone, Retirement, Status,
    },
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Default)]
struct Observed {
    retirement: Option<crate::audio_process::Retirement>,
    status: Option<Status>,
}
/// Original native capture retirement, retained after the session owner ends.
/// This observer grants no recording or replacement authority. A requested
/// stop is never converted into a child-reaping receipt.
#[derive(Clone, Default)]
pub struct CaptureRetirement(Arc<Mutex<Observed>>);
impl CaptureRetirement {
    pub fn is_complete(&self) -> bool {
        self.0.lock().is_ok_and(|state| {
            state
                .retirement
                .as_ref()
                .is_none_or(crate::audio_process::Retirement::is_complete)
        })
    }
    pub fn status(&self) -> Option<Status> {
        let state = self.0.lock().ok()?;
        let mut status = state.status?;
        status.retirement = state
            .retirement
            .as_ref()
            .map_or(Retirement::NotStarted, |r| {
                if r.is_complete() {
                    Retirement::Complete
                } else {
                    Retirement::Pending
                }
            });
        Some(status)
    }
    pub(super) fn record(&self, uplink: &Uplink, ended: bool) {
        // Status checks can consult the application's local UI predicate;
        // never execute that code while the observer snapshot lock is held.
        let retirement = uplink.retirement();
        let mut status = uplink.native_status();
        if ended {
            status.state = viewer::State::Stopped;
        }
        if let Ok(mut state) = self.0.lock() {
            state.retirement = retirement;
            state.status = Some(status);
        }
    }
}

impl Uplink {
    /// Install an opt-in microphone on the original presented controller.
    /// A host offer must complete within the fixed timeout; no native capture
    /// starts before a later local permission result, explicit talk action and
    /// the host's exact positive `AudioConfigured` acknowledgement.
    pub fn expect_on(
        viewer: &mut ControlledViewer,
        options: Options,
        timeout: Duration,
    ) -> Result<(), viewer::Error> {
        Self::expect_with_local_check(viewer, options, timeout, None, None)
    }
    /// The public local UI keeps its independently sampled talk fence in every
    /// native capture/configuration/egress proof. Muting or losing the UI stops
    /// the original epoch even while the session task is stalled. This check
    /// never enables talk and is not applied to an idle muted attachment.
    pub fn expect_on_with_consent(
        viewer: &mut ControlledViewer,
        options: Options,
        timeout: Duration,
        check: fr_client::audio::uplink::LocalTalkCheck,
    ) -> Result<CaptureRetirement, viewer::Error> {
        let observer = CaptureRetirement::default();
        Self::expect_with_local_check(
            viewer,
            options,
            timeout,
            Some(check),
            Some(observer.clone()),
        )?;
        Ok(observer)
    }
    fn expect_with_local_check(
        viewer: &mut ControlledViewer,
        options: Options,
        timeout: Duration,
        check: Option<fr_client::audio::uplink::LocalTalkCheck>,
        observer: Option<CaptureRetirement>,
    ) -> Result<(), viewer::Error> {
        viewer.expect_microphone(timeout, move |q, lane, input, selection, cx| {
            let Some(clock) = cx.timer_driver() else {
                return Err(AttachError::new(viewer::Error::Clock, lane));
            };
            Self::attach_presented(
                q,
                lane,
                input,
                selection,
                // The actual authenticated host initiated this completed
                // AudioUp offer. Its separate approval is enforced again by
                // its native owner before the positive configuration reply.
                true,
                options,
                cx,
                || fr_client::input::ClientInstant(clock.now().as_nanos() / 1000),
            )
            .map(|mut uplink| {
                if let Some(check) = check {
                    uplink.transmitter.set_local_talk_check(check);
                }
                uplink.observer = observer;
                uplink.observe_native();
                Box::new(uplink) as Box<dyn NativeMicrophone>
            })
            .map_err(|error| {
                let (reason, lane) = error.into_parts();
                AttachError::new(convert(reason), lane)
            })
        })
    }
    fn observe_native(&self) {
        if let Some(observer) = &self.observer {
            observer.record(self, false);
        }
    }
    fn native_status(&self) -> Status {
        Status {
            state: match self.state() {
                State::Muted => viewer::State::Muted,
                State::AwaitingHost => viewer::State::AwaitingHost,
                State::Starting => viewer::State::Starting,
                State::Talking => viewer::State::Talking,
                State::Stopped => viewer::State::Stopped,
            },
            permission: self.permission(),
            reason: self.reason().map(convert),
            retirement: self.retirement().map_or(Retirement::NotStarted, |receipt| {
                if receipt.is_complete() {
                    Retirement::Complete
                } else {
                    Retirement::Pending
                }
            }),
        }
    }
}

impl NativeMicrophone for Uplink {
    fn operation_check(&self) -> Arc<dyn Fn() -> bool + Send + Sync> {
        Self::operation_check(self)
    }
    fn owns_inbound(&self, route: Route) -> bool {
        Self::owns_inbound(self, route)
    }
    fn permits_io(&self) -> bool {
        Self::permits_io(self)
    }
    fn maintain(&mut self, q: &mut QuicRecords) -> Result<(), viewer::Error> {
        let result = Self::maintain(self, q).map_err(convert);
        self.observe_native();
        result
    }
    fn service(
        &mut self,
        q: &mut QuicRecords,
        authorize: &mut dyn FnMut() -> bool,
    ) -> Result<(), viewer::Error> {
        let result = Self::service(self, q, authorize)
            .map(|_| ())
            .map_err(convert);
        self.observe_native();
        result
    }
    fn set_permission(&mut self, permission: MicPermission) -> Result<(), viewer::Error> {
        let result = Self::set_permission(self, permission).map_err(convert);
        self.observe_native();
        result
    }
    fn begin_talk(&mut self, mode: MicTalkMode) -> Result<(), viewer::Error> {
        let result = Self::begin_talk(self, mode).map(|_| ()).map_err(convert);
        self.observe_native();
        result
    }
    fn mute(&mut self) -> Result<(), viewer::Error> {
        let result = Self::mute(self).map_err(convert);
        self.observe_native();
        result
    }
    fn stop(&mut self) {
        Self::stop(self);
        self.observe_native();
    }
    fn retire(&mut self, q: &mut QuicRecords) -> Result<(), viewer::Error> {
        let result = Self::retire(self, q).map_err(convert);
        self.observe_native();
        result
    }
    fn status(&self) -> Status {
        self.native_status()
    }
}

fn convert(error: Error) -> viewer::Error {
    match error {
        Error::WrongConnection | Error::WrongScope => viewer::Error::WrongBinding,
        Error::Busy => viewer::Error::Busy,
        Error::Clock => viewer::Error::Clock,
        Error::Closed => viewer::Error::Closed,
        Error::InvalidRecord => viewer::Error::InvalidRecord,
        Error::EgressExpired => viewer::Error::EgressExpired,
        Error::Consent(error) => viewer::Error::Consent(error),
        Error::Transport(error) => viewer::Error::Transport(error),
        Error::Capture(error) => match error {
            transmit::Error::Consent(error) => viewer::Error::Consent(error),
            transmit::Error::Clock => viewer::Error::Clock,
            transmit::Error::Order => viewer::Error::Order,
            transmit::Error::Transport => viewer::Error::CaptureTransport,
            transmit::Error::Capture(error) => viewer::Error::Capture(capture(error)),
        },
    }
}
fn capture(error: crate::audio_process::Error) -> CaptureFailure {
    use crate::audio_process::Error as E;
    match error {
        E::Configuration => CaptureFailure::Configuration,
        E::Selection => CaptureFailure::Selection,
        E::Unavailable => CaptureFailure::Unavailable,
        E::Denied => CaptureFailure::Denied,
        E::Expired => CaptureFailure::Expired,
        E::Clock => CaptureFailure::Clock,
        E::Backpressure => CaptureFailure::Backpressure,
        E::Protocol => CaptureFailure::Protocol,
        E::Spawn => CaptureFailure::Spawn,
        E::Native => CaptureFailure::Native,
        E::Stopped => CaptureFailure::Stopped,
        E::CaptureDiscontinuity => CaptureFailure::Discontinuity,
        E::Allocation => CaptureFailure::Allocation,
    }
}

#![forbid(unsafe_code)]
//! Original controller consent → acknowledged configuration → supervised native
//! recording → bounded wire admission. No source opens before the host accepts
//! the exact configuration. The original input owner, OS permission and talk
//! generation remain live checks, including while the application is idle.
//!
//! This adapter performs no network I/O. Its containing session retains the
//! completed `AudioUp` attachment and admits records on that original connection
//! with the supplied immutable deadline. A callback returning true means whole
//! record admission, never that a host application heard the audio. Callbacks
//! must be nonblocking and must enforce the same original permission at their
//! own queue boundary. Native capture and encoding stay in the child process.
use super::{CaptureOptions, MicrophoneCapture, Retirement};
use fr_client::{
    audio::uplink::{self, CapturePermit, CaptureReceipt, ConfigurationPermit, Sender},
    input::ClientInstant,
};
use fr_core::audio::{MicPermission, MicTalkMode};
use fr_media::audio::AudioAccessUnit;
use fr_wire::audio::{AudioConfiguration, AudioStop};
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

const CUSTODY_US: u64 = 100_000;
const TURN_PACKETS: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Consent(uplink::Error),
    Capture(super::Error),
    Clock,
    Order,
    Transport,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "microphone-transmit: {self:?}")
    }
}
impl std::error::Error for Error {}

/// Local user-selected source. These values never come from an audio record.
/// No default source, implicit server or automatic device replacement exists.
pub struct Source {
    pub server: PathBuf,
    pub name: String,
    pub bitrate: u32,
}
impl std::fmt::Debug for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MicrophoneSource([explicit local selection])")
    }
}

/// State for a containing session's visible microphone indicator. Ready means
/// the native recorder is configured under active talk consent, not proof that
/// every packet was admitted or that an application selected the host source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Disabled,
    AwaitingHost,
    Starting,
    Ready,
    Stopped,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub admitted_packets: usize,
    pub blocked: bool,
}

/// The anchor is sampled BEFORE reading the client's existing monotonic clock.
/// Extrapolated checks are therefore conservative and translated deadlines are
/// equal or earlier. This never correlates a peer's clock or renews a grant.
#[derive(Clone, Copy)]
struct Clock {
    origin: Instant,
    client: ClientInstant,
}
impl Clock {
    fn bind(now: impl FnOnce() -> ClientInstant) -> Self {
        let origin = Instant::now();
        Self {
            origin,
            client: now(),
        }
    }
    fn now(self) -> Result<ClientInstant, Error> {
        let elapsed = Instant::now()
            .checked_duration_since(self.origin)
            .ok_or(Error::Clock)?;
        let us = u64::try_from(elapsed.as_nanos().div_ceil(1000)).map_err(|_| Error::Clock)?;
        self.client
            .0
            .checked_add(us)
            .map(ClientInstant)
            .ok_or(Error::Clock)
    }
    fn deadline(self, until: ClientInstant) -> Result<Instant, Error> {
        let us = until.0.checked_sub(self.client.0).ok_or(Error::Clock)?;
        self.origin
            .checked_add(Duration::from_micros(us))
            .ok_or(Error::Clock)
    }
    fn lower_deadline(self, until: Instant) -> Result<ClientInstant, Error> {
        let us = u64::try_from(
            until
                .checked_duration_since(self.origin)
                .ok_or(Error::Clock)?
                .as_micros(),
        )
        .map_err(|_| Error::Clock)?;
        self.client
            .0
            .checked_add(us)
            .map(ClientInstant)
            .ok_or(Error::Clock)
    }
    fn permits(self, permit: &CapturePermit) -> bool {
        self.now().is_ok_and(|now| permit.valid(now))
    }
}

struct Packets {
    receipt: CaptureReceipt,
    packets: std::vec::IntoIter<AudioAccessUnit>,
}

/// Original weak authority for the reliable configuration queue before a
/// host ACK. This grants no capture or packet admission; it must be retained
/// until those exact configuration bytes have left native transport custody.
#[derive(Clone)]
pub struct ConfigurationEgress {
    permit: ConfigurationPermit,
    clock: Clock,
}
impl ConfigurationEgress {
    pub fn is_open(&self) -> bool {
        self.clock.now().is_ok_and(|now| self.permit.valid(now))
    }
}

/// Read-only fence for retained output under one immutable capture proof.
/// After an acknowledged renewal, new output obtains a new `Egress`; an older
/// clone still expires at its original cap and cannot borrow the successor.
/// It cannot retain the original controller/sender or enable a replacement.
/// The containing transport still owns its authenticated route and must retire
/// that route when this gate closes; this handle grants no channel ownership.
#[derive(Clone)]
pub struct Egress {
    permit: Arc<CapturePermit>,
    control: super::Control,
    clock: Clock,
}
impl Egress {
    pub fn is_open(&self) -> bool {
        !self.control.is_stopped() && self.clock.permits(&self.permit)
    }
}
impl std::fmt::Debug for Egress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MicrophoneEgress")
            .field("open", &self.is_open())
            .finish_non_exhaustive()
    }
}

/// One attachment's sender and original native capture process. At most one
/// native request, one four-packet batch and one framed record are retained.
/// Backpressure stops further pulls; expiry never makes room by renewing audio.
pub struct Transmitter {
    sender: Sender,
    image: PathBuf,
    source: Source,
    clock: Clock,
    offer: Option<AudioConfiguration>,
    permit: Option<Arc<CapturePermit>>,
    pending_permit: Option<Arc<CapturePermit>>,
    capture: Option<MicrophoneCapture>,
    retirement: Option<Retirement>,
    request: Option<CaptureReceipt>,
    packets: Option<Packets>,
    state: State,
}
impl std::fmt::Debug for Transmitter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MicrophoneTransmitter")
            .field("state", &self.state())
            .finish_non_exhaustive()
    }
}
impl Transmitter {
    /// `now` samples the SAME client clock used to accept the original control
    /// grant. No process, socket, capture device or codec is opened here.
    pub fn new(
        sender: Sender,
        image: &Path,
        source: Source,
        now: impl FnOnce() -> ClientInstant,
    ) -> Self {
        Self {
            sender,
            image: image.into(),
            source,
            clock: Clock::bind(now),
            offer: None,
            permit: None,
            pending_permit: None,
            capture: None,
            retirement: None,
            request: None,
            packets: None,
            state: State::Disabled,
        }
    }
    pub fn state(&self) -> State {
        if self.sender.status() == uplink::Status::Stopped
            || self
                .capture
                .as_ref()
                .is_some_and(|capture| capture.control().is_stopped())
            || self
                .permit
                .as_ref()
                .is_some_and(|permit| !self.clock.permits(permit))
        {
            State::Stopped
        } else {
            self.state
        }
    }
    /// Actual child reaping is distinct from stopping recording admission.
    pub fn retirement(&self) -> Option<Retirement> {
        self.retirement.clone()
    }
    /// The immutable pre-ACK configuration guard, never a capture capability.
    /// Snapshot before admitting the reliable record and retain through drain.
    pub fn configuration_egress(&self) -> Option<ConfigurationEgress> {
        Some(ConfigurationEgress {
            permit: self.sender.configuration_permit()?,
            clock: self.clock,
        })
    }
    /// The final queue/send guard, available only after the host ACK and
    /// process admission. Sample this for each new transport handoff: a later
    /// acknowledged renewal never extends an already returned clone.
    /// Mute, source failure, consent loss, replacement or
    /// owner destruction also invalidate every previously returned clone.
    pub fn egress(&self) -> Option<Egress> {
        Some(Egress {
            permit: Arc::clone(self.permit.as_ref()?),
            control: self.capture.as_ref()?.control(),
            clock: self.clock,
        })
    }
    pub fn set_permission(&mut self, permission: MicPermission) {
        self.sender.set_permission(permission);
        if permission != MicPermission::Granted {
            self.stop_capture();
            self.offer = None;
            self.state = State::Disabled;
        }
    }
    /// Install the original local UI fence before capture is enabled. Each
    /// immutable talk proof retains this check, including the native process's
    /// independently serviced monitor. Replacing it always discards old talk.
    pub fn set_local_talk_check(&mut self, check: uplink::LocalTalkCheck) {
        self.stop_capture();
        self.sender.set_local_talk_check(check);
        self.offer = None;
        self.state = State::Disabled;
    }
    /// A local panel may fence the native monitor before the session task gets
    /// its next UI callback. Preserve that explicit cause for the existing mute
    /// protocol. Never reinterpret an already failed native process as a mute.
    pub fn local_talk_withdrawn(&self) -> bool {
        if self.state == State::Stopped {
            return false;
        }
        if self.capture.as_ref().is_some_and(|capture| {
            capture.shared.stopped.load(Ordering::Acquire) && !self.sender.local_talk_withdrawn()
        }) {
            return false;
        }
        // No native callback is run here. This samples only the local bounded
        // UI predicate; an original capture monitor can have sampled it first.
        let _ = self.sender.status();
        self.sender.local_talk_withdrawn()
            && self.capture.as_ref().is_none_or(|capture| {
                !capture.shared.stopped.load(Ordering::Acquire)
                    || matches!(
                        capture.shared.error(),
                        super::Error::Denied | super::Error::Stopped
                    )
            })
    }
    /// Only an explicit new local talk action calls this method. A reconnect,
    /// restored OS permission, decoder readiness or timer never calls it.
    /// An older child must be positively reaped first. Backpressure preserves
    /// its cleanup receipt and stores no intent to start recording later.
    pub fn begin_talk(&mut self, mode: MicTalkMode) -> Result<AudioConfiguration, Error> {
        if self
            .retirement
            .as_ref()
            .is_some_and(|old| !old.is_complete())
        {
            return Err(Error::Capture(super::Error::Backpressure));
        }
        let result = (|| {
            let offer = self
                .sender
                .begin_talk(mode, self.clock.now()?)
                .map_err(Error::Consent)?;
            self.stop_capture();
            self.offer = Some(offer);
            self.state = State::AwaitingHost;
            Ok(offer)
        })();
        if matches!(
            result,
            Err(Error::Consent(
                uplink::Error::Permission | uplink::Error::NotTalking | uplink::Error::Order
            ))
        ) {
            return result;
        }
        self.checked(result)
    }
    /// Retry only an unadmitted configuration under its original setup bound.
    /// Unknown callback outcome is terminal; it is never replayed automatically.
    pub fn send_configuration(
        &mut self,
        send: impl FnOnce(&[u8], ClientInstant) -> Result<bool, ()>,
    ) -> Result<bool, Error> {
        let result = (|| {
            let until = self.sender.configuration_deadline().ok_or(Error::Order)?;
            let Some(bytes) = self
                .sender
                .pending_configuration(self.clock.now()?)
                .map_err(Error::Consent)?
            else {
                return Ok(false);
            };
            let admitted = catch_unwind(AssertUnwindSafe(|| send(bytes, until)))
                .map_err(|_| Error::Transport)?
                .map_err(|()| Error::Transport)?;
            if admitted {
                self.sender
                    .configuration_sent(self.clock.now()?)
                    .map_err(Error::Consent)?;
            }
            Ok(admitted)
        })();
        self.checked(result)
    }
    /// The first possible process spawn follows a matching positive host ACK.
    /// Native setup refusal fences this talk epoch and requires fresh consent.
    pub fn configured(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let result = (|| {
            if self.state != State::AwaitingHost || self.capture.is_some() {
                return Err(Error::Order);
            }
            let offer = self.offer.ok_or(Error::Order)?;
            let permit = Arc::new(
                self.sender
                    .configured(bytes, self.clock.now()?)
                    .map_err(Error::Consent)?,
            );
            let clock = self.clock;
            let monitored = permit.monitor();
            let capture = MicrophoneCapture::start(
                &self.image,
                CaptureOptions {
                    server: self.source.server.clone(),
                    source: self.source.name.clone(),
                    configuration: permit.configuration(),
                    bitrate: self.source.bitrate,
                    max_packet_bytes: u16::try_from(offer.max_packet_bytes)
                        .map_err(|_| Error::Order)?,
                },
                self.clock.deadline(permit.local_deadline())?,
                move || clock.now().is_ok_and(|now| monitored.valid(now)),
            )
            .map_err(Error::Capture)?;
            self.retirement = Some(capture.retirement());
            self.capture = Some(capture);
            self.permit = Some(permit);
            self.state = State::Starting;
            Ok(())
        })();
        self.checked(result)
    }
    /// A bounded scheduling turn. At most four whole records reach `send`;
    /// neither a blocked transport nor a missing native reply starts a new pull.
    pub fn service(
        &mut self,
        mut send: impl FnMut(&[u8], ClientInstant) -> Result<bool, ()>,
    ) -> Result<Progress, Error> {
        let result = self.service_inner(&mut send);
        self.checked(result)
    }
    fn service_inner(
        &mut self,
        send: &mut impl FnMut(&[u8], ClientInstant) -> Result<bool, ()>,
    ) -> Result<Progress, Error> {
        // Ordinary session ticks while muted or awaiting the exact ACK never
        // open a device or consume future local talk consent. They still check
        // original authority and the unchanged configuration timeout.
        if matches!(self.state, State::Disabled | State::AwaitingHost) {
            self.sender
                .pending_configuration(self.clock.now()?)
                .map_err(Error::Consent)?;
            return Ok(Progress::default());
        }
        if self.collect_renewal()? {
            return Ok(Progress::default());
        }
        let permit = self.permit.as_ref().ok_or(Error::Order)?;
        permit.check(self.clock.now()?).map_err(Error::Consent)?;
        let clock = self.clock;
        let mut permitted = || clock.permits(permit);
        let capture = self.capture.as_mut().ok_or(Error::Order)?;
        if self.state == State::Starting {
            if !capture.poll_ready(&mut permitted).map_err(Error::Capture)? {
                return Ok(Progress::default());
            }
            self.state = State::Ready;
        }
        let mut progress = Progress::default();
        loop {
            if self.sender.has_pending_record() {
                let admitted = self
                    .sender
                    .try_send(self.clock.now()?, |bytes, until| {
                        catch_unwind(AssertUnwindSafe(|| send(bytes, until))).unwrap_or(Err(()))
                    })
                    .map_err(Error::Consent)?;
                if !admitted {
                    progress.blocked = true;
                    return Ok(progress);
                }
                progress.admitted_packets += 1;
                if progress.admitted_packets == TURN_PACKETS {
                    return Ok(progress);
                }
            }
            if let Some(batch) = &mut self.packets {
                if let Some(packet) = batch.packets.next() {
                    self.sender
                        .submit(&mut batch.receipt, &packet, self.clock.now()?)
                        .map_err(Error::Consent)?;
                    continue;
                }
                self.packets = None;
            }
            if self.request.is_some() {
                let Some(batch) = capture.poll_batch(&mut permitted).map_err(Error::Capture)?
                else {
                    return Ok(progress);
                };
                let mut receipt = self.request.take().ok_or(Error::Order)?;
                receipt
                    .restrict_expiry(self.clock.lower_deadline(batch.expires())?)
                    .map_err(Error::Consent)?;
                let packets = batch.consume(&mut permitted).map_err(Error::Capture)?;
                if packets.len() > TURN_PACKETS {
                    return Err(Error::Order);
                }
                if !packets.is_empty() {
                    self.packets = Some(Packets {
                        receipt,
                        packets: packets.into_iter(),
                    });
                    continue;
                }
            }
            let now = self.clock.now()?;
            if let Some(successor) = self
                .sender
                .refresh_capture(permit, now)
                .map_err(Error::Consent)?
            {
                let successor = Arc::new(successor);
                capture
                    .renew_authority(self.clock.deadline(successor.local_deadline())?, || {
                        clock.permits(&successor)
                    })
                    .map_err(Error::Capture)?;
                self.pending_permit = Some(successor);
                return Ok(progress);
            }
            let until = ClientInstant(
                now.0
                    .checked_add(CUSTODY_US)
                    .ok_or(Error::Clock)?
                    .min(permit.local_deadline().0),
            );
            let receipt = self
                .sender
                .begin_capture(permit, now, until)
                .map_err(Error::Consent)?;
            capture
                .request(self.clock.deadline(until)?, &mut permitted)
                .map_err(Error::Capture)?;
            self.request = Some(receipt);
            return Ok(progress);
        }
    }
    /// Publish the new immutable egress proof only after the exact child ACK.
    /// Yield this turn so the owner can snapshot it before any new admission.
    fn collect_renewal(&mut self) -> Result<bool, Error> {
        let Some(proposed) = self.pending_permit.as_ref() else {
            return Ok(false);
        };
        let clock = self.clock;
        let capture = self.capture.as_mut().ok_or(Error::Order)?;
        if capture
            .poll_renewed(|| clock.permits(proposed))
            .map_err(Error::Capture)?
        {
            self.permit = self.pending_permit.take();
        }
        Ok(true)
    }
    /// Fences locally first. The caller may send this stop on its original
    /// reliable attachment; network availability never delays local muting.
    pub fn mute(&mut self) -> Option<AudioStop> {
        let stop = self.sender.mute();
        self.stop_capture();
        self.offer = None;
        self.state = State::Disabled;
        stop
    }
    pub fn device_changed(&mut self) {
        self.sender.device_changed();
        self.stop_capture();
        self.offer = None;
        self.state = State::Disabled;
    }
    pub fn receive_stop(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let result = self
            .clock
            .now()
            .and_then(|now| self.sender.receive_stop(bytes, now).map_err(Error::Consent));
        self.stop_capture();
        self.offer = None;
        self.state = State::Disabled;
        self.checked(result)
    }
    pub fn stop(&mut self) {
        self.sender.stop();
        self.stop_capture();
        self.offer = None;
        self.state = State::Stopped;
    }
    fn stop_capture(&mut self) {
        if let Some(mut capture) = self.capture.take() {
            capture.stop();
        }
        self.permit = None;
        self.pending_permit = None;
        self.request = None;
        self.packets = None;
    }
    fn checked<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if result.is_err() {
            self.stop();
        }
        result
    }
}
impl Drop for Transmitter {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_mapping_never_extends_original_native_expiry() {
        let clock = Clock::bind(|| ClientInstant(1_000_000));
        for us in [1, 1_000, 100_000, 3_000_000] {
            let until = ClientInstant(clock.client.0 + us);
            let native = clock.deadline(until).unwrap();
            assert_eq!(clock.lower_deadline(native).unwrap(), until);
            let earlier = native.checked_sub(Duration::from_nanos(1)).unwrap();
            assert!(clock.lower_deadline(earlier).unwrap().0 < until.0);
        }
        assert!(clock.now().unwrap().0 >= clock.client.0);
        assert_eq!(clock.deadline(ClientInstant(0)), Err(Error::Clock));
    }
}

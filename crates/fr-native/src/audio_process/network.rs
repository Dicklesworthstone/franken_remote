#![forbid(unsafe_code)]
//! An explicitly enabled microphone on its original authenticated connection.
//!
//! The accepted `InputClient` creates the authority owner; the completed
//! `AudioUpChannel` supplies routing evidence. Neither replaces local talk
//! consent, OS permission, or the host's separately approved endpoint. The
//! real capture child starts only after the exact reliable host acknowledgement.
//! Every admitted datagram retains its immutable capture permit and ORIGINAL
//! custody deadline until the native transport proves it has drained.
//!
//! The containing session must call `service` between I/O turns and compose
//! `permits_io` with its existing control authorization on EVERY connection
//! operation, or use `drive`. Stopping an optional lane cannot retract a native
//! datagram or protected packet: if protected audio may remain, failure closes
//! the original connection before further I/O. A proven drained stop retires
//! only this microphone lane. This module does not advertise a public offer.

mod guard;
#[cfg(feature = "linux-microphone-session")]
mod session;
#[cfg(feature = "linux-microphone-session")]
pub use session::CaptureRetirement;

use super::{
    Retirement,
    transmit::{self, ConfigurationEgress, Egress, Source, Transmitter},
};
use asupersync::cx::Cx;
use fr_client::{
    audio::uplink::{self, Profile},
    input::{ClientInstant, InputClient, presentation::PresentedInput},
};
use fr_core::audio::{
    AudioDirection, AudioGeneration, AudioStopReason, MicPermission, MicTalkMode,
};
use fr_transport::quic::{
    self, ConnectionBinding, Disposition, QuicRecords, SendAdmission,
    audio::{AudioUpChannel, Side},
};
use fr_wire::{
    audio::{self, AudioConfiguration, AudioStop},
    negotiation::Selection,
};
use std::{path::PathBuf, time::Duration};

/// A capture turn contains at most four packets. Do not consume another turn
/// while these immutable native-send guards still occupy their bounded slots.
const INFLIGHT_PACKETS: usize = uplink::MAX_CAPTURE_PACKETS as usize;
const STOP_LIFETIME_US: u64 = uplink::CONFIGURATION_TIMEOUT_US;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    WrongConnection,
    WrongScope,
    Busy,
    Clock,
    Closed,
    InvalidRecord,
    /// Only the original optional microphone pair exceeded its receive deadline.
    ReceiveExpired,
    EgressExpired,
    Capture(transmit::Error),
    Consent(uplink::Error),
    Transport(quic::Error),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "microphone-network: {self:?}")
    }
}
impl std::error::Error for Error {}

/// Constructor refusal retains custody of the consumed routing proof. A
/// foreign connection refusal returns it unchanged; a refusal after proving
/// the original connection returns a retired optional lane. Neither case can
/// silently abandon a live attachment in another connection.
pub struct AttachError {
    reason: Error,
    lane: Box<AudioUpChannel>,
}
impl std::fmt::Debug for AttachError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MicrophoneAttachError")
            .field("reason", &self.reason)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for AttachError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.reason)
    }
}
impl std::error::Error for AttachError {}
impl AttachError {
    pub const fn reason(&self) -> Error {
        self.reason
    }
    pub fn into_parts(self) -> (Error, AudioUpChannel) {
        (self.reason, *self.lane)
    }
}

/// Local capture selection. Paths and device names come from the trusted local
/// application, never from an attachment or an audio configuration record.
pub struct Options {
    pub image: PathBuf,
    pub source: Source,
    pub profile: Profile,
}
impl std::fmt::Debug for Options {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MicrophoneNetworkOptions")
            .field("profile", &self.profile)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Muted,
    AwaitingHost,
    Starting,
    Talking,
    Stopped,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub state: State,
    pub admitted_packets: usize,
    pub blocked: bool,
}

#[derive(Clone, Copy)]
struct Clock {
    transport: u64,
    client: ClientInstant,
}
impl Clock {
    fn deadline(self, until: ClientInstant) -> Result<u64, Error> {
        // The transport sample precedes the client sample. Mapping onto it can
        // shorten a deadline by the sampling interval, never extend custody.
        self.transport
            .checked_add(until.0.checked_sub(self.client.0).ok_or(Error::Clock)?)
            .ok_or(Error::Clock)
    }
}
fn now(cx: &Cx) -> Result<u64, Error> {
    cx.timer_driver()
        .map(|driver| driver.now().as_nanos() / 1000)
        .ok_or(Error::Clock)
}
#[derive(Clone)]
struct PendingEgress {
    permit: Egress,
    until: u64,
}
impl PendingEgress {
    fn live(&self, at: u64) -> bool {
        at < self.until && self.permit.is_open()
    }
}
struct PendingStop {
    bytes: [u8; audio::AUDIO_STOP_RECORD_BYTES],
    until: u64,
}

/// One non-cloneable capture/network owner. Dropping it stops recording and
/// invalidates the attachment; it never joins or waits for a native destructor.
pub struct Uplink {
    lane: AudioUpChannel,
    connection: ConnectionBinding,
    transmitter: Transmitter,
    cx: Cx,
    clock: Clock,
    permission: MicPermission,
    generation: Option<AudioGeneration>,
    muted_through: AudioGeneration,
    configuration_pending: bool,
    configuration_sent: bool,
    configuration_egress: Option<ConfigurationEgress>,
    control_until: Option<u64>,
    pending_stop: Option<PendingStop>,
    inflight: [Option<PendingEgress>; INFLIGHT_PACKETS],
    retired: bool,
    reason: Option<Error>,
    io: guard::Shared,
    #[cfg(feature = "linux-microphone-session")]
    observer: Option<CaptureRetirement>,
}
impl std::fmt::Debug for Uplink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MicrophoneUplink")
            .field("state", &self.state())
            .field("reason", &self.reason)
            .finish_non_exhaustive()
    }
}
impl Uplink {
    /// Attach a muted microphone to the ORIGINAL accepted controller. `now`
    /// samples that controller's client clock, including any original offset;
    /// no peer timestamp or newly invented clock is an authority substitute.
    /// No device, codec process, or OS permission dialog opens here.
    #[allow(clippy::too_many_arguments)]
    pub fn attach(
        q: &mut QuicRecords,
        lane: AudioUpChannel,
        input: &mut InputClient,
        selection: &Selection,
        host_granted: bool,
        options: Options,
        cx: &Cx,
        client_now: impl FnMut() -> ClientInstant,
    ) -> Result<Self, AttachError> {
        Self::attach_with(
            q,
            lane,
            selection,
            options,
            cx,
            client_now,
            |lane, profile, at| {
                input.attach_microphone(
                    lane.parent(),
                    lane.channel(),
                    selection,
                    host_granted,
                    profile,
                    at,
                )
            },
        )
    }
    /// The same network/capture owner, attached through the original
    /// decoder-backed input owner retained by a running `ControlledViewer`.
    #[allow(clippy::too_many_arguments)]
    pub fn attach_presented(
        q: &mut QuicRecords,
        lane: AudioUpChannel,
        input: &mut PresentedInput,
        selection: &Selection,
        host_granted: bool,
        options: Options,
        cx: &Cx,
        client_now: impl FnMut() -> ClientInstant,
    ) -> Result<Self, AttachError> {
        Self::attach_with(
            q,
            lane,
            selection,
            options,
            cx,
            client_now,
            |lane, profile, at| {
                input.attach_microphone(
                    lane.parent(),
                    lane.channel(),
                    selection,
                    host_granted,
                    profile,
                    at,
                )
            },
        )
    }
    fn attach_with(
        q: &mut QuicRecords,
        mut lane: AudioUpChannel,
        selection: &Selection,
        options: Options,
        cx: &Cx,
        mut client_now: impl FnMut() -> ClientInstant,
        sender: impl FnOnce(
            &AudioUpChannel,
            Profile,
            ClientInstant,
        ) -> Result<uplink::Sender, uplink::Error>,
    ) -> Result<Self, AttachError> {
        if let Err(error) = lane.check(q) {
            return Err(AttachError {
                reason: Error::Transport(error),
                lane: Box::new(lane),
            });
        }
        let setup = (|| {
            if lane.side() != Side::Controller
                || lane.limits() != selection.limits
                || options.profile.max_record_bytes > lane.maximum_record_bytes()
                || options.profile.max_record_bytes > lane.packet_maximum()
            {
                return Err(Error::WrongScope);
            }
            let transport = now(cx)?;
            let sender = sender(&lane, options.profile, client_now()).map_err(Error::Consent)?;
            let mut client = None;
            let transmitter = Transmitter::new(sender, &options.image, options.source, || {
                let sampled = client_now();
                client = Some(sampled);
                sampled
            });
            Ok((
                transmitter,
                Clock {
                    transport,
                    client: client.expect("constructor samples original clock once"),
                },
            ))
        })();
        let (transmitter, clock) = match setup {
            Ok(ready) => ready,
            Err(mut reason) => {
                if let Err(error) = lane.retire(q, cx) {
                    q.close();
                    reason = Error::Transport(error);
                }
                return Err(AttachError {
                    reason,
                    lane: Box::new(lane),
                });
            }
        };
        let io = guard::Shared::new(cx.clone(), lane.retirement_request(), clock.transport);
        Ok(Self {
            connection: q.binding(),
            lane,
            transmitter,
            cx: cx.clone(),
            clock,
            permission: MicPermission::NotRequested,
            generation: None,
            muted_through: AudioGeneration::INITIAL,
            configuration_pending: false,
            configuration_sent: false,
            configuration_egress: None,
            control_until: None,
            pending_stop: None,
            inflight: std::array::from_fn(|_| None),
            retired: false,
            reason: None,
            io,
            #[cfg(feature = "linux-microphone-session")]
            observer: None,
        })
    }
    pub fn state(&self) -> State {
        if self.retired {
            return State::Stopped;
        }
        match self.transmitter.state() {
            transmit::State::Disabled => State::Muted,
            transmit::State::AwaitingHost => State::AwaitingHost,
            transmit::State::Starting => State::Starting,
            transmit::State::Ready => State::Talking,
            transmit::State::Stopped => State::Stopped,
        }
    }
    pub const fn permission(&self) -> MicPermission {
        self.permission
    }
    pub const fn reason(&self) -> Option<Error> {
        self.reason
    }
    /// Route classification for the containing connection's other dispatchers;
    /// it is not authority or proof of a different connection's equal IDs.
    pub fn owns_inbound(&self, route: quic::Route) -> bool {
        self.lane.owns_inbound(route)
    }
    /// Actual eventual wait/reap evidence; admission stopping is immediate and
    /// does not claim the foreign child has already exited.
    pub fn retirement(&self) -> Option<Retirement> {
        self.transmitter.retirement()
    }
    /// Bind the original visible local UI before any permission/talk action.
    /// Every new talk uses the UI's fresh token; a token change invalidates all
    /// earlier native/queued proofs even if the mute interval was not sampled.
    pub fn bind_local_talk_check(&mut self, check: uplink::LocalTalkCheck) -> Result<(), Error> {
        if self.retired
            || self.permission != MicPermission::NotRequested
            || self.generation.is_some()
            || self.state() != State::Muted
        {
            return Err(Error::Busy);
        }
        self.transmitter.set_local_talk_check(check);
        Ok(())
    }
    /// Record a local OS permission result. Permission recovery is still muted
    /// and requires another explicit `begin_talk` action.
    pub fn set_permission(&mut self, permission: MicPermission) -> Result<(), Error> {
        let muted = if permission == MicPermission::Granted {
            Ok(())
        } else {
            self.mute()
        };
        self.permission = permission;
        self.transmitter.set_permission(permission);
        muted
    }
    /// Local visible talk action only. Queue capacity, a later OS grant, or a
    /// late host reply never retries this intent automatically.
    pub fn begin_talk(&mut self, mode: MicTalkMode) -> Result<AudioConfiguration, Error> {
        if self.retired {
            return Err(Error::Closed);
        }
        if self.pending_stop.is_some() || self.control_until.is_some() {
            return Err(Error::Busy);
        }
        let offer = self.transmitter.begin_talk(mode).map_err(Error::Capture)?;
        self.generation = Some(offer.generation);
        self.configuration_pending = true;
        self.configuration_sent = false;
        Ok(offer)
    }
    /// Fence capture FIRST. A bounded reliable stop is only a notification;
    /// neither backpressure nor a missing peer can delay local muting.
    pub fn mute(&mut self) -> Result<(), Error> {
        let notification = self.transmitter.mute();
        self.finish_local_mute(notification)
    }
    pub fn device_changed(&mut self) -> Result<(), Error> {
        let mut notification = self.transmitter.mute();
        self.transmitter.device_changed();
        self.permission = MicPermission::NotRequested;
        if let Some(stop) = &mut notification {
            stop.reason = AudioStopReason::DeviceChanged;
        }
        self.finish_local_mute(notification)
    }
    fn finish_local_mute(&mut self, notification: Option<AudioStop>) -> Result<(), Error> {
        self.configuration_pending = false;
        if let Some(generation) = self.generation.take() {
            self.muted_through = self.muted_through.max(generation);
        }
        let was_sent = std::mem::replace(&mut self.configuration_sent, false);
        if let Some(stop) = notification.filter(|_| was_sent && !self.retired) {
            let mut bytes = [0; audio::AUDIO_STOP_RECORD_BYTES];
            audio::encode_stop(&stop, self.lane.channel(), &mut bytes)
                .map_err(|_| Error::InvalidRecord)?;
            let until = now(&self.cx)?
                .checked_add(STOP_LIFETIME_US)
                .ok_or(Error::Clock)?;
            self.pending_stop = Some(PendingStop { bytes, until });
        }
        self.publish_guard();
        Ok(())
    }
    /// Immediately end local admission. Call `service`/`retire` between I/O
    /// turns to finish optional transport retirement; retained guards stay live
    /// objects until exact drain evidence or original-connection closure.
    pub fn stop(&mut self) {
        self.transmitter.stop();
        self.configuration_pending = false;
        self.pending_stop = None;
        self.publish_guard();
    }
    fn bound(&self, q: &QuicRecords) -> Result<(), Error> {
        if q.is_bound_to(&self.connection) {
            Ok(())
        } else {
            Err(Error::WrongConnection)
        }
    }
    /// Compose this with the containing session's existing original-control
    /// gate on every native I/O poll. An immutable packet deadline is never
    /// extended by a newer capture permit, a queue retry or a later ACK.
    pub fn permits_io(&self) -> bool {
        self.io.permits()
    }
    /// A retained read-only gate for the original transport's every operation.
    /// The snapshot contains only the same immutable guards already owned here,
    /// never replacement authority or payload. Drain/retirement updates are
    /// published between operations, with no lock held over a transport call.
    pub fn operation_check(&self) -> std::sync::Arc<dyn Fn() -> bool + Send + Sync> {
        self.io.operation_check()
    }
    fn snapshot(&self) -> guard::Snapshot {
        guard::Snapshot {
            inflight: self.inflight.clone(),
            configuration: self.configuration_egress.clone(),
            control_until: self.control_until,
            stop_until: self.pending_stop.as_ref().map(|stop| stop.until),
            retired: self.retired,
        }
    }
    fn publish_guard(&self) {
        self.io.publish(self.snapshot());
    }
    /// Explicit terminal retirement. If the native stack may still own an
    /// audio datagram/protected packet, conservatively close its original
    /// connection; deleting only its route cannot retract those bytes.
    pub fn retire(&mut self, q: &mut QuicRecords) -> Result<(), Error> {
        self.bound(q)?;
        self.stop();
        if self.retired {
            return Ok(());
        }
        let drained = self.inflight.iter().all(Option::is_none)
            || self.lane.datagrams_send_drained(q).unwrap_or(false);
        if !drained {
            q.close();
            self.retired = true;
            self.inflight.fill_with(|| None);
            self.control_until = None;
            self.configuration_egress = None;
            self.publish_guard();
            return Err(Error::EgressExpired);
        }
        // Drop packet guards only on the actual native drain witness above.
        // Keep reliable guards and the non-retired snapshot across retirement:
        // even an unwinding original security callback must not publish a
        // successful optional reset that never happened.
        self.inflight.fill_with(|| None);
        self.publish_guard();
        if q.is_closed() {
            self.retired = true;
            self.control_until = None;
            self.configuration_egress = None;
            self.publish_guard();
            return Ok(());
        }
        let retired = self.lane.retire(q, &self.cx).map_err(Error::Transport);
        if retired.is_err() {
            q.close();
        }
        self.retired = true;
        self.control_until = None;
        self.configuration_egress = None;
        self.publish_guard();
        retired
    }
    fn fail(&mut self, q: &mut QuicRecords, reason: Error) -> Result<(), Error> {
        let connection_closed = q.is_closed();
        // A send's final admission check can retire the pair after the owner
        // preflight. Preserve its exact terminal cause without relabeling a
        // closed parent, lost authority, or retained microphone output.
        let reason = if matches!(
            reason,
            Error::Transport(
                quic::Error::Closed
                    | quic::Error::WrongRoute
                    | quic::Error::Stream(fr_wire::stream::StreamError::Expired)
            )
        ) && self.lane.is_retired(q) == Ok(true)
            && self.lane.receive_expired(q)
        {
            Error::ReceiveExpired
        } else {
            reason
        };
        self.reason = Some(reason);
        self.retire(q)?;
        if connection_closed {
            return Err(reason);
        }
        Ok(())
    }
    /// Consume exact native drain witnesses before other session lanes run.
    /// This never starts capture or refreshes an admitted packet's deadline.
    pub fn maintain(&mut self, q: &mut QuicRecords) -> Result<(), Error> {
        self.bound(q)?;
        if self.retired {
            if !self.lane.is_retired(q).map_err(Error::Transport)? {
                return Err(Error::Transport(quic::Error::WrongRoute));
            }
            return Ok(());
        }
        // Consume native drain evidence BEFORE testing old guards. A guard
        // whose bytes were completely flushed no longer fences unrelated I/O.
        if self.inflight.iter().any(Option::is_some)
            && self.lane.datagrams_send_drained(q).unwrap_or(false)
        {
            self.inflight.fill_with(|| None);
        }
        if (self.control_until.is_some() || self.configuration_egress.is_some())
            && self.lane.control_send_drained(q).unwrap_or(false)
        {
            self.control_until = None;
            self.configuration_egress = None;
        }
        self.publish_guard();
        if let Err(error) = self.lane.check(q) {
            if self.lane.is_retired(q) == Ok(true) {
                let reason = if self.lane.receive_expired(q) {
                    Error::ReceiveExpired
                } else {
                    Error::Closed
                };
                return self.fail(q, reason);
            }
            return Err(Error::Transport(error));
        }
        let at = match now(&self.cx) {
            Ok(at) => at,
            Err(reason) => return self.fail(q, reason),
        };
        // The real UI/native monitor can withdraw talk before the session's
        // next application callback. After consuming original drain witnesses,
        // use the same bounded UserMute/AudioStop path as an explicit callback.
        // All still-retained configuration/datagram proofs below stay intact:
        // withdrawing local talk cannot reauthorize bytes already in custody.
        if self.transmitter.local_talk_withdrawn() {
            self.mute()?;
        }
        if !self.inflight.iter().flatten().all(|guard| guard.live(at)) {
            return self.fail(q, Error::EgressExpired);
        }
        if self.control_until.is_some_and(|until| at >= until)
            || self
                .configuration_egress
                .as_ref()
                .is_some_and(|guard| !guard.is_open())
            || self
                .pending_stop
                .as_ref()
                .is_some_and(|stop| at >= stop.until)
        {
            return self.fail(q, Error::EgressExpired);
        }
        if self.transmitter.state() == transmit::State::Stopped {
            return self.fail(q, Error::Closed);
        }
        Ok(())
    }
    /// Bounded, nonblocking network/capture turn: one host reply and at most
    /// four original capture packets. No native call or wait runs here.
    pub fn service(
        &mut self,
        q: &mut QuicRecords,
        mut authorize: impl FnMut() -> bool,
    ) -> Result<Progress, Error> {
        // Refuse a foreign object before changing EITHER connection or owner.
        self.bound(q)?;
        let mut guard = Drive {
            uplink: self,
            q,
            complete: false,
        };
        let result = guard.uplink.service_inner(guard.q, &mut authorize);
        if let Err(reason) = result {
            guard.uplink.reason.get_or_insert(reason);
            guard.uplink.stop();
            guard.uplink.retired = true;
            guard.q.close();
            guard.uplink.publish_guard();
        }
        guard.complete = true;
        result
    }
    fn service_inner(
        &mut self,
        q: &mut QuicRecords,
        authorize: &mut impl FnMut() -> bool,
    ) -> Result<Progress, Error> {
        self.maintain(q)?;
        if self.retired {
            return Ok(self.progress(0, false));
        }
        self.dispatch(q, authorize)?;
        if self.retired {
            return Ok(self.progress(0, false));
        }
        self.send_stop(q, authorize)?;
        if self.configuration_pending {
            self.send_configuration(q, authorize)?;
        }
        if self.retired {
            return Ok(self.progress(0, false));
        }
        let permit = self.transmitter.egress();
        let snapshot = self.snapshot();
        let Self {
            transmitter,
            lane,
            cx,
            clock,
            inflight,
            io,
            ..
        } = self;
        let mut failure = None;
        let result = transmitter.service(|bytes, until| {
            let Some(permit) = &permit else {
                failure = Some(Error::InvalidRecord);
                return Err(());
            };
            if inflight.iter().all(Option::is_some) {
                return Ok(false);
            }
            let until = match clock.deadline(until) {
                Ok(until) => until,
                Err(reason) => {
                    failure = Some(reason);
                    return Err(());
                }
            };
            let prepared = || {
                let at = now(cx)?;
                if at >= until || !permit.is_open() {
                    return Err(Error::EgressExpired);
                }
                Ok(())
            };
            let admitted = lane.send_packet_prepared(
                q,
                cx,
                bytes,
                until,
                || {
                    now(cx).is_ok_and(|at| inflight.iter().flatten().all(|p| p.live(at)))
                        && authorize()
                },
                prepared,
            );
            match admitted {
                Ok(SendAdmission::Accepted) => {
                    *inflight
                        .iter_mut()
                        .find(|slot| slot.is_none())
                        .expect("bounded admission") = Some(PendingEgress {
                        permit: permit.clone(),
                        until,
                    });
                    // Publish after EACH admission, before the next packet or
                    // any unrelated operation can observe the original socket.
                    io.publish(guard::Snapshot {
                        inflight: inflight.clone(),
                        ..snapshot.clone()
                    });
                    Ok(true)
                }
                Err(quic::Error::Backpressure) => Ok(false),
                Ok(SendAdmission::Refused(reason)) => {
                    failure = Some(reason);
                    Err(())
                }
                Err(error) => {
                    failure = Some(Error::Transport(error));
                    Err(())
                }
            }
        });
        match result {
            Ok(progress) => Ok(self.progress(progress.admitted_packets, progress.blocked)),
            Err(error) => {
                self.fail(q, failure.unwrap_or(Error::Capture(error)))?;
                Ok(self.progress(0, false))
            }
        }
    }
    fn progress(&self, admitted_packets: usize, blocked: bool) -> Progress {
        Progress {
            state: self.state(),
            admitted_packets,
            blocked,
        }
    }
    fn send_configuration(
        &mut self,
        q: &mut QuicRecords,
        authorize: &mut impl FnMut() -> bool,
    ) -> Result<(), Error> {
        let Some(egress) = self.transmitter.configuration_egress() else {
            return self.fail(q, Error::InvalidRecord);
        };
        let Self {
            transmitter,
            lane,
            cx,
            clock,
            control_until,
            configuration_egress,
            ..
        } = self;
        let mut failure = None;
        let result = transmitter.send_configuration(|bytes, until| {
            let until = match clock.deadline(until) {
                Ok(until) => until,
                Err(reason) => {
                    failure = Some(reason);
                    return Err(());
                }
            };
            match lane.send_control_prepared(q, cx, bytes, until, authorize, || {
                if egress.is_open() {
                    Ok(())
                } else {
                    Err(Error::EgressExpired)
                }
            }) {
                Ok(SendAdmission::Accepted) => {
                    *control_until = Some(control_until.map_or(until, |old| old.min(until)));
                    *configuration_egress = Some(egress);
                    Ok(true)
                }
                Err(quic::Error::Backpressure) => Ok(false),
                Ok(SendAdmission::Refused(reason)) => {
                    failure = Some(reason);
                    Err(())
                }
                Err(error) => {
                    failure = Some(Error::Transport(error));
                    Err(())
                }
            }
        });
        self.publish_guard();
        match result {
            Ok(true) => {
                self.configuration_pending = false;
                self.configuration_sent = true;
                Ok(())
            }
            Ok(false) => Ok(()),
            Err(error) => self.fail(q, failure.unwrap_or(Error::Capture(error))),
        }
    }
    fn send_stop(
        &mut self,
        q: &mut QuicRecords,
        authorize: &mut impl FnMut() -> bool,
    ) -> Result<(), Error> {
        let Some(stop) = &self.pending_stop else {
            return Ok(());
        };
        match self.lane.send_control_prepared(
            q,
            &self.cx,
            &stop.bytes,
            stop.until,
            || self.permits_io() && authorize(),
            || Ok::<_, Error>(()),
        ) {
            Ok(SendAdmission::Accepted) => {
                self.control_until = Some(
                    self.control_until
                        .map_or(stop.until, |old| old.min(stop.until)),
                );
                self.pending_stop = None;
                self.publish_guard();
                Ok(())
            }
            Err(quic::Error::Backpressure) => Ok(()),
            Ok(SendAdmission::Refused(reason)) => self.fail(q, reason),
            Err(error) => self.fail(q, Error::Transport(error)),
        }
    }
    fn dispatch(
        &mut self,
        q: &mut QuicRecords,
        authorize: &mut impl FnMut() -> bool,
    ) -> Result<(), Error> {
        let mut record = [0; audio::AUDIO_CONFIGURED_RECORD_BYTES];
        let mut length = 0;
        let mut invalid = false;
        // Copy one bounded reliable record. Its callback never spawns a worker
        // or holds a borrow across native transport dispatch.
        let received = self.lane.dispatch(
            q,
            &self.cx,
            || self.permits_io() && authorize(),
            |_| true,
            |_, bytes, receipt| {
                if receipt.is_some() || bytes.len() > record.len() {
                    invalid = true;
                    return Ok(Disposition::Consumed);
                }
                length = bytes.len();
                record[..length].copy_from_slice(bytes);
                Ok(Disposition::Consumed)
            },
        );
        if let Err(error) = received {
            if self.lane.is_retired(q) == Ok(true)
                && (matches!(error, quic::Error::WrongRoute | quic::Error::Closed)
                    || (error == quic::Error::Stream(fr_wire::stream::StreamError::Expired)
                        && self.lane.receive_expired(q)))
            {
                // An original configuration witness may close inside a gate.
                // Only the existing retired reservation proves optional
                // isolation; parent-control closure and closed q stay fatal.
                let reason = if self.lane.receive_expired(q) {
                    Error::ReceiveExpired
                } else {
                    Error::Closed
                };
                return self.fail(q, reason);
            }
            return Err(Error::Transport(error));
        }
        if invalid {
            return self.fail(q, Error::InvalidRecord);
        }
        if length == 0 {
            return Ok(());
        }
        let bytes = &record[..length];
        match audio::record_kind(bytes) {
            Some(0x61) => {
                let Ok(ack) = audio::decode_configured(bytes, self.lane.channel()) else {
                    return self.fail(q, Error::InvalidRecord);
                };
                if ack.direction != AudioDirection::Uplink {
                    return self.fail(q, Error::InvalidRecord);
                }
                if ack.generation <= self.muted_through {
                    // A reply to locally cancelled consent can never start a
                    // source, nor poison a later explicit talk generation.
                    return Ok(());
                }
                if let Err(error) = self.transmitter.configured(bytes) {
                    self.fail(q, Error::Capture(error))?;
                }
            }
            Some(0x63) => {
                let Ok(stop) = audio::decode_stop(bytes, self.lane.channel()) else {
                    return self.fail(q, Error::InvalidRecord);
                };
                if stop.direction != AudioDirection::Uplink {
                    return self.fail(q, Error::InvalidRecord);
                }
                if self
                    .generation
                    .is_some_and(|generation| stop.generation < generation)
                    || (self.generation.is_none() && stop.generation < self.muted_through)
                {
                    return Ok(());
                }
                if self.generation == Some(stop.generation) {
                    if let Err(error) = self.transmitter.receive_stop(bytes) {
                        return self.fail(q, Error::Capture(error));
                    }
                } else if stop.generation != self.muted_through {
                    return self.fail(q, Error::InvalidRecord);
                }
                self.fail(q, Error::Closed)?;
            }
            _ => return self.fail(q, Error::InvalidRecord),
        }
        Ok(())
    }
    /// Drive the existing connection with both the containing session's gate
    /// and every original microphone egress guard. Dropping this future even
    /// before its first poll stops capture and closes only its original object.
    pub fn drive<'a>(
        &'a mut self,
        q: &'a mut QuicRecords,
        wait: Duration,
        mut authorize: impl FnMut() -> bool + 'a,
    ) -> impl std::future::Future<Output = Result<Progress, Error>> + 'a {
        let bound = self.bound(q);
        let mut guard = bound.is_ok().then(|| Drive {
            uplink: self,
            q,
            complete: false,
        });
        async move {
            bound?;
            let io = guard.as_mut().expect("bound original connection");
            let first = io.uplink.service(io.q, &mut authorize)?;
            if let Err(error) =
                io.q.drive(&io.uplink.cx, wait, || {
                    io.uplink.permits_io() && authorize()
                })
                .await
            {
                let reason = Error::Transport(error);
                io.uplink.reason = Some(reason);
                return Err(reason);
            }
            let mut last = io.uplink.service(io.q, &mut authorize)?;
            last.admitted_packets += first.admitted_packets;
            last.blocked |= first.blocked;
            io.complete = true;
            Ok(last)
        }
    }
}
impl Drop for Uplink {
    fn drop(&mut self) {
        self.transmitter.stop();
        #[cfg(feature = "linux-microphone-session")]
        if let Some(observer) = &self.observer {
            observer.record(self, true);
        }
    }
}
struct Drive<'a> {
    uplink: &'a mut Uplink,
    q: &'a mut QuicRecords,
    complete: bool,
}
impl Drop for Drive<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.uplink.stop();
            self.uplink.retired = true;
            self.uplink
                .reason
                .get_or_insert(Error::Transport(quic::Error::Cancelled));
            self.q.close();
            self.uplink.publish_guard();
        }
    }
}

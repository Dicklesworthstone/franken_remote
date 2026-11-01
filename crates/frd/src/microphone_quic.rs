#![forbid(unsafe_code)]
//! The authenticated `AudioUp` attachment joined to the original host controller
//! and two real, independently supervised native children. No listener, lease,
//! approval, endpoint discovery or capability advertisement is created here.
//!
//! Call [`Host::service`] between the existing connection's I/O turns and include
//! [`Host::permits_io`] in every original connection authorization callback. A
//! configuration ACK is admitted to that exact transport before private Commit;
//! packet custody keeps the transport's original immutable receipt and expiry.
use crate::{input_quic::QuicInput, microphone_process as native};
use asupersync::cx::Cx;
use fr_core::{audio::AudioStopReason, input_submission::Refusal, time::HostInstant};
use fr_media::{audio::authority::HostController, worker::microphone::Snapshot};
use fr_transport::quic::{
    self, ConnectionBinding, Disposition, QuicRecords, Route, SendAdmission, audio::AudioUpChannel,
};
use fr_wire::{audio, negotiation::Selection};
use std::sync::{
    Arc, Mutex, PoisonError,
    atomic::{AtomicBool, Ordering},
};

#[path = "microphone_quic/epoch.rs"]
mod epoch;
use epoch::{Epoch, Parameters};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    WrongConnection,
    WrongRole,
    NotSelected,
    Permission,
    Binding,
    Clock,
    Cancelled,
    Closed,
    Protocol,
    SetupExpired,
    AcknowledgementExpired,
    AcknowledgementUnknown,
    UnresolvedChildren,
    Authority(Refusal),
    Input(crate::input_quic::Error),
    Native(native::Error),
    RemoteStopped(AudioStopReason),
    Transport(quic::Error),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "host-microphone-quic: {self:?}")
    }
}
impl std::error::Error for Error {}

/// Construction failure retains ownership of the supplied proof. A foreign
/// connection cannot abandon the original live attachment as an error side
/// effect. Same-original policy refusals explicitly retire the optional pair;
/// `into_parts` then returns its tombstone alongside the typed reason.
#[derive(Debug)]
pub struct AttachError {
    reason: Error,
    lane: Box<AudioUpChannel>,
}
impl AttachError {
    fn new(reason: Error, lane: AudioUpChannel) -> Self {
        Self {
            reason,
            lane: Box::new(lane),
        }
    }
    pub const fn reason(&self) -> Error {
        self.reason
    }
    pub fn into_parts(self) -> (Error, AudioUpChannel) {
        (self.reason, *self.lane)
    }
}
impl std::fmt::Display for AttachError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.reason, f)
    }
}
impl std::error::Error for AttachError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Idle,
    Starting,
    Ready,
    Committing,
    Running,
    /// Both original children must report actual exits before a new epoch.
    Retiring,
    /// This attachment's identities are tombstones; it cannot be reopened.
    Retired,
}

/// Bounded numeric progress only. Native admission never claims an application
/// heard a packet; no audio bytes or history are retained for diagnostics.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Statistics {
    pub configurations_started: u64,
    pub acknowledgements_admitted: u64,
    pub packets_queued: u64,
    pub packets_admitted: u64,
    pub packets_ignored: u64,
}

#[derive(Default)]
struct Shared {
    stopped: AtomicBool,
    native: Mutex<Option<native::Control>>,
}
impl Shared {
    fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(native) = self
            .native
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            native.stop();
        }
    }
    fn clear_epoch(&self) {
        self.native
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
    }
    fn install(&self, native: native::Control) -> Result<(), Error> {
        let mut current = self.native.lock().unwrap_or_else(PoisonError::into_inner);
        if self.stopped.load(Ordering::Acquire) {
            native.stop();
            return Err(Error::Closed);
        }
        *current = Some(native);
        Ok(())
    }
    fn check(&self) -> Result<(), Error> {
        if self.stopped.load(Ordering::Acquire) {
            return Err(Error::Closed);
        }
        if self
            .native
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .is_some_and(native::Control::is_stopped)
        {
            return Err(Error::Native(native::Error::Stopped));
        }
        Ok(())
    }
}

/// A local revoke-only handle. It fences native output immediately, including
/// while the containing network task is idle. It cannot renew or re-enable an
/// epoch, and copies of an old handle never refer to a replacement attachment.
#[derive(Clone)]
pub struct Control(Arc<Shared>);
impl Control {
    pub fn stop(&self) {
        self.0.stop();
    }
    pub fn is_stopped(&self) -> bool {
        self.0.check().is_err()
    }
}

/// One completed `AudioUp` pair, one native epoch, one fixed-size ACK and one
/// native packet handoff. A newer configuration stays in the transport's
/// existing bounded reliable slot until old children and ACK bytes retire.
pub struct Host {
    lane: AudioUpChannel,
    connection: ConnectionBinding,
    cx: Cx,
    controller: HostController,
    shared: Arc<Shared>,
    selection: Selection,
    launch: native::Launch,
    epoch: Epoch,
    retired: bool,
    reason: Option<Error>,
}
impl std::fmt::Debug for Host {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostMicrophoneQuic")
            .field("state", &self.state())
            .field("generation", &self.epoch.floor)
            .finish_non_exhaustive()
    }
}
impl Drop for Host {
    fn drop(&mut self) {
        self.shared.stop();
        self.epoch.stop(&self.shared);
        // Without a mutable original connection, the channel's conservative
        // Drop fences that connection. Explicit retire() preserves other lanes.
    }
}
impl Host {
    /// `approved` is separate local microphone permission, never input consent.
    /// `input` and `lane` must belong to this exact existing QUIC connection.
    /// Its clock is obtained from that original input owner, never from the
    /// caller. The containing session retains its admission and input driver.
    pub fn new(
        q: &mut QuicRecords,
        mut lane: AudioUpChannel,
        input: &QuicInput,
        selection: &Selection,
        approved: bool,
        launch: native::Launch,
    ) -> Result<Self, AttachError> {
        if let Err(error) = lane.check(q) {
            return Err(AttachError::new(Error::Transport(error), lane));
        }
        let cx = match input.microphone_context(q) {
            Ok(cx) => cx,
            Err(error) => {
                // The supplied input owner cannot provide this connection's
                // clock. Return its unchanged proof for the actual owner to
                // recover; never retire using another connection's context.
                return Err(AttachError::new(Error::Input(error), lane));
            }
        };
        let shared = Arc::new(Shared::default());
        let setup = (|| {
            if !lane.is_host() {
                return Err(Error::WrongRole);
            }
            if !approved {
                return Err(Error::Permission);
            }
            if !fr_wire::audio_up::uplink_selected(selection) {
                return Err(Error::NotSelected);
            }
            if lane.limits() != selection.limits {
                return Err(Error::Binding);
            }
            let controller = input.microphone_controller(q).map_err(Error::Input)?;
            if controller.scope().session != lane.parent().remote_session {
                return Err(Error::Binding);
            }
            admission(&cx, &controller, &shared)?;
            Ok(controller)
        })();
        let controller = match setup {
            Ok(controller) => controller,
            Err(reason) => {
                // The original completed pair was proved above and no native
                // owner was started. Refusal retires this optional pair only.
                let reason = match lane.retire(q, &cx) {
                    Ok(()) => reason,
                    Err(error) => Error::Transport(error),
                };
                return Err(AttachError::new(reason, lane));
            }
        };
        Ok(Self {
            lane,
            connection: q.binding(),
            cx,
            controller,
            shared,
            selection: selection.clone(),
            launch,
            epoch: Epoch::default(),
            retired: false,
            reason: None,
        })
    }
    pub fn control(&self) -> Control {
        Control(self.shared.clone())
    }
    pub fn state(&self) -> State {
        if self.retired {
            State::Retired
        } else {
            self.epoch.state()
        }
    }
    pub const fn reason(&self) -> Option<Error> {
        self.reason
    }
    pub fn snapshot(&self) -> Option<Snapshot> {
        self.epoch
            .process
            .as_ref()
            .and_then(native::Process::snapshot)
    }
    pub const fn statistics(&self) -> Statistics {
        self.epoch.statistics
    }
    pub fn retirement(&self) -> Option<native::Retirement> {
        self.epoch
            .process
            .as_ref()
            .map(native::Process::retirement)
            .or_else(|| self.epoch.retiring.clone())
    }
    pub fn owns_inbound(&self, route: Route) -> bool {
        self.lane.owns_inbound(route)
    }
    pub fn stop(&self) {
        self.shared.stop();
    }
    fn bound(&self, q: &QuicRecords) -> Result<(), Error> {
        if q.is_bound_to(&self.connection) {
            Ok(())
        } else {
            Err(Error::WrongConnection)
        }
    }
    /// Include this gate in the containing driver's authorization callback.
    /// Before a native poll, retained ACK bytes require immediate retirement on
    /// authority loss. With no retained ACK, the already-fenced optional owner
    /// can wait for the next maintenance turn without disrupting other lanes.
    pub fn permits_io(&self) -> bool {
        if self.retired {
            return true;
        }
        permits_io(
            &self.cx,
            &self.controller,
            &self.shared,
            &self.lane,
            self.epoch.inflight,
            self.epoch.precommit_deadline(),
        )
    }
    /// Native fence first, then audio-only retirement. This never waits for a
    /// child; [`Self::retirement`] reports the two independent OS wait receipts.
    pub fn retire(&mut self, q: &mut QuicRecords) -> Result<(), Error> {
        self.bound(q)?;
        self.shared.stop();
        self.epoch.stop(&self.shared);
        if !self.retired {
            self.retired = true;
            if q.is_closed() {
                self.lane.request_retirement();
            } else {
                self.lane.retire(q, &self.cx).map_err(Error::Transport)?;
            }
        }
        Ok(())
    }
    fn refuse(&mut self, q: &mut QuicRecords, reason: Error) -> Result<State, Error> {
        self.reason = Some(
            self.epoch
                .process
                .as_ref()
                .and_then(native::Process::error)
                .map_or(reason, Error::Native),
        );
        self.retire(q)?;
        Ok(State::Retired)
    }
    fn transport_refusal(
        &mut self,
        q: &mut QuicRecords,
        error: quic::Error,
    ) -> Result<State, Error> {
        // Observe the actual original reservation BEFORE cleanup. Voluntarily
        // retiring afterward cannot turn a failed parent control route into
        // evidence of healthy optional retirement.
        let optional = matches!(error, quic::Error::Closed | quic::Error::WrongRoute)
            && self.lane.is_retired(q) == Ok(true);
        if optional {
            return self.refuse(q, Error::Transport(error));
        }
        self.shared.stop();
        q.close();
        self.refuse(q, Error::Transport(error))?;
        Err(Error::Transport(error))
    }
    /// Drain witnesses are consumed before checking their old immutable caps.
    /// This method is also needed while unrelated presentation pauses payloads.
    pub fn maintain(&mut self, q: &mut QuicRecords) -> Result<State, Error> {
        self.bound(q)?;
        if self.retired {
            return match self.lane.is_retired(q) {
                Ok(true) => Ok(State::Retired),
                Ok(false) => self.transport_refusal(q, quic::Error::WrongRoute),
                Err(error) => self.transport_refusal(q, error),
            };
        }
        if let Err(error) = self.lane.check(q) {
            return self.transport_refusal(q, error);
        }
        if self.epoch.inflight.is_some()
            && self
                .lane
                .control_send_drained(q)
                .map_err(Error::Transport)?
        {
            self.epoch.inflight = None;
        }
        let result = admission(&self.cx, &self.controller, &self.shared).and_then(|now| {
            self.epoch.check_deadlines(now.as_micros())?;
            self.epoch.poll()
        });
        if let Err(reason) = result {
            return self.refuse(q, reason);
        }
        Ok(self.state())
    }
    /// One bounded control/datagram dispatch and one ACK admission attempt. No
    /// device/codec call, child wait, sleep or additional runtime runs here.
    pub fn service(
        &mut self,
        q: &mut QuicRecords,
        authorize: impl FnMut() -> bool,
    ) -> Result<State, Error> {
        // A foreign object must never be touched, even by an unwind guard.
        self.bound(q)?;
        let mut turn = ServiceTurn {
            host: self,
            q,
            completed: false,
        };
        let result = turn.host.service_inner(turn.q, authorize);
        turn.completed = result.is_ok();
        result
    }
    fn service_inner(
        &mut self,
        q: &mut QuicRecords,
        mut authorize: impl FnMut() -> bool,
    ) -> Result<State, Error> {
        if self.maintain(q)? == State::Retired {
            return Ok(State::Retired);
        }
        if let Err(error) = q.tick(&self.cx, || self.permits_io() && authorize()) {
            self.refuse(q, Error::Transport(error))?;
            return Err(Error::Transport(error));
        }
        if self.receive(q, &mut authorize)? == State::Retired {
            return Ok(State::Retired);
        }
        self.acknowledge(q, &mut authorize)?;
        self.maintain(q)
    }
    fn receive(
        &mut self,
        q: &mut QuicRecords,
        mut authorize: impl FnMut() -> bool,
    ) -> Result<State, Error> {
        let packet_ready = self.epoch.packet_ready();
        let inflight = self.epoch.inflight;
        let setup = self.epoch.precommit_deadline();
        let parameters = Parameters {
            cx: &self.cx,
            controller: &self.controller,
            shared: &self.shared,
            selection: &self.selection,
            launch: &self.launch,
            channel: native::Channel {
                parent: self.lane.parent(),
                id: self.lane.binding(),
                packet_maximum: self.lane.packet_maximum(),
            },
        };
        let mut refusal = None;
        let result = self.lane.dispatch(
            q,
            &self.cx,
            || {
                permits_io(
                    &self.cx,
                    &self.controller,
                    &self.shared,
                    &self.lane,
                    inflight,
                    setup,
                ) && authorize()
            },
            |route| matches!(route, Route::Stream(_)) || packet_ready,
            |_, bytes, receipt| match self.epoch.receive(bytes, receipt, &parameters) {
                Ok(disposition) => Ok(disposition),
                Err(reason) => {
                    // An optional semantic refusal must not become a whole
                    // connection handler error. Retire after dispatch returns.
                    refusal = Some(reason);
                    self.shared.stop();
                    Ok(Disposition::Consumed)
                }
            },
        );
        if let Some(reason) = refusal {
            if let Err(error) = self.lane.is_retired(q) {
                return self.transport_refusal(q, error);
            }
            return self.refuse(q, reason);
        }
        if let Err(error) = result {
            return self.transport_refusal(q, error);
        }
        self.maintain(q)
    }
    fn acknowledge(
        &mut self,
        q: &mut QuicRecords,
        mut authorize: impl FnMut() -> bool,
    ) -> Result<(), Error> {
        let Some(process) = self.epoch.process.as_ref() else {
            return Ok(());
        };
        if self.epoch.acknowledgement.is_none() {
            match process.ready_acknowledgement() {
                Ok(record) => self.epoch.acknowledgement = record,
                Err(error) => {
                    self.refuse(q, Error::Native(error))?;
                    return Ok(());
                }
            }
        }
        let Some(bytes) = self.epoch.acknowledgement else {
            return Ok(());
        };
        let until = self.epoch.setup_until.ok_or(Error::Protocol)?;
        let sent = self.lane.send_control_prepared(
            q,
            &self.cx,
            &bytes,
            until,
            || self.permits_io() && authorize(),
            || {
                let now = admission(&self.cx, &self.controller, &self.shared)?;
                self.epoch.check_deadlines(now.as_micros())?;
                if process.ready_acknowledgement().map_err(Error::Native)? != Some(bytes) {
                    return Err(Error::AcknowledgementUnknown);
                }
                Ok(())
            },
        );
        match sent {
            Err(quic::Error::Backpressure) => Ok(()),
            Ok(SendAdmission::Accepted) => {
                self.epoch.acknowledgement = None;
                self.epoch.inflight = Some(until);
                self.epoch.statistics.acknowledgements_admitted = self
                    .epoch
                    .statistics
                    .acknowledgements_admitted
                    .saturating_add(1);
                // Admission already succeeded on the original route. This
                // one-shot callback verifies the exact original native bytes;
                // it neither retries transport admission nor manufactures ACK.
                let result = self
                    .epoch
                    .process
                    .as_mut()
                    .ok_or(Error::Protocol)?
                    .acknowledge(|record| (record == bytes).then_some(()).ok_or(()));
                if let Err(error) = result {
                    self.refuse(q, Error::Native(error))?;
                }
                Ok(())
            }
            Ok(SendAdmission::Refused(reason)) => {
                self.refuse(q, reason)?;
                Ok(())
            }
            Err(error) => {
                if error == quic::Error::Expired && !q.is_closed() {
                    // The fixed ACK admission cap elapsed before any bytes
                    // entered transport; the optional owner can retire alone.
                    self.refuse(q, Error::AcknowledgementExpired)?;
                    Ok(())
                } else {
                    self.transport_refusal(q, error).map(|_| ())
                }
            }
        }
    }
}

/// Cancellation/unwind cannot strand a real native epoch or retained ACK.
/// Construct only after proving the exact original connection identity.
struct ServiceTurn<'a> {
    host: &'a mut Host,
    q: &'a mut QuicRecords,
    completed: bool,
}
impl Drop for ServiceTurn<'_> {
    fn drop(&mut self) {
        if !self.completed {
            self.host.shared.stop();
            self.host.epoch.stop(&self.host.shared);
            self.host.retired = true;
            self.host.reason.get_or_insert(Error::Cancelled);
            self.q.close();
        }
    }
}

fn admission(cx: &Cx, controller: &HostController, shared: &Shared) -> Result<HostInstant, Error> {
    cx.checkpoint().map_err(|_| Error::Cancelled)?;
    let now =
        HostInstant::from_micros(cx.timer_driver().ok_or(Error::Clock)?.now().as_nanos() / 1000);
    controller.deadline(now).map_err(Error::Authority)?;
    shared.check()?;
    Ok(now)
}

fn permits_io(
    cx: &Cx,
    controller: &HostController,
    shared: &Shared,
    lane: &AudioUpChannel,
    inflight: Option<u64>,
    setup: Option<u64>,
) -> bool {
    let result = admission(cx, controller, shared).and_then(|now| {
        if inflight.is_some_and(|until| now.as_micros() >= until) {
            Err(Error::AcknowledgementExpired)
        } else if setup.is_some_and(|until| now.as_micros() >= until) {
            Err(Error::SetupExpired)
        } else {
            Ok(())
        }
    });
    match result {
        Ok(()) => true,
        Err(Error::Clock | Error::Cancelled) => {
            shared.stop();
            false
        }
        Err(_) => {
            shared.stop();
            if inflight.is_some() {
                lane.request_retirement();
            }
            true
        }
    }
}

#[cfg(test)]
#[path = "microphone_quic/tests.rs"]
mod tests;

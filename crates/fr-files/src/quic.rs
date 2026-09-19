//! File attachment -> bounded native transport -> original disk-worker owner.
//!
//! The existing session owns and drives QUIC. Call `service` before and after
//! its I/O turn; service other lanes normally. No socket, runtime, identity,
//! permission, input lease or destination path is manufactured here.
use crate::{
    atp::receive as atp,
    receive::DropDirectory,
    session::{Permission, Policy},
    wire::{self, Admission, ResultReceipt, Settings},
    worker::{self, Task},
};
use asupersync::{cx::Cx, time::TimerDriverHandle};
use fr_core::input_submission::InputSession;
use fr_transport::quic::{self, ConnectionBinding, Disposition, QuicRecords, files::FilesChannel};
use fr_wire::files::Role;
use std::time::Duration;

pub struct Configuration {
    pub directory: DropDirectory,
    pub permission: Permission,
    pub policy: Policy,
    /// Original proof-send budget, fixed when the proof is created. Backpressure
    /// cannot extend it. Expiry retires files, never retries a publication.
    pub reply_lifetime: Duration,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    WrongConnection,
    WrongRole,
    Limits,
    Clock,
    Cancelled,
    Transport(quic::Error),
    File(wire::Error),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Active,
    Draining,
    Retired,
}

/// One receive operation in the original disk slot, one retained reply buffer,
/// and one bounded native send reservation. Never blocks for a syscall or thread
/// join. File retirement clears only the file pair; actual results stay readable.
pub struct HostReceiver {
    cx: Cx,
    clock: TimerDriverHandle,
    connection: ConnectionBinding,
    lane: FilesChannel,
    receiver: wire::HostReceiver,
    task: Task,
    reply: Vec<u8>,
    reply_len: usize,
    reply_deadline: u64,
    reply_lifetime: u64,
    sent: bool,
    state: State,
}
impl std::fmt::Debug for HostReceiver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeFileReceiver")
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}
impl HostReceiver {
    /// The lane must already have completed its one-use authenticated exchange on
    /// q. Setup/consent failure retires that optional pair, not desktop control.
    pub fn spawn(
        cx: Cx,
        q: &mut QuicRecords,
        lane: FilesChannel,
        input: &InputSession,
        config: Configuration,
    ) -> Result<Self, Error> {
        Self::spawn_with_authority(
            cx,
            q,
            lane,
            crate::session::Authority::from_input(input),
            config,
        )
    }
    pub fn spawn_with_authority(
        cx: Cx,
        q: &mut QuicRecords,
        mut lane: FilesChannel,
        authority: crate::session::Authority,
        config: Configuration,
    ) -> Result<Self, Error> {
        lane.check(q).map_err(Error::Transport)?;
        let setup = (|| {
            cx.checkpoint().map_err(|_| Error::Cancelled)?;
            if lane.outgoing().sender != Role::Host {
                return Err(Error::WrongRole);
            }
            let reply_lifetime =
                u64::try_from(config.reply_lifetime.as_micros()).map_err(|_| Error::Limits)?;
            if !(1..=1_000_000).contains(&reply_lifetime) {
                return Err(Error::Limits);
            }
            let clock = cx.timer_driver().ok_or(Error::Clock)?;
            let maximum = lane.limits().record_bytes();
            let mut reply = Vec::new();
            reply
                .try_reserve_exact(maximum)
                .map_err(|_| Error::Limits)?;
            reply.resize(maximum, 0);
            let (receiver, task) = wire::HostReceiver::spawn_with_authority(
                cx.clone(),
                authority,
                config.directory,
                config.permission,
                config.policy,
                Settings {
                    incoming: lane.incoming(),
                    outgoing: lane.outgoing(),
                    limits: lane.limits(),
                },
            )
            .map_err(Error::File)?;
            Ok((clock, reply_lifetime, reply, receiver, task))
        })();
        match setup {
            Ok((clock, reply_lifetime, reply, receiver, task)) => Ok(Self {
                cx,
                clock,
                connection: q.binding(),
                lane,
                receiver,
                task,
                reply,
                reply_lifetime,
                reply_len: 0,
                reply_deadline: 0,
                sent: false,
                state: State::Active,
            }),
            Err(error) => {
                lane.retire(q, &cx).map_err(Error::Transport)?;
                Err(error)
            }
        }
    }
    pub fn owns_inbound(&self, route: quic::Route) -> bool {
        self.lane.owns_inbound(route)
    }
    pub fn state(&self) -> State {
        self.state
    }
    pub fn last_result(&self) -> Option<ResultReceipt> {
        self.receiver.last_result()
    }
    pub fn progress(&self) -> Option<crate::session::Progress> {
        self.receiver.progress()
    }
    pub fn cleanup_finished(&self) -> bool {
        self.task.is_finished()
    }
    pub fn try_finish_cleanup(&mut self) -> Option<Result<(), worker::Error>> {
        self.task.try_finish()
    }
    /// Local stop fences publication immediately and wakes the disk owner. The
    /// session must still call `service` for bounded proof draining/retirement.
    pub fn stop(&mut self) {
        self.receiver.stop();
        if self.state != State::Retired {
            self.state = State::Draining;
        }
    }
    /// Explicit file-only reset. A foreign connection is untouched. A completed
    /// or racing disk outcome remains available through `collect_after_close`.
    pub fn retire(&mut self, q: &mut QuicRecords) -> Result<(), Error> {
        self.check_identity(q)?;
        self.receiver.stop();
        self.task.stop();
        if self.state != State::Retired && !q.is_closed() {
            self.lane.retire(q, &self.cx).map_err(Error::Transport)?;
        }
        self.state = State::Retired;
        self.reply.fill(0);
        self.reply_len = 0;
        self.sent = false;
        Ok(())
    }
    /// Poll actual effects after disconnection/retirement, without network I/O or
    /// resurrecting authority. None/missing worker result is NOT a no-effect claim.
    pub fn collect_after_close(&mut self) -> Result<Option<ResultReceipt>, Error> {
        self.receiver.stop();
        if self.state == State::Active {
            self.state = State::Draining;
        }
        if self.reply_len == 0 {
            match self.receiver.poll_reply(&mut self.reply) {
                Ok(Some(n)) => self.reply_len = n,
                Ok(None) => {}
                Err(error) if busy(error) => {}
                Err(error) => return Err(Error::File(error)),
            }
        }
        Ok(self.last_result())
    }
    /// Service at most one receive record and one reply. `authorize` checks the
    /// parent connection/session; file permission is separately checked by the
    /// original disk owner before EVERY write and final publication.
    pub fn service(
        &mut self,
        q: &mut QuicRecords,
        mut authorize: impl FnMut() -> bool,
    ) -> Result<State, Error> {
        self.check_identity(q)?;
        if self.state == State::Retired {
            self.collect_after_close()?;
            return Ok(self.state);
        }
        if self.cx.is_cancel_requested() {
            self.receiver.stop();
            self.collect_after_close()?;
            self.retire(q)?;
            return Err(Error::Cancelled);
        }
        if let Err(error) = self.lane.check(q) {
            self.receiver.stop();
            self.collect_after_close()?;
            self.retire(q)?;
            return Err(Error::Transport(error));
        }
        let result = self.turn(q, &mut authorize);
        if result.is_err() {
            self.receiver.stop();
            self.retire(q)?;
        }
        result.map(|()| self.state)
    }
    fn turn(
        &mut self,
        q: &mut QuicRecords,
        authorize: &mut impl FnMut() -> bool,
    ) -> Result<(), Error> {
        let now = self.clock.now().as_nanos() / 1000;
        if self.sent && self.lane.send_drained(q).map_err(Error::Transport)? {
            self.sent = false;
            self.reply[..self.reply_len].fill(0);
            self.reply_len = 0;
        }
        if self.reply_len == 0 {
            match self.receiver.poll_reply(&mut self.reply) {
                Ok(Some(n)) => {
                    self.reply_len = n;
                    self.reply_deadline =
                        now.checked_add(self.reply_lifetime).ok_or(Error::Clock)?;
                }
                Ok(None) => {}
                Err(error) if busy(error) => return Ok(()),
                Err(error) => return Err(Error::File(error)),
            }
        }
        if self.receiver.is_closed() {
            self.state = State::Draining;
        }
        if self.reply_len > 0 {
            if now >= self.reply_deadline {
                return self.retire(q);
            }
            if !self.sent {
                match self.lane.send(
                    q,
                    &self.cx,
                    &self.reply[..self.reply_len],
                    self.reply_deadline,
                    &mut *authorize,
                ) {
                    Ok(()) => self.sent = true,
                    Err(quic::Error::Backpressure) => {}
                    Err(error) => return Err(Error::Transport(error)),
                }
            }
            // Proof must stay owned until native send/retransmission state drains.
            // A next-record FileCancel can still fence the disk owner; ordinary
            // data remains transport-owned until the reply slot is free.
            return if self.state == State::Active {
                self.receive_one(q, authorize, true)
            } else {
                Ok(())
            };
        }
        if self.state == State::Draining && !self.receiver.is_busy() {
            return self.retire(q);
        }
        if self.state == State::Draining {
            return Ok(());
        }
        self.receive_one(q, authorize, false)
    }
    fn receive_one(
        &mut self,
        q: &mut QuicRecords,
        authorize: &mut impl FnMut() -> bool,
        cancel_only: bool,
    ) -> Result<(), Error> {
        let incoming = self.lane.incoming();
        let limits = self.lane.limits();
        let receiver = &mut self.receiver;
        let mut refusal = None;
        self.lane
            .dispatch(q, &self.cx, &mut *authorize, |bytes| {
                if cancel_only
                    && !matches!(
                        fr_wire::files::decode(bytes, incoming, limits),
                        Ok(fr_wire::files::Message {
                            body: fr_wire::files::Body::Cancel(_),
                            ..
                        })
                    )
                {
                    return Ok(Disposition::Blocked);
                }
                match receiver.receive(bytes) {
                    Ok(Admission::Queued(_) | Admission::CancellationRequested) => {
                        Ok(Disposition::Consumed)
                    }
                    Err(error) if busy(error) => Ok(Disposition::Blocked),
                    Err(error) => {
                        refusal = Some(error);
                        Ok(Disposition::Consumed)
                    }
                }
            })
            .map_err(Error::Transport)?;
        if let Some(error) = refusal {
            return Err(Error::File(error));
        }
        Ok(())
    }
    fn check_identity(&self, q: &QuicRecords) -> Result<(), Error> {
        if q.is_bound_to(&self.connection) {
            Ok(())
        } else {
            Err(Error::WrongConnection)
        }
    }
}
fn busy(error: wire::Error) -> bool {
    matches!(
        error,
        wire::Error::Busy
            | wire::Error::Atp(atp::Error::Busy | atp::Error::Worker(worker::Error::Busy))
    )
}
impl Drop for HostReceiver {
    fn drop(&mut self) {
        self.receiver.stop();
        self.task.stop();
        self.reply.fill(0);
    }
}

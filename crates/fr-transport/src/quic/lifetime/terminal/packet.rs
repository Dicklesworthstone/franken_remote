//! One protected close on the original socket. No ordinary data is flushed.
use super::{DRAIN_US, Error, QuicRecords, now};
use asupersync::{
    cx::Cx,
    net::quic_native::{NativeQuicUdpConnection, QuicConnectionState},
    time::timeout,
};
use std::{sync::Arc, time::Duration};

pub(super) struct Packet {
    native: NativeQuicUdpConnection,
    cx: Cx,
    gate: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    code: u64,
    started: u64,
    until: u64,
}
impl QuicRecords {
    pub(super) fn prepare_terminal_packet(
        &mut self,
        cleanup: &Cx,
        started: u64,
        code: u64,
    ) -> Result<Packet, Error> {
        cleanup.checkpoint().map_err(|_| Error::Cancelled)?;
        let until = started.checked_add(DRAIN_US).ok_or(Error::Clock)?;
        let current = now(cleanup)?;
        if current < started || self.last_now.is_some_and(|last| started < last) {
            return Err(Error::Clock);
        }
        if current >= until {
            return Err(Error::Expired);
        }
        if self
            .terminal_lifetime_check
            .as_ref()
            .is_some_and(|gate| !gate())
        {
            return Err(Error::Unauthorized);
        }
        if self
            .native
            .as_ref()
            .ok_or(Error::Closed)?
            .connection()
            .state()
            != QuicConnectionState::Established
        {
            return Err(Error::Closed);
        }
        Ok(Packet {
            native: self.native.take().ok_or(Error::Closed)?,
            cx: cleanup.clone(),
            gate: self.terminal_lifetime_check.clone(),
            code,
            started,
            until,
        })
    }
}
impl Packet {
    pub(super) fn replace_code(&mut self, code: u64) {
        self.code = code;
    }

    /// Success is a socket write only, never an ACK or remote cleanup receipt.
    pub(super) async fn run(mut self: Box<Self>) -> Result<(), Error> {
        self.cx.checkpoint().map_err(|_| Error::Cancelled)?;
        if self.gate.as_ref().is_some_and(|gate| !gate()) {
            return Err(Error::Unauthorized);
        }
        let at = now(&self.cx)?;
        if at < self.started {
            return Err(Error::Clock);
        }
        if at >= self.until {
            return Err(Error::Expired);
        }
        if self.native.connection().state() != QuicConnectionState::Established {
            return Err(Error::Closed);
        }
        let cx = self.cx.clone();
        let remaining = Duration::from_micros(self.until - at);
        let gate = self.gate.clone();
        let sent = timeout(
            cx.now(),
            remaining,
            super::super::poll_io(
                &cx,
                gate.as_deref(),
                &mut || true,
                &[],
                at,
                Some(self.until),
                // The pinned native operation first enters Draining, discards
                // protected application packets and emits only CONNECTION_CLOSE.
                self.native.close(&cx, self.code),
            ),
        )
        .await
        .map_err(|_| Error::Expired)??;
        if sent == 0 {
            return Err(Error::Closed);
        }
        Ok(())
    }
}

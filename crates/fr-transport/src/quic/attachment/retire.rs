//! Completed optional-channel cleanup, not abandonment of a partial handshake.
use super::{ACTIVE, MediaChannel, MediaRole, Ordering, Phase, QuicRecords, RETIRED};
use crate::quic::{Cx, Error};
use asupersync::bytes::Bytes;

// Application cancellation of the optional clipboard pair. No new FRD0 kind,
// acknowledgment, authority token, or retry semantics are introduced.
const CLIPBOARD_RETIRED: u64 = 0x4652_4350;
const FILES_RETIRED: u64 = 0x4652_4649;

impl MediaChannel {
    /// Retire only a COMPLETED optional clipboard pair on the original connection.
    /// Also accepts a peer-retired tombstone, including during the selected
    /// running-session readiness exchange. Never releases IDs for reuse or
    /// grants clipboard access. Incomplete exchanges cannot use this escape.
    pub fn retire_clipboard(&mut self, q: &mut QuicRecords, cx: &Cx) -> Result<(), Error> {
        self.retire_optional(q, cx, MediaRole::Clipboard)
    }
    /// Retire only the completed file pair. Retain its consumed ticket, binding
    /// and stream IDs; a second attachment cannot reset cumulative file quotas.
    pub fn retire_files(&mut self, q: &mut QuicRecords, cx: &Cx) -> Result<(), Error> {
        self.retire_optional(q, cx, MediaRole::Files)
    }
    fn retire_optional(
        &mut self,
        q: &mut QuicRecords,
        cx: &Cx,
        role: MediaRole,
    ) -> Result<(), Error> {
        // Reject a foreign object BEFORE any operation or error can mutate it.
        if !q.is_bound_to(&self.connection) || self.descriptor.role != role {
            return Err(Error::WrongRoute);
        }
        if self.state.load(Ordering::Acquire) == RETIRED {
            self.close();
            return Ok(());
        }
        self.completed_on(q)?;
        let index = q
            .attachments
            .iter()
            .position(|r| std::sync::Arc::ptr_eq(&r.state, &self.state))
            .ok_or(Error::WrongRoute)?;
        if let Err(error) = q.retire_optional_at(cx, index) {
            self.close();
            q.close();
            return Err(error);
        }
        self.close();
        debug_assert_eq!(self.phase, Phase::Closed);
        Ok(())
    }
}
impl QuicRecords {
    /// Run before framing/retained-record deadline checks. A peer's reset/FIN
    /// cancels this optional pair; it is not EOF on the critical input stream.
    /// Retired reservations remain forever to prevent replay-ledger recreation.
    pub(crate) fn service_optional_retirements(&mut self, cx: &Cx) -> Result<(), Error> {
        for index in 0..self.attachments.len() {
            let r = &self.attachments[index];
            if !matches!(r.role, MediaRole::Clipboard | MediaRole::Files) {
                continue;
            }
            if r.state.load(Ordering::Acquire) == ACTIVE {
                let streams = self
                    .native
                    .as_ref()
                    .ok_or(Error::Closed)?
                    .connection()
                    .inner()
                    .streams();
                let outgoing = streams.stream(r.outbound).map_err(|_| Error::Native)?;
                let incoming = streams.stream(r.inbound).map_err(|_| Error::Native)?;
                if outgoing.stop_sending_error_code.is_some()
                    || outgoing.send_reset.is_some()
                    || incoming.recv_reset.is_some()
                    || incoming.final_size.is_some()
                {
                    self.retire_optional_at(cx, index)?;
                }
            }
            if self.attachments[index].retired() {
                self.account_retired_receive(index)?;
            }
        }
        self.refresh_retired_credit(cx)
    }
    fn retire_optional_at(&mut self, cx: &Cx, index: usize) -> Result<(), Error> {
        let r = &self.attachments[index];
        if !matches!(r.role, MediaRole::Clipboard | MediaRole::Files)
            || r.state.load(Ordering::Acquire) != ACTIVE
        {
            return Err(Error::WrongRoute);
        }
        let (outbound, inbound, binding) = (r.outbound, r.inbound, r.binding);
        let code = if r.role == MediaRole::Files {
            FILES_RETIRED
        } else {
            CLIPBOARD_RETIRED
        };
        let native = self.native.as_mut().ok_or(Error::Closed)?;
        let outgoing = native
            .connection()
            .inner()
            .streams()
            .stream(outbound)
            .map_err(|_| Error::Native)?;
        if outgoing.send_reset.is_none() {
            // The native reset uses the actual committed offset, clears unsent
            // stream buffers AND removes STREAM references from loss recovery.
            // It does not discard other streams' retransmissions/control frames.
            native
                .connection_mut()
                .reset_stream(cx, outbound, code)
                .map_err(|_| Error::Native)?;
        }
        let incoming = native
            .connection()
            .inner()
            .streams()
            .stream(inbound)
            .map_err(|_| Error::Native)?;
        self.attachments[index].retired_receive_accounted = incoming.read_offset;
        native
            .connection_mut()
            .stop_stream_receiving(cx, inbound, code)
            .map_err(|_| Error::Native)?;
        self.pending_writes.retain(|p| p.route.stream != outbound);
        for sender in &mut self.senders {
            if sender.route.stream == outbound {
                sender.bytes = 0;
                sender.records = 0;
                sender.until = None;
            }
        }
        for receiver in &mut self.inbound {
            if receiver.route.stream == inbound {
                receiver.framing.close();
                receiver.remainder = Bytes::new();
                receiver.fin = true;
            }
        }
        // Keep numeric route entries as tombstones too: native stream counts
        // include retired IDs, and none of these routes can be reopened/rebound.
        debug_assert!(self.streams.iter().any(|r| r.binding == binding));
        self.attachments[index]
            .state
            .store(RETIRED, Ordering::Release);
        self.account_retired_receive(index)?;
        self.refresh_retired_credit(cx)
    }
    fn account_retired_receive(&mut self, index: usize) -> Result<(), Error> {
        let r = &mut self.attachments[index];
        let stream = self
            .native
            .as_ref()
            .ok_or(Error::Closed)?
            .connection()
            .inner()
            .streams()
            .stream(r.inbound)
            .map_err(|_| Error::Native)?;
        // High-water/final-size bytes already charged by QUIC are now discarded,
        // not consumed by an application. A later RESET can charge additional
        // final-size credit. Reclaim each byte ONCE, never a full window per tick.
        let charged = stream.recv_credit.used();
        let additional = charged
            .checked_sub(r.retired_receive_accounted)
            .ok_or(Error::Native)?;
        self.read_bytes = self
            .read_bytes
            .checked_add(additional)
            .ok_or(Error::Clock)?;
        r.retired_receive_accounted = charged;
        Ok(())
    }
    fn refresh_retired_credit(&mut self, cx: &Cx) -> Result<(), Error> {
        let limit = self
            .read_bytes
            .checked_add(self.policy.connection_window)
            .ok_or(Error::Clock)?;
        if limit > self.advertised_limit {
            self.native
                .as_mut()
                .ok_or(Error::Closed)?
                .connection_mut()
                .advertise_connection_receive_limit(cx, limit)
                .map_err(|_| Error::Native)?;
            self.advertised_limit = limit;
        }
        Ok(())
    }
}

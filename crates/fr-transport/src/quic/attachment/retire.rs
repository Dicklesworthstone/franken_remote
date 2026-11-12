//! Completed auxiliary-channel cleanup, never abandonment of a partial handshake.
mod files_setup;
use super::{ACTIVE, MediaChannel, MediaRole, Ordering, Phase, QuicRecords, RETIRED};
use crate::quic::{Cx, Error};
use asupersync::bytes::Bytes;

// Application cancellation of the optional clipboard pair. No new FRD0 kind,
// acknowledgment, authority token, or retry semantics are introduced.
const CLIPBOARD_RETIRED: u64 = 0x4652_4350;
// Only the original ACTIVE Clipboard pair can report this framing expiry.
// It is terminal metadata, not a new item lifetime or permission to retry.
const CLIPBOARD_RECEIVE_EXPIRED: u64 = 0x4652_4345;
// The original sender's transport deadline, not the item's authority lifetime.
// Only the original ACTIVE Clipboard pair recognizes this terminal metadata.
const CLIPBOARD_SEND_EXPIRED: u64 = 0x4652_4353;
const FILES_RETIRED: u64 = 0x4652_4649;
// An expired receive is a terminal file-transfer outcome, including for its
// peer sender. It carries no retry or authority semantics and is recognized
// only after the original Files/FilesDownload exchange has reached ACTIVE.
const FILES_RECEIVE_EXPIRED: u64 = 0x4652_4645;
// The original sender's retained delivery deadline, on an ACTIVE file pair.
// Both endpoints retain that cause; it is not the peer's framing deadline.
const FILES_SEND_EXPIRED: u64 = 0x4652_4653;
const MEDIA_RETIRED: u64 = 0x4652_4d44;
const AUDIO_RETIRED: u64 = 0x4652_4144;
const MICROPHONE_RETIRED: u64 = 0x4652_4155;
const AUDIO_RECEIVE_EXPIRED: u64 = 0x4652_4145;
const MICROPHONE_RECEIVE_EXPIRED: u64 = 0x4652_5545;

fn receive_expiry_code(role: MediaRole) -> Option<u64> {
    match role {
        MediaRole::Clipboard => Some(CLIPBOARD_RECEIVE_EXPIRED),
        MediaRole::Files | MediaRole::FilesDownload => Some(FILES_RECEIVE_EXPIRED),
        MediaRole::AudioDown => Some(AUDIO_RECEIVE_EXPIRED),
        MediaRole::AudioUp => Some(MICROPHONE_RECEIVE_EXPIRED),
        MediaRole::Configuration | MediaRole::Recovery | MediaRole::Video | MediaRole::Input => {
            None
        }
    }
}

fn send_expiry_code(role: MediaRole) -> Option<u64> {
    match role {
        MediaRole::Clipboard => Some(CLIPBOARD_SEND_EXPIRED),
        MediaRole::Files | MediaRole::FilesDownload => Some(FILES_SEND_EXPIRED),
        MediaRole::Configuration
        | MediaRole::Recovery
        | MediaRole::Video
        | MediaRole::Input
        | MediaRole::AudioDown
        | MediaRole::AudioUp => None,
    }
}

pub(super) fn retirement_code(role: MediaRole) -> Option<u64> {
    match role {
        MediaRole::Clipboard => Some(CLIPBOARD_RETIRED),
        MediaRole::Files | MediaRole::FilesDownload => Some(FILES_RETIRED),
        MediaRole::AudioDown => Some(AUDIO_RETIRED),
        MediaRole::AudioUp => Some(MICROPHONE_RETIRED),
        MediaRole::Configuration | MediaRole::Recovery | MediaRole::Video => Some(MEDIA_RETIRED),
        // Input has one ordering/authority domain for the connection lifetime.
        MediaRole::Input => None,
    }
}

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
        if !self.descriptor.role.is_files() {
            return Err(Error::WrongRoute);
        }
        self.retire_optional(q, cx, self.descriptor.role)
    }
    /// Retire only this COMPLETED audio direction on its original connection.
    /// The caller fences its native capture/output before retirement. The other
    /// audio direction, video and control remain independent; consumed binding,
    /// ticket and stream identities stay tombstones. Partial exchanges cannot
    /// use this escape and retain their connection-fencing cancellation policy.
    /// Microphone retirement closes the original connection if outbound native
    /// datagrams or uncertain protected output may still contain its audio;
    /// deleting a route cannot revoke bytes already admitted to that queue.
    pub fn retire_audio(&mut self, q: &mut QuicRecords, cx: &Cx) -> Result<(), Error> {
        if !matches!(
            self.descriptor.role,
            MediaRole::AudioDown | MediaRole::AudioUp
        ) {
            return Err(Error::WrongRoute);
        }
        self.retire_optional(q, cx, self.descriptor.role)
    }
    /// Retire a COMPLETED configuration, recovery or video attachment on its
    /// original connection. The session fences view/input authority first, then
    /// retires all three old media roles and admits fresh, generation-bound
    /// attachments. This operation does not grant replacement admission.
    ///
    /// Reliable native buffers/retransmissions and application-held fragments
    /// are abandoned. Already admitted datagrams remain bounded native work and
    /// may be in flight; their old binding is no longer deliverable after local
    /// retirement. Input/control and other subscriptions are not reset.
    ///
    /// Consumed tickets, stream IDs and binding IDs remain tombstones. Repeated
    /// replacements reach the existing connection limit rather than allocating
    /// an unbounded history or recycling old stream identities.
    pub fn retire_media(&mut self, q: &mut QuicRecords, cx: &Cx) -> Result<(), Error> {
        if !matches!(
            self.descriptor.role,
            MediaRole::Configuration | MediaRole::Recovery | MediaRole::Video
        ) {
            return Err(Error::WrongRoute);
        }
        self.retire_optional(q, cx, self.descriptor.role)
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
    /// Read the terminal receive cause of one registered audio pair. `inbound`
    /// must be the exact original receive route, including direction, role,
    /// priority and bound. This diagnostic neither admits data nor establishes
    /// connection identity: its session owner must supply its original checked
    /// connection. A closed connection is not evidence of optional retirement.
    pub fn audio_receive_expired(&self, inbound: crate::quic::StreamRoute) -> Result<bool, Error> {
        if self.is_closed() {
            return Err(Error::Closed);
        }
        if inbound.outbound || !self.streams.contains(&inbound) {
            return Err(Error::WrongRoute);
        }
        let reservation = self
            .attachments
            .iter()
            .find(|r| {
                r.inbound == inbound.stream
                    && r.binding == inbound.binding
                    && matches!(r.role, MediaRole::AudioDown | MediaRole::AudioUp)
            })
            .ok_or(Error::WrongRoute)?;
        Ok(reservation.retired() && reservation.receive_expired)
    }
    /// Run before framing/retained-record deadline checks. A peer's reset/FIN
    /// cancels this media/optional pair; it is not EOF on the critical input stream.
    /// So does the local owner's `request_retirement`.
    /// Retired reservations remain forever to prevent replay-ledger recreation.
    pub(crate) fn service_optional_retirements(&mut self, cx: &Cx, now: u64) -> Result<(), Error> {
        for index in 0..self.attachments.len() {
            let r = &self.attachments[index];
            if retirement_code(r.role).is_none() {
                continue;
            }
            if r.state.load(Ordering::Acquire) == ACTIVE {
                // Optional clipboard, files and audio share this connection
                // with input and video. A stalled prefix or unconsumed complete
                // record expires its optional pair, not its desktop.
                // Retire the ACTIVE original pair before generic framing checks;
                // never extend the first-byte deadline or reuse its binding.
                // Malformed records, unfinished exchanges and critical streams
                // still take their ordinary connection-fencing paths.
                let receive_expired = self.inbound.iter().any(|s| {
                    s.route.stream == r.inbound
                        && !s.fin
                        && receive_expiry_code(r.role).is_some()
                        && s.framing.next_deadline().is_some_and(|until| now >= until)
                });
                let streams = self
                    .native
                    .as_ref()
                    .ok_or(Error::Closed)?
                    .connection()
                    .inner()
                    .streams();
                let outgoing = streams.stream(r.outbound).map_err(|_| Error::Native)?;
                let incoming = streams.stream(r.inbound).map_err(|_| Error::Native)?;
                let peer_receive_expired = receive_expiry_code(r.role).is_some_and(|code| {
                    outgoing.stop_sending_error_code == Some(code)
                        || incoming.recv_reset.map(|(code, _)| code) == Some(code)
                });
                let send_expiry = send_expiry_code(r.role);
                let peer_send_expired = send_expiry.is_some_and(|code| {
                    outgoing.stop_sending_error_code == Some(code)
                        || incoming.recv_reset.map(|(code, _)| code) == Some(code)
                });
                // A received terminal cause predates this turn's local clock
                // checks. Do not relabel the peer's send expiry as a receive
                // expiry merely because this consumer was scheduled later.
                let receive_expired =
                    peer_receive_expired || (receive_expired && !peer_send_expired);
                let send_expired = send_expiry.is_some()
                    && !receive_expired
                    && (peer_send_expired
                        || self.senders.iter().any(|sender| {
                            sender.route.stream == r.outbound
                                && sender.until.is_some_and(|until| now >= until)
                        }));
                // Only the exclusive between-I/O owner can retract this
                // stream's unsent and retransmittable bytes. An unsettled
                // native operation may own protected packets containing them;
                // resetting stream metadata cannot selectively withdraw those.
                // poll_io_at_boundary keeps its unchanged terminal deadline
                // check while a native future owns the connection.
                if send_expired && self.native_io_unsettled {
                    return Err(Error::Expired);
                }
                if receive_expired
                    || send_expired
                    || r.retire.load(Ordering::Acquire)
                    || outgoing.stop_sending_error_code.is_some()
                    || outgoing.send_reset.is_some()
                    || incoming.recv_reset.is_some()
                    || incoming.final_size.is_some()
                {
                    self.attachments[index].receive_expired = receive_expired;
                    self.attachments[index].send_expired = send_expired;
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
        if self.attachments[index].state.load(Ordering::Acquire) != ACTIVE {
            return Err(Error::WrongRoute);
        }
        self.retire_reservation_at(cx, index)
    }
    /// The ordinary path requires ACTIVE. The only other caller proves the
    /// original Files exchange reached its direction-specific staged final boundary.
    fn retire_reservation_at(&mut self, cx: &Cx, index: usize) -> Result<(), Error> {
        let r = &self.attachments[index];
        let code = if r.receive_expired {
            receive_expiry_code(r.role).ok_or(Error::WrongRoute)?
        } else if r.send_expired {
            send_expiry_code(r.role).ok_or(Error::WrongRoute)?
        } else {
            retirement_code(r.role).ok_or(Error::WrongRoute)?
        };
        let (outbound, inbound, binding) = (r.outbound, r.inbound, r.binding);
        // The native DATAGRAM queue mixes routes and exposes no selective erase.
        // Check before removing this binding, on all three retirement paths:
        // explicit local retirement, a callback request, and peer reset/FIN.
        // Host-side AudioUp cannot queue audio datagrams, so unrelated host
        // video/playback must not turn an ordinary microphone stop into a close.
        let outgoing_microphone = r.role == MediaRole::AudioUp
            && self
                .datagrams
                .iter()
                .any(|route| route.binding == binding && route.outbound);
        let native = self.native.as_mut().ok_or(Error::Closed)?;
        if r.role == MediaRole::AudioUp
            && (self.native_io_unsettled
                || (outgoing_microphone
                    && native
                        .connection()
                        .inner()
                        .pending_outbound_datagram_count()
                        != 0))
        {
            return Err(Error::RetainedMicrophoneOutput);
        }
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
        // Remove only this binding's routing capability. Do not drain the
        // native datagram queue: it can also contain another view's media or
        // critical pointer state. Late arrivals fail the normal route lookup.
        self.datagrams.retain(|route| route.binding != binding);
        self.pending_datagrams.retire(binding);
        for sender in &mut self.senders {
            if sender.route.stream == outbound {
                sender.bytes = 0;
                sender.records = 0;
                sender.until = None;
                sender.epoch = None;
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

impl QuicRecords {
    /// Non-consuming namespace capacity, including all retired tombstones.
    /// This is not an admission reservation: concurrent/unfinished exchanges
    /// and the native peer's limits are still validated by offer/accept.
    pub fn remaining_channel_pairs(&self) -> usize {
        ((crate::quic::MAX_STREAMS.saturating_sub(self.streams.len())) / 2)
            .min(((crate::quic::MAX_STREAMS - 2) / 2).saturating_sub(self.attachments.len()))
    }

    /// Retire a completed configuration/recovery/video set retained by its
    /// original connection owner. Validate the ENTIRE set before resetting any
    /// stream; metadata is not a replacement for the opaque connection proof.
    /// The caller fences its view/input first. This grants no new authority.
    /// Peer-retired sets are idempotent, and all consumed IDs remain tombstones.
    pub fn retire_media_set(
        &mut self,
        cx: &Cx,
        original: &crate::quic::ConnectionBinding,
        pairs: [super::AttachedChannel; 3],
    ) -> Result<(), Error> {
        if !self.is_bound_to(original) {
            return Err(Error::WrongRoute);
        }
        if self.is_closed() {
            return Err(Error::Closed);
        }
        cx.checkpoint().map_err(|_| Error::Cancelled)?;
        let mut indices = [0; 3];
        let view = pairs[0].descriptor.binding;
        for (index, (pair, role)) in pairs
            .iter()
            .zip([
                MediaRole::Configuration,
                MediaRole::Recovery,
                MediaRole::Video,
            ])
            .enumerate()
        {
            let mut binding = pair.descriptor.binding;
            binding.parent.id = view.parent.id;
            if binding != view
                || pair.descriptor.role != role
                || !pair.outbound.outbound
                || pair.inbound.outbound
                || pair.outbound.binding != pair.descriptor.binding.parent.id
                || pair.inbound.binding != pair.outbound.binding
                || !self.streams.contains(&pair.outbound)
                || !self.streams.contains(&pair.inbound)
            {
                return Err(Error::WrongRoute);
            }
            let found = self
                .attachments
                .iter()
                .position(|r| {
                    r.role == role
                        && r.binding == pair.outbound.binding
                        && r.outbound == pair.outbound.stream
                        && r.inbound == pair.inbound.stream
                        && matches!(r.state.load(Ordering::Acquire), ACTIVE | RETIRED)
                })
                .ok_or(Error::WrongRoute)?;
            if indices[..index].contains(&found) {
                return Err(Error::WrongRoute);
            }
            indices[index] = found;
        }
        for index in indices {
            if !self.attachments[index].retired()
                && let Err(error) = self.retire_optional_at(cx, index)
            {
                // Native failure can have partially reset this set. Never
                // expose an apparently reusable connection after that effect.
                self.close();
                return Err(error);
            }
        }
        Ok(())
    }
}

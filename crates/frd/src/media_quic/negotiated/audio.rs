//! The optional `audio-down` attachment joined to the ORIGINAL media set.
//! Selection alone is typed absence until this exact attachment completes on
//! the same connection for the same view; nothing here is a local audio
//! enable, an approval, or evidence of capture freshness.
use super::{Error, NegotiatedMedia};
use fr_transport::quic::{DatagramRoute, MediaChannel, Messages, QuicRecords, Route, StreamRoute};
use fr_wire::{attachment::MediaRole, negotiation::Role};

/// Exact routes of one side of the audio-down channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioLanes {
    /// Host outbound / viewer inbound: `AudioConfiguration`, `AudioStop`.
    pub control: StreamRoute,
    /// Viewer outbound / host inbound: `AudioConfigured`, `AudioStop`.
    pub replies: StreamRoute,
    /// Host outbound / viewer inbound `AudioPacket` datagrams.
    pub packets: DatagramRoute,
    /// The channel binding every audio record carries.
    pub binding: u32,
    /// Complete-record bound on the packet route.
    pub packet_maximum: usize,
}

impl NegotiatedMedia {
    /// Positive selection of audio-down by an OBSERVER in this negotiation.
    /// A control selection never carries audio in this slice.
    pub fn audio_selected(&self) -> bool {
        self.selection.role == Role::Observe
            && self.selection.capabilities.iter().any(|c| {
                c.name == fr_wire::audio::CAPABILITY && c.version == fr_wire::audio::VERSION
            })
    }
    /// Join the completed audio-down attachment. It must belong to this
    /// connection and view (same parent tuple, its own binding ID) and carry
    /// exactly the audio lanes; a second join refuses.
    pub fn attach_audio(&mut self, q: &QuicRecords, channel: &MediaChannel) -> Result<(), Error> {
        self.check(q)?;
        if !self.audio_selected() || self.audio.is_some() {
            return Err(Error::InvalidRoutes);
        }
        if channel.completed_limits(q).map_err(Error::Transport)? != self.selection.limits {
            return Err(Error::InvalidRoutes);
        }
        let audio = channel.completed_on(q).map_err(Error::Transport)?;
        let binding = audio.descriptor.binding;
        let host = self.is_host();
        let (control, replies) = if host {
            (audio.outbound, audio.inbound)
        } else {
            (audio.inbound, audio.outbound)
        };
        if audio.descriptor.role != MediaRole::AudioDown
            || !super::same_view(self.binding(), binding)
            || [
                self.configuration.descriptor.binding.parent.id,
                self.recovery.descriptor.binding.parent.id,
                self.video.descriptor.binding.parent.id,
            ]
            .contains(&binding.parent.id)
            || control.messages != Messages::AudioControl
            || replies.messages != Messages::AudioReplies
            || !audio
                .datagram
                .is_some_and(|d| d.kind == 0x0062 && d.outbound == host)
        {
            return Err(Error::InvalidRoutes);
        }
        self.audio = Some(Box::new(audio));
        Ok(())
    }
    /// `Ok(None)` is typed absence: not selected, or not attached. An error
    /// means the attached lane itself is gone (retired/reset), not video.
    pub fn audio_lanes(&self, q: &QuicRecords) -> Result<Option<AudioLanes>, Error> {
        if !q.is_bound_to(&self.connection) {
            return Err(Error::ForeignConnection);
        }
        let Some(audio) = self
            .audio
            .as_deref()
            .copied()
            .filter(|_| self.audio_selected())
        else {
            return Ok(None);
        };
        let packets = audio.datagram.ok_or(Error::InvalidRoutes)?;
        if q.is_closed() {
            return Err(Error::Closed);
        }
        if !q.has_route(Route::Stream(audio.outbound))
            || !q.has_route(Route::Stream(audio.inbound))
            || !q.has_route(Route::Datagram(packets))
        {
            return Err(Error::InvalidRoutes);
        }
        if q.receive_ended(audio.inbound).map_err(Error::Transport)? {
            return Err(Error::Closed);
        }
        let (control, replies) = if self.is_host() {
            (audio.outbound, audio.inbound)
        } else {
            (audio.inbound, audio.outbound)
        };
        Ok(Some(AudioLanes {
            control,
            replies,
            packets,
            binding: audio.descriptor.binding.parent.id,
            // Already min(control bound, datagram record bound) at attachment.
            packet_maximum: usize::try_from(audio.byte_allowance)
                .map_err(|_| Error::InvalidRoutes)?,
        }))
    }
}

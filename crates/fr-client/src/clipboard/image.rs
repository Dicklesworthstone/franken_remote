//! Client controller image clipboard bound to the original accepted grant.
//!
//! Negotiated optional capability carried over the transfer channel.
//! Validates untrusted image dimensions and decompression budgets before native publication.
//! Suppresses echo loops using cryptographic content hashes (SHA-256) and source stamps.

use super::{Error, ProjectedClock};
use crate::input::ClientInstant;
use fr_core::{
    clipboard::{
        ClipboardSwitch, Endpoint, Receipt, Stamp,
        authority::Monitor,
        image::{
            ImageBegin, ImageClipboardSession, ImageClipboardSink, ImageMetadata, inspect_png,
        },
    },
    limits::ProtocolLimits,
};
use std::{cell::Cell, fmt};

/// Client controller image clipboard manager.
pub struct ControllerImageClipboard {
    session: ImageClipboardSession,
    clock: ProjectedClock,
    last_offered: Option<(Stamp, [u8; 32])>,
}

impl fmt::Debug for ControllerImageClipboard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ControllerImageClipboard")
            .field("closed", &self.session.is_closed())
            .field("buffered_bytes", &self.session.buffered_bytes())
            .finish_non_exhaustive()
    }
}

impl ControllerImageClipboard {
    /// Creates a new client-side image clipboard controller bound to the host input monitor.
    pub(crate) fn new(
        monitor: Monitor,
        limits: ProtocolLimits,
        granted: bool,
        clock: ProjectedClock,
        now: ClientInstant,
    ) -> Result<Self, Error> {
        let at = clock.sample(now)?;
        let session =
            ImageClipboardSession::new(monitor, Endpoint::Controller, limits, granted, at)
                .map_err(|_| Error::Permission)?;
        Ok(Self {
            session,
            clock,
            last_offered: None,
        })
    }

    pub fn local_switch(&self) -> ClipboardSwitch {
        self.session.local_switch()
    }

    pub fn peer_switch(&self) -> ClipboardSwitch {
        self.session.peer_switch()
    }

    pub fn is_closed(&self) -> bool {
        self.session.is_closed()
    }

    pub fn close(&mut self) {
        self.session.close();
    }

    /// Prepares an outbound image offer from local clipboard data.
    /// Validates the PNG payload and ensures it fits within protocol limits.
    pub fn offer(
        &mut self,
        id: u128,
        png_bytes: &[u8],
        origin: Option<Stamp>,
        now: ClientInstant,
    ) -> Result<(ImageBegin, ImageMetadata), Error> {
        let at = match self.clock.sample(now) {
            Ok(at) => at,
            Err(e) => {
                self.close();
                return Err(e);
            }
        };

        // Validate untrusted PNG data
        let metadata =
            inspect_png(png_bytes, &ProtocolLimits::ABSOLUTE).map_err(|_| Error::InvalidLimits)?;

        // Check echo suppression against last offered image
        if let Some((last_stamp, last_hash)) = self.last_offered
            && (origin.is_some_and(|s| s == last_stamp) || last_hash == metadata.content_sha256)
        {
            return Err(Error::Stopped);
        }

        // Check echo suppression against last published image
        let is_new = self
            .session
            .local_change(origin, Some(&metadata.content_sha256), at)
            .map_err(|_| Error::Stopped)?;

        if !is_new {
            return Err(Error::Stopped);
        }

        let chunk_size =
            u32::try_from(fr_core::clipboard::image::MAX_IMAGE_CHUNK_BYTES).unwrap_or(16_384);
        let chunks = metadata.total_bytes.div_ceil(chunk_size);

        let stamp = Stamp {
            id,
            source: Endpoint::Controller,
            sequence: 1,
        };

        let begin = ImageBegin {
            binding: self.session.binding(),
            stamp,
            metadata,
            chunks,
        };

        self.last_offered = Some((stamp, metadata.content_sha256));
        Ok((begin, metadata))
    }

    /// Admits an inbound image transfer declaration.
    pub fn begin(&mut self, begin: ImageBegin, now: ClientInstant) -> Result<(), Error> {
        let at = self.clock.sample(now)?;
        self.session.begin(begin, at).map_err(|_| Error::Stopped)
    }

    /// Appends a sequential image payload chunk.
    pub fn chunk(
        &mut self,
        stamp: Stamp,
        index: u32,
        offset: u32,
        bytes: &[u8],
        now: ClientInstant,
    ) -> Result<(), Error> {
        let at = self.clock.sample(now)?;
        self.session
            .chunk(stamp, index, offset, bytes, at)
            .map_err(|_| Error::Stopped)
    }

    /// Cancels an in-flight transfer.
    pub fn cancel(&mut self, stamp: Stamp) -> Result<(), Error> {
        self.session.cancel(stamp).map_err(|_| Error::Stopped)
    }

    /// Commits the complete received image to the native OS sink.
    pub fn commit(
        &mut self,
        stamp: Stamp,
        total_bytes: u32,
        sink: &mut impl ImageClipboardSink,
        mut clock: impl FnMut() -> ClientInstant,
    ) -> Result<Receipt, Error> {
        let failure = Cell::new(None);
        let result = self.session.commit(stamp, total_bytes, sink, || {
            self.clock.checked(clock(), &failure)
        });
        result.map_err(|_| failure.get().unwrap_or(Error::Stopped))
    }
}

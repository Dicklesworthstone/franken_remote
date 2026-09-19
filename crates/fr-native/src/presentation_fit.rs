//! Explicit CPU nearest-neighbour downscaling for an immutable local viewport.
//! No codec reconfiguration, GPU/zero-copy claim, or visibility witness. This
//! owner holds exactly one extra bounded BGRA frame and never queues pictures.
#![forbid(unsafe_code)]
use super::{BgraFrame, NativeError, frame_len, zeroed};
use fr_core::limits::ProtocolLimits;
use fr_media::worker::presentation::{Fit, X11Target};

pub struct FittedFrame {
    source: (u32, u32),
    placement: Fit,
    output: BgraFrame,
}
impl std::fmt::Debug for FittedFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FittedFrame")
            .field("source", &self.source)
            .field("placement", &self.placement)
            .field("retained_bytes", &self.output.bytes.capacity())
            .finish_non_exhaustive()
    }
}
impl FittedFrame {
    pub fn new(
        source_width: u32,
        source_height: u32,
        target: X11Target,
        limits: ProtocolLimits,
    ) -> Result<Self, NativeError> {
        frame_len(source_width, source_height, &limits)?;
        let length = frame_len(target.width(), target.height(), &limits)?;
        let placement = Fit::new(source_width, source_height, target)
            .map_err(|_| NativeError::InvalidConfiguration)?;
        let mut bytes = zeroed(length)?;
        // Opaque black bars agree with the X11 BGRA snapshot representation.
        for pixel in bytes.as_chunks_mut::<4>().0 {
            pixel[3] = 255;
        }
        Ok(Self {
            source: (source_width, source_height),
            placement,
            output: BgraFrame {
                width: target.width(),
                height: target.height(),
                bytes,
            },
        })
    }
    pub const fn placement(&self) -> Fit {
        self.placement
    }
    pub fn retained_bytes(&self) -> usize {
        self.output.bytes.capacity()
    }
    /// The borrow prevents retaining the old output while accepting another
    /// picture. An unexpected source size is terminal to the containing worker;
    /// it is never interpreted using a stale layout or cropped implicitly.
    pub fn render(&mut self, source: &BgraFrame) -> Result<&BgraFrame, NativeError> {
        if (source.width, source.height) != self.source {
            return Err(NativeError::GeometryChanged);
        }
        let fit = self.placement;
        // Dimensions were checked before allocation. u64 products cover the
        // complete u32 input domain, and each resulting index is in its row.
        for y in 0..fit.height {
            let sy = u64::from(y) * u64::from(source.height) / u64::from(fit.height);
            let src = usize::try_from(sy * u64::from(source.width) * 4)
                .map_err(|_| NativeError::InvalidConfiguration)?;
            let dst = usize::try_from(
                (u64::from(y + fit.y) * u64::from(self.output.width) + u64::from(fit.x)) * 4,
            )
            .map_err(|_| NativeError::InvalidConfiguration)?;
            for x in 0..fit.width {
                let sx = usize::try_from(
                    u64::from(x) * u64::from(source.width) / u64::from(fit.width) * 4,
                )
                .map_err(|_| NativeError::InvalidConfiguration)?;
                let dx = usize::try_from(x).map_err(|_| NativeError::InvalidConfiguration)? * 4;
                self.output.bytes[dst + dx..dst + dx + 4]
                    .copy_from_slice(&source.bytes[src + sx..src + sx + 4]);
            }
        }
        Ok(&self.output)
    }
}

#[cfg(test)]
#[path = "presentation_fit/tests.rs"]
mod tests;

//! Pure codec for one already-admitted capture request and its replies.
//! Authority, immutable IPC deadlines and source identity remain with the caller.
use fr_media::{
    access_unit::{EncodedAccessUnit, FrameId},
    worker::{self, Configuration, Kind, UnchangedCapture},
};

#[derive(Debug)]
pub(super) enum Content {
    Encoded(EncodedAccessUnit),
    Unchanged(UnchangedCapture),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Error {
    Protocol(worker::Error),
    InvalidFrame,
    Backpressure,
}
impl From<worker::Error> for Error {
    fn from(error: worker::Error) -> Self {
        Self::Protocol(error)
    }
}

/// This preflight runs before display selection, authority or recovery mutation.
pub(super) fn validate_bitrate(
    configuration: Configuration,
    bitrate: Option<u32>,
) -> Result<(), worker::Error> {
    if let Some(rate) = bitrate {
        if configuration.backend != worker::Backend::SoftwareExplicit {
            return Err(worker::Error::Unsupported);
        }
        if !(10_000..=configuration.bitrate).contains(&rate) {
            return Err(worker::Error::ResourceLimit);
        }
    }
    Ok(())
}

/// Original values frozen after admission and candidate retirement, reused for
/// every poll without renewing the request or its source observation timestamp.
pub(super) struct Request {
    pub(super) configuration: Configuration,
    pub(super) frame: FrameId,
    pub(super) observed_micros: u64,
    pub(super) force_idr: bool,
    pub(super) conditional: bool,
    pub(super) bitrate: Option<u32>,
    pub(super) reference: Option<FrameId>,
}
impl Request {
    pub(super) fn payload(&self) -> Result<(Kind, Vec<u8>), worker::Error> {
        if let Some(bitrate) = self.bitrate {
            Ok((
                Kind::CaptureAtRate,
                worker::rate::Capture {
                    frame: self.frame,
                    observed_micros: self.observed_micros,
                    force_idr: self.force_idr,
                    only_if_changed: self.conditional,
                    bitrate,
                }
                .encode()?,
            ))
        } else {
            Ok((
                if self.conditional {
                    Kind::CaptureIfChanged
                } else {
                    Kind::Capture
                },
                worker::capture_payload(self.frame, self.observed_micros, self.force_idr),
            ))
        }
    }

    /// None means the same original operation needs another poll. Only a fully
    /// validated encoded result may advance the caller's last-capture anchor.
    pub(super) fn response(
        &self,
        kind: Kind,
        body: Vec<u8>,
    ) -> Result<Option<(Content, Option<u32>)>, Error> {
        match kind {
            Kind::Unit | Kind::RateUnit => self.unit(kind, body).map(Some),
            Kind::Unchanged | Kind::RateUnchanged if self.conditional && !self.force_idr => {
                self.unchanged(kind, &body).map(Some)
            }
            Kind::NeedInput => Ok(None),
            _ => Err(Error::Backpressure),
        }
    }

    fn unit(&self, kind: Kind, body: Vec<u8>) -> Result<(Content, Option<u32>), Error> {
        let (actual, unit) = if kind == Kind::RateUnit && self.bitrate.is_some() {
            let (rate, unit) = worker::rate::parse_unit(body, &self.configuration.limits()?)?;
            if rate > self.configuration.bitrate {
                return Err(Error::InvalidFrame);
            }
            (Some(rate), unit)
        } else if kind == Kind::Unit && self.bitrate.is_none() {
            (
                None,
                worker::parse_unit(body, &self.configuration.limits()?)?,
            )
        } else {
            return Err(Error::InvalidFrame);
        };
        if unit.frame() != self.frame
            || unit.config_generation() != self.configuration.generation
            || unit.capture_micros() != self.observed_micros
            || (self.force_idr && !unit.is_idr())
        {
            return Err(Error::InvalidFrame);
        }
        Ok((Content::Encoded(unit), actual))
    }

    fn unchanged(&self, kind: Kind, body: &[u8]) -> Result<(Content, Option<u32>), Error> {
        let (actual, proof) = if kind == Kind::RateUnchanged && self.bitrate.is_some() {
            let (rate, proof) = worker::rate::parse_unchanged(body)?;
            if rate > self.configuration.bitrate {
                return Err(Error::InvalidFrame);
            }
            (Some(rate), proof)
        } else if kind == Kind::Unchanged && self.bitrate.is_none() {
            (None, UnchangedCapture::decode(body)?)
        } else {
            return Err(Error::InvalidFrame);
        };
        if proof.candidate != self.frame
            || Some(proof.reference) != self.reference
            || proof.observed_micros != self.observed_micros
        {
            return Err(Error::InvalidFrame);
        }
        Ok((Content::Unchanged(proof), actual))
    }
}

#[cfg(test)]
#[path = "protocol_tests.rs"]
mod tests;

//! Explicit pixels from the original authenticated foreground owner. No paths,
//! PNG encoding, capture loops or input authority cross this local IPC boundary.
use super::{Cx, Deadline, Error, HANDSHAKE, Info, bounded, identified};
use asupersync::io::{AsyncReadExt, AsyncWriteExt};
use fr_media::worker::presentation_snapshot::{self, Snapshot};
use serde::{Deserialize, Serialize};
use std::time::Instant;
#[cfg(any(feature = "linux-desktop", test))]
use {
    super::{UnixStream, input, observation},
    fr_client::robot::snapshot::Image,
    std::{
        future::{Future, poll_fn},
        pin::pin,
    },
};

#[path = "screenshot/metadata.rs"]
mod metadata;
pub(crate) use metadata::Metadata;

pub(super) const REQUEST: [u8; 8] = *b"FRSC1SP\0";
const RESPONSE: [u8; 8] = *b"FRSC1PX\0";
const MAX_METADATA: usize = 2048;
const SEAL: [u8; 1] = [0xa5];
const HEADER_BYTES: usize = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Refusal {
    Unavailable,
    NotReady,
    NotCurrent,
    Busy,
    Expired,
    Closed,
}
impl From<fr_client::robot::snapshot::Error> for Refusal {
    fn from(error: fr_client::robot::snapshot::Error) -> Self {
        use fr_client::robot::snapshot::Error as Source;
        match error {
            Source::Busy => Self::Busy,
            Source::Expired => Self::Expired,
            Source::Unavailable => Self::Unavailable,
            Source::NotCurrent => Self::NotCurrent,
            Source::Closed => Self::Closed,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Reply {
    Image { metadata: Box<Metadata> },
    Refused { reason: Refusal },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReadError {
    Local(Error),
    Refused(Refusal),
}
/// Pixel storage deliberately has no Debug implementation.
pub(crate) struct Observed {
    pub info: Info,
    pub metadata: Metadata,
    pub snapshot: Snapshot,
    pub round_trip_ms: u64,
}

/// Identify and compare once on this same credential-checked socket. The original
/// total deadline covers identification, export, pixels and the final seal.
pub(crate) async fn read(cx: &Cx, target: &str, expected: &str) -> Result<Observed, ReadError> {
    crate::robot_input::id(expected).map_err(|_| ReadError::Local(Error::InvalidInstance))?;
    let began = Instant::now();
    let deadline = Deadline::new(cx, HANDSHAKE).map_err(ReadError::Local)?;
    bounded(cx, deadline, async {
        let (mut stream, info, instance) = identified(cx, target).await?;
        if info.instance != expected {
            return Err(Error::WrongOwner);
        }
        let mut request = [0; 24];
        request[..8].copy_from_slice(&REQUEST);
        request[8..].copy_from_slice(&instance);
        stream
            .write_all(&request)
            .await
            .map_err(|_| Error::Unavailable)?;
        let mut header = [0; HEADER_BYTES];
        stream
            .read_exact(&mut header)
            .await
            .map_err(|_| Error::Unavailable)?;
        let (metadata_length, pixel_length) = reply_lengths(&header, instance)?;
        let mut bytes = [0; MAX_METADATA];
        stream
            .read_exact(&mut bytes[..metadata_length])
            .await
            .map_err(|_| Error::Protocol)?;
        let reply: Reply =
            serde_json::from_slice(&bytes[..metadata_length]).map_err(|_| Error::Protocol)?;
        let metadata = match reply {
            Reply::Refused { reason } if pixel_length == 0 => {
                read_seal(&mut stream).await?;
                return Ok(Err(ReadError::Refused(reason)));
            }
            Reply::Image { metadata } if pixel_length != 0 => {
                metadata.validate_length(pixel_length)?;
                *metadata
            }
            _ => return Err(Error::Protocol),
        };
        // Both framing and metadata have been validated before bounded allocation.
        let mut body = Vec::new();
        body.try_reserve_exact(pixel_length)
            .map_err(|_| Error::Unavailable)?;
        if body.capacity() > presentation_snapshot::MAX_BODY_BYTES {
            return Err(Error::Protocol);
        }
        body.resize(pixel_length, 0);
        stream
            .read_exact(&mut body)
            .await
            .map_err(|_| Error::Unavailable)?;
        read_seal(&mut stream).await?;
        let snapshot = Snapshot::decode(body).map_err(|_| Error::Protocol)?;
        metadata.validate_image(&snapshot)?;
        let round_trip_ms = elapsed_ms(began)?;
        Ok(Ok(Observed {
            info,
            metadata,
            snapshot,
            round_trip_ms,
        }))
    })
    .await
    .map_err(ReadError::Local)?
}
async fn read_seal(stream: &mut super::UnixStream) -> Result<(), Error> {
    let mut seal = [0];
    stream
        .read_exact(&mut seal)
        .await
        .map_err(|_| Error::Unavailable)?;
    if seal != SEAL {
        return Err(Error::Protocol);
    }
    Ok(())
}
fn elapsed_ms(began: Instant) -> Result<u64, Error> {
    let duration = Instant::now()
        .checked_duration_since(began)
        .ok_or(Error::Unavailable)?;
    u64::try_from(duration.as_nanos().div_ceil(1_000_000)).map_err(|_| Error::Unavailable)
}
fn reply_lengths(header: &[u8; HEADER_BYTES], instance: [u8; 16]) -> Result<(usize, usize), Error> {
    if header[..8] != RESPONSE || header[8..24] != instance {
        return Err(Error::Protocol);
    }
    let metadata = usize::from(u16::from_be_bytes(header[24..26].try_into().unwrap()));
    let pixels = usize::try_from(u32::from_be_bytes(header[26..].try_into().unwrap()))
        .map_err(|_| Error::Protocol)?;
    if metadata == 0
        || metadata > MAX_METADATA
        || (pixels != 0
            && (!pixels
                .checked_sub(presentation_snapshot::HEADER_BYTES)
                .is_some_and(|n| n > 0 && n.is_multiple_of(4))
                || pixels > presentation_snapshot::MAX_BODY_BYTES))
    {
        return Err(Error::Protocol);
    }
    Ok((metadata, pixels))
}
#[cfg(any(feature = "linux-desktop", test))]
fn header(instance: [u8; 16], metadata: usize, pixels: usize) -> Result<[u8; HEADER_BYTES], Error> {
    let mut result = [0; HEADER_BYTES];
    result[..8].copy_from_slice(&RESPONSE);
    result[8..24].copy_from_slice(&instance);
    result[24..26].copy_from_slice(
        &u16::try_from(metadata)
            .map_err(|_| Error::Protocol)?
            .to_be_bytes(),
    );
    result[26..].copy_from_slice(
        &u32::try_from(pixels)
            .map_err(|_| Error::Protocol)?
            .to_be_bytes(),
    );
    reply_lengths(&result, instance)?;
    Ok(result)
}

#[cfg(any(feature = "linux-desktop", test))]
pub(super) async fn respond(
    stream: &mut UnixStream,
    instance: [u8; 16],
    publisher: Option<&observation::Publisher>,
    automation: Option<&input::Shared>,
) -> Result<(), Error> {
    let captured = match (publisher, automation) {
        (Some(publisher), None) => publisher.capture().await,
        (None, Some(automation)) => automation.capture().await,
        _ => Err(Refusal::Unavailable),
    };
    let (reply, image) = match captured {
        Ok((metadata, image)) => (
            Reply::Image {
                metadata: Box::new(metadata),
            },
            Some(image),
        ),
        Err(reason) => (Reply::Refused { reason }, None),
    };
    // No shared-state guard survives capture or serialization. The Image owns
    // the one global export reservation until this socket finishes or drops.
    let bytes = serde_json::to_vec(&reply).map_err(|_| Error::Protocol)?;
    if bytes.len() > MAX_METADATA {
        return Err(Error::Protocol);
    }
    let pixel_length = image.as_ref().map_or(0, |image| {
        presentation_snapshot::HEADER_BYTES + image.snapshot().pixels().len()
    });
    let header = header(instance, bytes.len(), pixel_length)?;
    if let Some(image) = &image {
        checked_write(stream, &header, image, || Ok(())).await?;
        checked_write(stream, &bytes, image, || Ok(())).await?;
        let snapshot = image.snapshot();
        let mut framing = [0; presentation_snapshot::HEADER_BYTES];
        framing[..24].copy_from_slice(&snapshot.stamp().encode());
        framing[24..28].copy_from_slice(&snapshot.width().to_be_bytes());
        framing[28..].copy_from_slice(&snapshot.height().to_be_bytes());
        checked_write(stream, &framing, image, || Ok(())).await?;
        for chunk in snapshot.pixels().chunks(64 * 1024) {
            checked_write(stream, chunk, image, || Ok(())).await?;
        }
        let Reply::Image { metadata } = &reply else {
            return Err(Error::Protocol);
        };
        checked_write(stream, &SEAL, image, || match (publisher, automation) {
            (Some(publisher), None) => publisher.check_export(metadata),
            (None, Some(automation)) => automation.check_export(metadata),
            _ => Err(Refusal::Unavailable),
        })
        .await?;
    } else {
        stream
            .write_all(&header)
            .await
            .map_err(|_| Error::Unavailable)?;
        stream
            .write_all(&bytes)
            .await
            .map_err(|_| Error::Unavailable)?;
        stream
            .write_all(&SEAL)
            .await
            .map_err(|_| Error::Unavailable)?;
    }
    Ok(())
}
/// Recheck before EVERY write poll, including a seal that became socket-blocked.
/// Revocation/recovery or a changed input pin cannot slip through a resumed await.
#[cfg(any(feature = "linux-desktop", test))]
async fn checked_write(
    stream: &mut UnixStream,
    bytes: &[u8],
    image: &Image,
    check_owner: impl Fn() -> Result<(), Refusal>,
) -> Result<(), Error> {
    let mut writing = pin!(stream.write_all(bytes));
    poll_fn(|task| {
        image.check().map_err(|_| Error::Unavailable)?;
        check_owner().map_err(|_| Error::Unavailable)?;
        writing.as_mut().poll(task).map_err(|_| Error::Unavailable)
    })
    .await
}

#[cfg(test)]
#[path = "screenshot/tests.rs"]
mod tests;

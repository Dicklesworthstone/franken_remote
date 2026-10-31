//! Read-only snapshots on the ORIGINAL authenticated local control socket.
//! The comparison ID pins an invocation; it is not a credential or input lease.
#[cfg(any(feature = "linux-desktop", test))]
use super::UnixStream;
use super::{Cx, Deadline, Error, HANDSHAKE, Info, bounded, identified};
use asupersync::io::{AsyncReadExt, AsyncWriteExt};
use fr_client::robot::live::Snapshot;
#[cfg(any(feature = "linux-desktop", test))]
use fr_client::robot::live::{Observation, Phase};
#[cfg(any(feature = "linux-desktop", test))]
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub(super) const REQUEST: [u8; 8] = *b"FRSC1OB\0";
const RESPONSE: [u8; 8] = *b"FRSC1OV\0";
const MAX_BYTES: usize = 2048;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Observed {
    pub info: Info,
    pub snapshot: Snapshot,
    /// Local request duration, including identification. Never host clock skew.
    pub round_trip_ms: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReadError {
    Local(Error),
    Unavailable,
}

#[cfg(any(feature = "linux-desktop", test))]
#[derive(Clone, Default)]
pub(crate) struct Publisher(
    Arc<Mutex<Observation>>,
    Arc<Mutex<Option<fr_client::robot::snapshot::Handle>>>,
);
#[cfg(any(feature = "linux-desktop", test))]
impl Publisher {
    /// Session-thread work is bounded and never waits for a reader-held lock.
    pub fn update(
        &self,
        action: impl FnOnce(&mut Observation) -> Result<(), fr_client::robot::live::Error>,
    ) -> Result<(), ()> {
        let mut value = self.0.try_lock().map_err(|_| ())?;
        action(&mut value).map_err(|_| ())
    }
    pub fn retire(&self, phase: Phase) {
        let _ = self.update(|value| {
            value.retire(phase);
            Ok(())
        });
        // Drop a handle outside the shared lock; no image work happens here.
        let old = self.1.try_lock().ok().and_then(|mut handle| handle.take());
        drop(old);
    }
    fn read(&self) -> Option<Snapshot> {
        self.0.try_lock().ok()?.snapshot(Instant::now()).ok()
    }
}

/// No retry, no new tailnet connection, and no STOP on this stream. Validate
/// the pin before I/O, then compare the identity and request on the SAME socket.
pub(crate) async fn read(cx: &Cx, target: &str, expected: &str) -> Result<Observed, ReadError> {
    if expected.len() != 32
        || !expected
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(ReadError::Local(Error::InvalidInstance));
    }
    let began = Instant::now();
    let result = async {
        // One overall deadline includes the existing identification sub-operation.
        let deadline = Deadline::new(cx, HANDSHAKE)?;
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
            let mut header = [0; 26];
            stream
                .read_exact(&mut header)
                .await
                .map_err(|_| Error::Unavailable)?;
            let length = reply_length(&header, instance)?;
            if length == 0 {
                return Ok(None);
            }
            // Peer-controlled lengths never size an allocation or stack object.
            let mut body = [0; MAX_BYTES];
            stream
                .read_exact(&mut body[..length])
                .await
                .map_err(|_| Error::Protocol)?;
            let snapshot: Snapshot =
                serde_json::from_slice(&body[..length]).map_err(|_| Error::Protocol)?;
            snapshot.validate().map_err(|_| Error::Protocol)?;
            let round_trip_ms = u64::try_from(
                Instant::now()
                    .checked_duration_since(began)
                    .ok_or(Error::Unavailable)?
                    .as_nanos()
                    .div_ceil(1_000_000),
            )
            .map_err(|_| Error::Unavailable)?;
            Ok(Some(Observed {
                info,
                snapshot,
                round_trip_ms,
            }))
        })
        .await
    }
    .await
    .map_err(ReadError::Local)?;
    result.ok_or(ReadError::Unavailable)
}

fn reply_length(header: &[u8; 26], instance: [u8; 16]) -> Result<usize, Error> {
    if header[..8] != RESPONSE || header[8..24] != instance {
        return Err(Error::Protocol);
    }
    let length = usize::from(u16::from_be_bytes([header[24], header[25]]));
    if length > MAX_BYTES {
        return Err(Error::Protocol);
    }
    Ok(length)
}

#[cfg(any(feature = "linux-desktop", test))]
pub(super) async fn respond(
    stream: &mut UnixStream,
    instance: [u8; 16],
    publisher: Option<&Publisher>,
) -> Result<(), Error> {
    // Snapshot at request processing, never at socket admission. Drop the lock
    // before serialization or any await, so slow readers cannot hold the viewer.
    let snapshot = publisher.and_then(Publisher::read);
    let bytes = snapshot
        .as_ref()
        .map(serde_json::to_vec)
        .transpose()
        .map_err(|_| Error::Protocol)?
        .unwrap_or_default();
    if bytes.len() > MAX_BYTES {
        return Err(Error::Protocol);
    }
    let mut header = [0; 26];
    header[..8].copy_from_slice(&RESPONSE);
    header[8..24].copy_from_slice(&instance);
    header[24..].copy_from_slice(
        &u16::try_from(bytes.len())
            .map_err(|_| Error::Protocol)?
            .to_be_bytes(),
    );
    stream
        .write_all(&header)
        .await
        .map_err(|_| Error::Unavailable)?;
    stream
        .write_all(&bytes)
        .await
        .map_err(|_| Error::Unavailable)
}

#[cfg(test)]
#[path = "observation_tests.rs"]
mod tests;

#[cfg(any(feature = "linux-desktop", test))]
#[path = "observation/screenshot.rs"]
mod screenshot;

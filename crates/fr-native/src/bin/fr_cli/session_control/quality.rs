//! One content-free quality snapshot on the existing same-user control socket.
//! It is a host report, never remote readiness, measured throughput or a lease.
use super::{Cx, Deadline, Error, HANDSHAKE, Info, bounded, identified};
use asupersync::io::{AsyncReadExt, AsyncWriteExt};
use fr_wire::quality_decision::{Decision, Mode, Reason};
#[cfg(any(feature = "linux-desktop", test))]
use std::sync::{Arc, Mutex};

pub(super) const REQUEST: [u8; 8] = *b"FRSC1QY\0";
const RESPONSE: [u8; 8] = *b"FRSC1QV\0";
const MAX_BYTES: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Report {
    pub sequence: u64,
    pub capture_interval_us: u32,
    pub mode: String,
    pub reason: String,
    pub applied_bitrate_bps: Option<u32>,
    pub pending_bitrate_bps: Option<u32>,
}
impl From<Decision> for Report {
    fn from(value: Decision) -> Self {
        Self {
            sequence: value.sequence,
            capture_interval_us: value.capture_interval_us,
            mode: match value.mode {
                Mode::Active => "active",
                Mode::Idle => "idle",
            }
            .into(),
            reason: match value.reason {
                Reason::Hold => "hold",
                Reason::SourceWork => "source_work",
                Reason::SendAdmission => "send_admission",
                Reason::CaptureCredit => "capture_credit",
                Reason::ReceiverBacklog => "receiver_backlog",
                Reason::DecoderWork => "decoder_work",
                Reason::HeadroomProbe => "headroom_probe",
                Reason::VerifiedIdle => "verified_idle",
                Reason::ChangedAfterIdle => "changed_after_idle",
                Reason::EvidenceGap => "evidence_gap",
            }
            .into(),
            applied_bitrate_bps: value.applied_bitrate_bps,
            pending_bitrate_bps: value.pending_bitrate_bps,
        }
    }
}
impl Report {
    fn validate(&self) -> Result<(), Error> {
        let mode = match self.mode.as_str() {
            "active" => Mode::Active,
            "idle" => Mode::Idle,
            _ => return Err(Error::Protocol),
        };
        let reason = match self.reason.as_str() {
            "hold" => Reason::Hold,
            "source_work" => Reason::SourceWork,
            "send_admission" => Reason::SendAdmission,
            "capture_credit" => Reason::CaptureCredit,
            "receiver_backlog" => Reason::ReceiverBacklog,
            "decoder_work" => Reason::DecoderWork,
            "headroom_probe" => Reason::HeadroomProbe,
            "verified_idle" => Reason::VerifiedIdle,
            "changed_after_idle" => Reason::ChangedAfterIdle,
            "evidence_gap" => Reason::EvidenceGap,
            _ => return Err(Error::Protocol),
        };
        Decision {
            sequence: self.sequence,
            capture_interval_us: self.capture_interval_us,
            mode,
            reason,
            applied_bitrate_bps: self.applied_bitrate_bps,
            pending_bitrate_bps: self.pending_bitrate_bps,
        }
        .validate()
        .map_err(|_| Error::Protocol)
    }
    pub fn text(&self) -> String {
        let applied = self
            .applied_bitrate_bps
            .map_or_else(|| "unknown".into(), |v| format!("{v} bps"));
        let pending = self
            .pending_bitrate_bps
            .map_or_else(|| "none".into(), |v| format!("{v} bps (not yet applied)"));
        format!(
            "Latest host quality decision #{} for the current media view: capture interval {} us, {} ({}); encoder bitrate applied {applied}, pending {pending}. These are reported settings, not measured delivery rate or view freshness.\n",
            self.sequence, self.capture_interval_us, self.mode, self.reason
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Status {
    pub info: Info,
    pub quality: Option<Report>,
}

#[cfg(any(feature = "linux-desktop", test))]
#[derive(Clone, Default)]
pub(crate) struct Publisher(Arc<Mutex<Option<frd::session_startup::ViewerQualityHandle>>>);
#[cfg(any(feature = "linux-desktop", test))]
impl Publisher {
    /// Install once per attempt. Readers hold this lock only while cloning a
    /// weak handle, never during snapshot work or I/O; installation cannot be
    /// dropped because a simultaneous status request held the previous handle.
    pub fn set(&self, handle: Option<frd::session_startup::ViewerQualityHandle>) {
        let old = std::mem::replace(
            &mut *self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            handle,
        );
        drop(old);
    }
    fn read(&self) -> Option<Report> {
        let handle = self.0.try_lock().ok()?.clone()?;
        handle.current().map(Report::from)
    }
}

/// Identify and inspect on one original socket under one bounded deadline.
/// This request cannot submit input, stop a session or initiate a host dial.
pub(crate) async fn read(cx: &Cx, target: &str) -> Result<Status, Error> {
    bounded(cx, Deadline::new(cx, HANDSHAKE)?, async {
        let (mut stream, info, instance) = identified(cx, target).await?;
        let mut request = [0; 24];
        request[..8].copy_from_slice(&REQUEST);
        request[8..].copy_from_slice(&instance);
        stream
            .write_all(&request)
            .await
            .map_err(|_| Error::Unavailable)?;
        let mut header = [0; 26];
        let first = stream
            .read(&mut header[..1])
            .await
            .map_err(|_| Error::Unavailable)?;
        if first == 0 {
            // Older foreground binaries identify correctly, then close on an
            // unknown optional request. Preserve that observed local identity;
            // no quality, remote readiness or continued liveness is inferred.
            return Ok(Status {
                info,
                quality: None,
            });
        }
        // Once any extension byte arrives, truncation is corruption, not lack
        // of support. Never downgrade malformed or foreign records to unknown.
        stream
            .read_exact(&mut header[1..])
            .await
            .map_err(|_| Error::Protocol)?;
        let length = reply_length(header, instance)?;
        let quality = if length == 0 {
            None
        } else {
            let mut bytes = [0; MAX_BYTES];
            stream
                .read_exact(&mut bytes[..length])
                .await
                .map_err(|_| Error::Protocol)?;
            let report: Report =
                serde_json::from_slice(&bytes[..length]).map_err(|_| Error::Protocol)?;
            report.validate()?;
            Some(report)
        };
        Ok(Status { info, quality })
    })
    .await
}
fn reply_length(header: [u8; 26], instance: [u8; 16]) -> Result<usize, Error> {
    let length = usize::from(u16::from_be_bytes([header[24], header[25]]));
    if header[..8] != RESPONSE || header[8..24] != instance || length > MAX_BYTES {
        return Err(Error::Protocol);
    }
    Ok(length)
}

#[cfg(any(feature = "linux-desktop", test))]
pub(super) async fn respond(
    stream: &mut super::UnixStream,
    instance: [u8; 16],
    publisher: &Publisher,
) -> Result<(), Error> {
    // Acquire only a copy at processing time. No shared lock crosses an await.
    let bytes = publisher
        .read()
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
#[path = "quality_tests.rs"]
mod tests;

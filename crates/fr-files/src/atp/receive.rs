//! Existing ATP full-object frames joined to the original controller's disk worker.
//!
//! This is the payload adapter INSIDE a separately authenticated file attachment,
//! not an ATP listener or an alternative identity handshake. The caller must bound
//! the outer `FileOffer/FileChunk` records and subtract their envelope from F before
//! constructing this owner. It never accepts paths, jobs or credentials from an
//! ATP handshake. The selected 0.5.0 portable full-object profiles is documented in
//! `FILE_RECEIVE.md`; directory trees use explicit profile 2; metadata, delta/resume and `RaptorQ` refuse.
use crate::{
    receive::{DirectoryManifest, DropDirectory, Expected, MAX_CHUNK_BYTES},
    session::{Binding, Permission, Policy, Progress, VerifiedOffer},
    worker::{self, Completion, Mailbox, Task},
};
use asupersync::{
    atp::safety::validate_portable_path_component,
    bytes::BytesMut,
    codec::Decoder,
    cx::Cx,
    net::atp::{
        protocol::{
            codec::AtpFrameCodec,
            frames::{Frame, FrameType, ProtocolVersion},
        },
        transport_tcp::{ReceiveReceipt, TransferManifest},
    },
};
use fr_core::input_submission::InputSession;
use std::fmt;

pub const MAX_FRAME_BYTES: usize = 65_536;
pub const MAX_MANIFEST_BYTES: usize = 32 * 1024;
/// At most this many bytes are needed for an ATP request or sanitized proof.
pub const MAX_REPLY_BYTES: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Limits,
    WrongBinding,
    WrongTransfer,
    Busy,
    Closed,
    Frame,
    Manifest,
    UnsupportedProfile,
    Order,
    ResourceLimit,
    BufferTooSmall,
    Worker(worker::Error),
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "file-atp: {self:?}")
    }
}
impl std::error::Error for Error {}

/// Actual worker outcome, not a guess based on receiving `ObjectComplete`. The
/// caller retains this value if reply encoding or the network send fails. A
/// published result MUST NOT be retried even when proof delivery is uncertain.
pub struct Event {
    pub id: u64,
    outcome: Result<Completion, worker::Error>,
    request_root: Option<String>,
    frame_bytes: usize,
    profile: u16,
    files: u32,
}
impl fmt::Debug for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileAtpEvent")
            .field("id", &self.id)
            .field("outcome", &self.outcome)
            .finish_non_exhaustive()
    }
}
impl Event {
    pub fn profile(&self) -> u16 {
        self.profile
    }
    pub fn outcome(&self) -> Result<Completion, worker::Error> {
        self.outcome
    }
    /// Encode the pinned upstream ObjectRequest/Proof schemas. Staged writes and
    /// refusals have no success reply. Outer `FileComplete` must also carry the
    /// publication durability stage from the actual `Published` receipt.
    /// No host path, transfer content or native error string enters the proof.
    pub fn encode_reply(&self, output: &mut [u8]) -> Result<Option<usize>, Error> {
        let (kind, payload) = match self.outcome {
            Ok(Completion::Begun(progress)) => {
                let root = self.request_root.as_ref().ok_or(Error::Order)?;
                // DeltaObjectRequest is crate-private upstream. These are its
                // exact full-object fields, not a new repair/resumption schema.
                let request = serde_json::json!({
                    "mode": "full_object",
                    "fallback_reason": "portable_full_object",
                    "sender_merkle_root_hex": root,
                    "missing_bytes": progress.total_bytes,
                    "shared_chunks": 0,
                    "stale_chunks": 0,
                    "missing_chunks": [],
                });
                (
                    FrameType::ObjectRequest,
                    serde_json::to_vec(&request).map_err(|_| Error::Frame)?,
                )
            }
            Ok(Completion::Published(receipt)) => {
                let proof = ReceiveReceipt {
                    committed: true,
                    bytes_received: receipt.bytes,
                    files: self.files,
                    sha_ok: true,
                    merkle_ok: true,
                    symbols_accepted: 0,
                    feedback_rounds: 0,
                    decode_count: 0,
                    decode_micros: 0,
                    reason: None,
                    committed_paths: Vec::new(),
                };
                (
                    FrameType::Proof,
                    serde_json::to_vec(&proof).map_err(|_| Error::Frame)?,
                )
            }
            _ => return Ok(None),
        };
        let wire = Frame::new(ProtocolVersion::CURRENT, kind, payload)
            .and_then(|frame| frame.to_wire_bytes())
            .map_err(|_| Error::Frame)?;
        if wire.len() > MAX_REPLY_BYTES || wire.len() > self.frame_bytes {
            return Err(Error::ResourceLimit);
        }
        let target = output.get_mut(..wire.len()).ok_or(Error::BufferTooSmall)?;
        target.copy_from_slice(&wire);
        Ok(Some(wire.len()))
    }
}

#[derive(Clone, Copy)]
enum Operation {
    Begin,
    Write,
    Complete,
}
struct Pending {
    id: u64,
    sequence: u64,
    operation: Operation,
    request_root: Option<String>,
}

/// One original input owner, one separately approved drop directory, one pending
/// disk operation. No frame is retained while waiting for disk: the worker owns
/// only its bounded payload. The transport must stop reading this bulk lane on
/// Busy and continue servicing input/revocation on their independent lanes.
pub struct Receiver {
    mailbox: Mailbox,
    frame_bytes: usize,
    active: Option<Progress>,
    pending: Option<Pending>,
    profile: u16,
    sizes: Vec<u64>,
    offsets: Vec<u64>,
}
impl fmt::Debug for Receiver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AtpFileReceiver([original attachment])")
    }
}
impl Receiver {
    /// `frame_bytes` is the negotiated F MINUS the complete FRD0 file envelope,
    /// never a larger override or the default ATP megabyte-scale record bound.
    pub fn spawn(
        cx: Cx,
        input: &InputSession,
        directory: DropDirectory,
        permission: Permission,
        policy: Policy,
        frame_bytes: usize,
    ) -> Result<(Self, Task), Error> {
        Self::spawn_with_authority(
            cx,
            crate::session::Authority::from_input(input),
            directory,
            permission,
            policy,
            frame_bytes,
        )
    }
    pub fn spawn_with_authority(
        cx: Cx,
        authority: crate::session::Authority,
        directory: DropDirectory,
        permission: Permission,
        policy: Policy,
        frame_bytes: usize,
    ) -> Result<(Self, Task), Error> {
        if !(MAX_REPLY_BYTES..=MAX_FRAME_BYTES).contains(&frame_bytes) {
            return Err(Error::Limits);
        }
        // Create the worker here: accepting an arbitrary mailbox could inherit
        // an old content-id-only transfer and mislabel its receipt as graph proof.
        let (mailbox, task) =
            worker::spawn_with_authority(cx, authority, directory, permission, policy)
                .map_err(Error::Worker)?;
        Ok((
            Self {
                mailbox,
                frame_bytes,
                active: None,
                pending: None,
                profile: fr_wire::files::ATP_PORTABLE_FULL,
                sizes: Vec::new(),
                offsets: Vec::new(),
            },
            task,
        ))
    }
    pub fn binding(&self) -> Binding {
        self.mailbox.binding()
    }
    pub fn is_closed(&self) -> bool {
        self.mailbox.is_closed()
    }
    pub fn has_pending(&self) -> bool {
        self.pending.is_some()
    }
    pub fn stop(&self) {
        self.mailbox.stop();
    }

    /// One already-delimited ATP frame and the immutable outer file binding.
    /// Success names local QUEUE admission only. Poll the actual outcome before
    /// accepting more data; never equate receipt of `ObjectComplete` with commit.
    pub fn push(&mut self, binding: Binding, id: u64, bytes: &[u8]) -> Result<u64, Error> {
        self.push_profile(binding, id, bytes, fr_wire::files::ATP_PORTABLE_FULL)
    }
    /// Select a supported full-object profile explicitly. Profile 1 never
    /// silently accepts a directory, and a profile change mid-object refuses.
    pub fn push_profile(
        &mut self,
        binding: Binding,
        id: u64,
        bytes: &[u8],
        profile: u16,
    ) -> Result<u64, Error> {
        self.push_enveloped(binding, id, bytes, None, profile)
    }
    pub fn profile(&self) -> u16 {
        self.profile
    }
    pub(crate) fn push_enveloped(
        &mut self,
        binding: Binding,
        id: u64,
        bytes: &[u8],
        offer: Option<bool>,
        profile: u16,
    ) -> Result<u64, Error> {
        self.check_binding(binding)?;
        if id == 0 {
            return Err(Error::WrongTransfer);
        }
        if self.pending.is_some() {
            return Err(Error::Busy);
        }
        if self.is_closed() {
            return Err(Error::Closed);
        }
        let result = self.push_inner(id, bytes, offer, profile);
        if let Err(error) = &result
            && !matches!(
                error,
                Error::Busy | Error::WrongTransfer | Error::Worker(worker::Error::Busy)
            )
        {
            self.stop();
        }
        result
    }
    fn push_inner(
        &mut self,
        id: u64,
        bytes: &[u8],
        offer: Option<bool>,
        profile: u16,
    ) -> Result<u64, Error> {
        let frame = decode_frame(bytes, self.frame_bytes)?;
        if frame.frame_type() != FrameType::ObjectManifest && profile != self.profile {
            return Err(Error::UnsupportedProfile);
        }
        validate_envelope(frame.frame_type(), offer)?;
        let (sequence, operation, request_root) = match frame.frame_type() {
            FrameType::ObjectManifest => {
                if self.active.is_some() {
                    return Err(Error::Order);
                }
                let manifest = portable_manifest(&frame, profile)?;
                let expected = if manifest.is_directory {
                    Expected::Directory(
                        DirectoryManifest::new(
                            manifest.root_name.clone(),
                            manifest.total_bytes,
                            manifest.entries.clone(),
                            manifest.merkle_root_hex.clone(),
                        )
                        .map_err(|_| Error::Manifest)?,
                    )
                } else {
                    Expected::Manifest {
                        sha256_hex: manifest.entries[0].sha256_hex.clone(),
                        merkle_root_hex: manifest.merkle_root_hex.clone(),
                    }
                };
                let sequence = self
                    .mailbox
                    .begin_verified(VerifiedOffer {
                        binding: self.binding(),
                        id,
                        name: &manifest.root_name,
                        size: manifest.total_bytes,
                        expected,
                    })
                    .map_err(Error::Worker)?;
                self.profile = profile;
                self.sizes = manifest.entries.iter().map(|e| e.size).collect();
                self.offsets = vec![0; self.sizes.len()];
                (sequence, Operation::Begin, Some(manifest.merkle_root_hex))
            }
            FrameType::ObjectData => {
                self.active_for(id)?;
                // Exact pinned ATP TCP/QUIC full-object data payload: big-endian
                // u32 entry index, big-endian u64 offset, then raw object bytes.
                let header = frame.payload.get(..12).ok_or(Error::Frame)?;
                let index = u32::from_be_bytes(header[..4].try_into().map_err(|_| Error::Frame)?);
                let offset = u64::from_be_bytes(header[4..].try_into().map_err(|_| Error::Frame)?);
                let payload = &frame.payload[12..];
                let end = offset
                    .checked_add(payload.len() as u64)
                    .ok_or(Error::ResourceLimit)?;
                if self.offsets.get(index as usize).copied() != Some(offset) {
                    return Err(Error::Order);
                }
                if payload.is_empty()
                    || payload.len() > MAX_CHUNK_BYTES
                    || self
                        .sizes
                        .get(index as usize)
                        .is_none_or(|size| end > *size)
                {
                    return Err(Error::ResourceLimit);
                }
                let seq = self
                    .mailbox
                    .write_entry(id, index, offset, payload)
                    .map_err(Error::Worker)?;
                self.offsets[index as usize] = end;
                (seq, Operation::Write, None)
            }
            FrameType::ObjectComplete => {
                let progress = self.active_for(id)?;
                if !frame.payload.is_empty() || progress.staged_bytes != progress.total_bytes {
                    return Err(Error::Order);
                }
                (
                    self.mailbox.complete(id).map_err(Error::Worker)?,
                    Operation::Complete,
                    None,
                )
            }
            _ => return Err(Error::UnsupportedProfile),
        };
        self.pending = Some(Pending {
            id,
            sequence,
            operation,
            request_root,
        });
        Ok(sequence)
    }
    /// `FileCancel` is outside ATP's data queue. An old transfer/binding cannot
    /// cancel a newer transfer. Cancellation terminates this file attachment and
    /// never keyboard/mouse control. A racing publication result remains pollable.
    pub fn cancel(&self, binding: Binding, id: u64) -> Result<(), Error> {
        self.check_binding(binding)?;
        let current = self
            .pending
            .as_ref()
            .map(|p| p.id)
            .or_else(|| self.active.map(|p| p.id));
        if current != Some(id) {
            return Err(Error::WrongTransfer);
        }
        self.stop();
        Ok(())
    }
    /// May be called after cancellation or worker completion. Once returned, an
    /// Event belongs to the caller; failed proof encoding never destroys it.
    pub fn poll(&mut self) -> Result<Option<Event>, Error> {
        if self.pending.is_none() {
            return Ok(None);
        }
        let Some(receipt) = self.mailbox.take_receipt().map_err(Error::Worker)? else {
            return Ok(None);
        };
        let pending = self.pending.take().ok_or(Error::Order)?;
        let consistent = receipt.sequence == pending.sequence
            && match (pending.operation, receipt.result) {
                (_, Err(_)) => true,
                (Operation::Begin, Ok(Completion::Begun(p)))
                | (Operation::Write, Ok(Completion::Written(p))) => p.id == pending.id,
                (Operation::Complete, Ok(Completion::Published(p))) => p.id == pending.id,
                _ => false,
            };
        let outcome = if consistent {
            receipt.result.map_err(|error| {
                if error == crate::session::Error::UnknownEffect {
                    worker::Error::UnknownEffect
                } else {
                    worker::Error::Session(error)
                }
            })
        } else {
            Err(worker::Error::UnknownEffect)
        };
        match outcome {
            Ok(Completion::Begun(p) | Completion::Written(p)) => self.active = Some(p),
            Ok(Completion::Published(_) | Completion::Cancelled) => self.active = None,
            Err(_) => {
                self.stop();
                self.active = None;
            }
        }
        Ok(Some(Event {
            id: pending.id,
            outcome,
            request_root: pending.request_root,
            frame_bytes: self.frame_bytes,
            profile: self.profile,
            files: u32::try_from(self.sizes.len()).expect("bounded manifest entries"),
        }))
    }
    fn active_for(&self, id: u64) -> Result<Progress, Error> {
        self.active
            .filter(|p| p.id == id)
            .ok_or(Error::WrongTransfer)
    }
    fn check_binding(&self, binding: Binding) -> Result<(), Error> {
        if binding == self.binding() {
            Ok(())
        } else {
            Err(Error::WrongBinding)
        }
    }
}

fn decode_frame(bytes: &[u8], maximum: usize) -> Result<Decoded, Error> {
    if bytes.len() > maximum {
        return Err(Error::ResourceLimit);
    }
    let mut buffer = BytesMut::from(bytes);
    let mut frame = AtpFrameCodec::with_max_frame_size(maximum as u64)
        .decode(&mut buffer)
        .map_err(|_| Error::Frame)?
        .ok_or(Error::Frame)?;
    if !buffer.is_empty()
        || frame.version() != ProtocolVersion::CURRENT
        || !frame.header.extensions.is_empty()
    {
        frame.payload.fill(0);
        return Err(Error::Frame);
    }
    Ok(Decoded(frame))
}
struct Decoded(Frame);
impl std::ops::Deref for Decoded {
    type Target = Frame;
    fn deref(&self) -> &Frame {
        &self.0
    }
}
impl Drop for Decoded {
    fn drop(&mut self) {
        self.0.payload.fill(0);
    }
}
fn lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|v| v.is_ascii_digit() || (b'a'..=b'f').contains(&v))
}
fn portable_manifest(frame: &Frame, profile: u16) -> Result<TransferManifest, Error> {
    if frame.payload.len() > MAX_MANIFEST_BYTES {
        return Err(Error::ResourceLimit);
    }
    let manifest: TransferManifest =
        serde_json::from_slice(&frame.payload).map_err(|_| Error::Manifest)?;
    if manifest.metadata_root_hex.is_some()
        || manifest.directory_metadata.is_some()
        || manifest.delta_manifest.is_some()
    {
        return Err(Error::UnsupportedProfile);
    }
    if !lower_hex(&manifest.transfer_id, 32) || !lower_hex(&manifest.merkle_root_hex, 64) {
        return Err(Error::Manifest);
    }
    match profile {
        fr_wire::files::ATP_PORTABLE_DIRECTORY_FULL if manifest.is_directory => {
            DirectoryManifest::new(
                manifest.root_name.clone(),
                manifest.total_bytes,
                manifest.entries.clone(),
                manifest.merkle_root_hex.clone(),
            )
            .map_err(|_| Error::Manifest)?;
            return Ok(manifest);
        }
        fr_wire::files::ATP_PORTABLE_FULL
            if !manifest.is_directory && manifest.entries.len() == 1 =>
        {
            if frame.payload.len() > 4096 {
                return Err(Error::ResourceLimit);
            }
        }
        _ => return Err(Error::UnsupportedProfile),
    }
    let entry = &manifest.entries[0];
    if entry.metadata.is_some() || !entry.members.is_empty() {
        return Err(Error::UnsupportedProfile);
    }
    if entry.index != 0
        || manifest.total_bytes != entry.size
        || manifest.root_name != entry.rel_path
        || entry.rel_path.len() > 255
        || entry.rel_path.starts_with(".fr-part-")
        || validate_portable_path_component(&entry.rel_path).is_err()
        || !lower_hex(&manifest.transfer_id, 32)
        || !lower_hex(&manifest.merkle_root_hex, 64)
        || !lower_hex(&entry.sha256_hex, 64)
    {
        return Err(Error::Manifest);
    }
    Ok(manifest)
}

fn validate_envelope(kind: FrameType, offer: Option<bool>) -> Result<(), Error> {
    let valid = match offer {
        Some(true) => kind == FrameType::ObjectManifest,
        Some(false) => matches!(kind, FrameType::ObjectData | FrameType::ObjectComplete),
        None => true,
    };
    if valid { Ok(()) } else { Err(Error::Order) }
}

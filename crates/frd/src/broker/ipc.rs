//! Local IPC protocol and role-capability boundary enforcement (plan sections 5.1, 5.2, 5.3, 19.2).
//!
//! `FrankenRemote` uses parent-created socket pairs (`UnixStream::pair()`) or
//! filesystem-protected endpoints (mode 0700). Peer credentials (`SO_PEERCRED`)
//! are validated on connection.
//!
//! CRITICAL SECURITY INVARIANTS:
//! - Credentials alone identify a user, NOT a trusted helper.
//! - Messages are tagged with sender role, process generation, and UID.
//! - Role capabilities are checked on every message: a worker attempting to exercise
//!   an approval, input-lease, cert-key, or Tailscale control operation is immediately
//!   rejected with a typed `ForbiddenCapability` error.
//! - Stale process generations are rejected (`StaleProcessGeneration`).
//! - UID mismatch between socket peer credentials and declared header is rejected (`ForgedCredentials`).

use super::process_role::{ProcessGeneration, ProcessRole, RoleCapability};
use core::fmt;

/// Magic bytes identifying `FrankenRemote` IPC frames ("FRIP").
pub const IPC_MAGIC: [u8; 4] = *b"FRIP";

/// Protocol version 1.
pub const IPC_VERSION: u8 = 1;

/// Fixed IPC header size in bytes.
pub const HEADER_BYTES: usize = 32;

/// Maximum IPC message payload size in bytes (64 KiB).
pub const MAX_IPC_PAYLOAD_BYTES: usize = 65536;

/// OS socket peer credentials (from `SO_PEERCRED` / `asupersync::net::unix::UCred`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SocketCredentials {
    pub pid: Option<i32>,
    pub uid: u32,
    pub gid: u32,
}

impl SocketCredentials {
    pub const fn new(pid: Option<i32>, uid: u32, gid: u32) -> Self {
        Self { pid, uid, gid }
    }
}

#[cfg(target_os = "linux")]
impl From<asupersync::net::unix::UCred> for SocketCredentials {
    fn from(ucred: asupersync::net::unix::UCred) -> Self {
        Self {
            pid: ucred.pid,
            uid: ucred.uid,
            gid: ucred.gid,
        }
    }
}

/// Message kinds exchanged over local IPC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u16)]
pub enum IpcMessageKind {
    /// Liveness probe.
    Ping = 1,
    /// Liveness acknowledgement.
    Pong = 2,
    /// Process announces its role and initial generation.
    RegisterRole = 3,
    /// Broker asks session agent for interactive user consent.
    ConsentPrompt = 4,
    /// Session agent returns user consent decision.
    ConsentDecision = 5,
    /// Session agent requests an input lease grant.
    InputLeaseRequest = 6,
    /// Broker notifies session agent of input lease grant.
    InputLeaseGrant = 7,
    /// Broker or agent notifies that input lease has expired or been revoked.
    InputLeaseRevoke = 8,
    /// Broker requests child process spawn.
    SpawnWorker = 9,
    /// Media worker signals capture and encoder pipeline ready.
    WorkerReady = 10,
    /// Broker requests media worker stop and drain.
    WorkerStop = 11,
    /// Query monitor and display geometry.
    QueryDisplays = 12,
    /// Return display layout and geometries.
    DisplayList = 13,
    /// Revoke all active sessions and release keys/buttons.
    RevokeAll = 14,
    /// Broker internal TLS certificate query (forbidden to workers).
    TailscaleCertRequest = 15,
    /// Broker internal Tailscale status query (forbidden to workers).
    TailscaleStatusQuery = 16,
}

impl IpcMessageKind {
    #[must_use]
    pub const fn as_u16(self) -> u16 {
        self as u16
    }

    #[must_use]
    pub const fn from_u16(raw: u16) -> Option<Self> {
        match raw {
            1 => Some(Self::Ping),
            2 => Some(Self::Pong),
            3 => Some(Self::RegisterRole),
            4 => Some(Self::ConsentPrompt),
            5 => Some(Self::ConsentDecision),
            6 => Some(Self::InputLeaseRequest),
            7 => Some(Self::InputLeaseGrant),
            8 => Some(Self::InputLeaseRevoke),
            9 => Some(Self::SpawnWorker),
            10 => Some(Self::WorkerReady),
            11 => Some(Self::WorkerStop),
            12 => Some(Self::QueryDisplays),
            13 => Some(Self::DisplayList),
            14 => Some(Self::RevokeAll),
            15 => Some(Self::TailscaleCertRequest),
            16 => Some(Self::TailscaleStatusQuery),
            _ => None,
        }
    }

    /// Required capability to send or handle this message kind.
    #[must_use]
    pub const fn required_capability(self) -> Option<RoleCapability> {
        match self {
            Self::Ping | Self::Pong | Self::RegisterRole => None,
            Self::ConsentPrompt | Self::ConsentDecision | Self::RevokeAll => {
                Some(RoleCapability::ApprovalEndpoint)
            }
            Self::InputLeaseRequest | Self::InputLeaseGrant | Self::InputLeaseRevoke => {
                Some(RoleCapability::InputLease)
            }
            Self::SpawnWorker | Self::WorkerStop => Some(RoleCapability::AuditLog),
            Self::WorkerReady => Some(RoleCapability::MediaCapture),
            Self::QueryDisplays | Self::DisplayList => Some(RoleCapability::DisplayQuery),
            Self::TailscaleCertRequest => Some(RoleCapability::CertificateKeys),
            Self::TailscaleStatusQuery => Some(RoleCapability::TailscaleControlSocket),
        }
    }
}

/// Fixed 32-byte header for local IPC messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IpcHeader {
    /// Magic identifier (must match `IPC_MAGIC`).
    pub magic: [u8; 4],
    /// Protocol version (must match `IPC_VERSION`).
    pub version: u8,
    /// Declared role of the sending process.
    pub sender_role: ProcessRole,
    /// Reserved for future flags/alignment.
    pub reserved: u16,
    /// Monotonic process generation of sender.
    pub process_generation: ProcessGeneration,
    /// UID of the sending process.
    pub sender_uid: u32,
    /// PID of the sending process.
    pub sender_pid: u32,
    /// Message kind.
    pub msg_kind: IpcMessageKind,
    /// Payload length in bytes.
    pub payload_len: u32,
}

impl IpcHeader {
    /// Create a new outgoing IPC header.
    #[must_use]
    #[allow(clippy::similar_names)]
    pub const fn new(
        sender_role: ProcessRole,
        process_generation: ProcessGeneration,
        sender_uid: u32,
        sender_pid: u32,
        msg_kind: IpcMessageKind,
        payload_len: u32,
    ) -> Self {
        Self {
            magic: IPC_MAGIC,
            version: IPC_VERSION,
            sender_role,
            reserved: 0,
            process_generation,
            sender_uid,
            sender_pid,
            msg_kind,
            payload_len,
        }
    }

    /// Encode header to fixed 32-byte array.
    #[must_use]
    pub fn encode(&self) -> [u8; HEADER_BYTES] {
        let mut buf = [0u8; HEADER_BYTES];
        buf[0..4].copy_from_slice(&self.magic);
        buf[4] = self.version;
        buf[5] = self.sender_role.as_u8();
        buf[6..8].copy_from_slice(&self.reserved.to_be_bytes());
        buf[8..16].copy_from_slice(&self.process_generation.as_raw().to_be_bytes());
        buf[16..20].copy_from_slice(&self.sender_uid.to_be_bytes());
        buf[20..24].copy_from_slice(&self.sender_pid.to_be_bytes());
        buf[24..26].copy_from_slice(&self.msg_kind.as_u16().to_be_bytes());
        buf[26..30].copy_from_slice(&self.payload_len.to_be_bytes());
        buf[30..32].copy_from_slice(&[0u8, 0u8]); // padding to 32 bytes
        buf
    }

    /// Decode header from 32-byte slice.
    #[allow(clippy::similar_names)]
    pub fn decode(bytes: &[u8]) -> Result<Self, IpcError> {
        if bytes.len() < HEADER_BYTES {
            return Err(IpcError::TruncatedHeader);
        }
        let mut magic = [0u8; 4];
        magic.copy_from_slice(&bytes[0..4]);
        if magic != IPC_MAGIC {
            return Err(IpcError::MalformedMagic);
        }
        let version = bytes[4];
        if version != IPC_VERSION {
            return Err(IpcError::UnsupportedVersion(version));
        }
        let sender_role = ProcessRole::from_u8(bytes[5]).ok_or(IpcError::UnknownRole(bytes[5]))?;
        let reserved = u16::from_be_bytes([bytes[6], bytes[7]]);
        let generation_raw = u64::from_be_bytes([
            bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
        ]);
        let process_generation = ProcessGeneration::from_raw(generation_raw);
        let sender_uid = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
        let sender_pid = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
        let kind_raw = u16::from_be_bytes([bytes[24], bytes[25]]);
        let msg_kind =
            IpcMessageKind::from_u16(kind_raw).ok_or(IpcError::UnknownMessageKind(kind_raw))?;
        let payload_len = u32::from_be_bytes([bytes[26], bytes[27], bytes[28], bytes[29]]);
        if (payload_len as usize) > MAX_IPC_PAYLOAD_BYTES {
            return Err(IpcError::PayloadTooLarge {
                length: payload_len as usize,
                maximum: MAX_IPC_PAYLOAD_BYTES,
            });
        }
        Ok(Self {
            magic,
            version,
            sender_role,
            reserved,
            process_generation,
            sender_uid,
            sender_pid,
            msg_kind,
            payload_len,
        })
    }
}

/// Typed errors in local IPC parsing and security boundary enforcement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpcError {
    /// Magic bytes mismatch.
    MalformedMagic,
    /// Protocol version not supported.
    UnsupportedVersion(u8),
    /// Role identifier is unrecognized.
    UnknownRole(u8),
    /// Message kind identifier is unrecognized.
    UnknownMessageKind(u16),
    /// Payload exceeds absolute 64 KiB ceiling.
    PayloadTooLarge { length: usize, maximum: usize },
    /// Header was shorter than 32 bytes.
    TruncatedHeader,
    /// Declared payload length not fully received.
    TruncatedPayload,
    /// Sender role did not match expected connection role.
    WrongRole {
        expected: ProcessRole,
        actual: ProcessRole,
    },
    /// Sender process generation is older than active generation.
    StaleProcessGeneration { expected: u64, actual: u64 },
    /// Sending process UID does not match authorized user.
    UnauthorizedUser { expected_uid: u32, actual_uid: u32 },
    /// Sending role attempted an unauthorized operation.
    ForbiddenCapability {
        role: ProcessRole,
        capability: RoleCapability,
    },
    /// Declared UID in message header contradicts kernel socket credentials.
    ForgedCredentials { declared_uid: u32, socket_uid: u32 },
    /// Local channel closed.
    ChannelClosed,
}

impl fmt::Display for IpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MalformedMagic => f.write_str("malformed IPC magic bytes"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported IPC version {v}"),
            Self::UnknownRole(r) => write!(f, "unknown role ID {r}"),
            Self::UnknownMessageKind(k) => write!(f, "unknown message kind {k}"),
            Self::PayloadTooLarge { length, maximum } => {
                write!(f, "payload too large: {length} > {maximum}")
            }
            Self::TruncatedHeader => f.write_str("truncated IPC header"),
            Self::TruncatedPayload => f.write_str("truncated IPC payload"),
            Self::WrongRole { expected, actual } => {
                write!(f, "wrong role: expected {expected}, actual {actual}")
            }
            Self::StaleProcessGeneration { expected, actual } => write!(
                f,
                "stale process generation: active {expected}, got {actual}"
            ),
            Self::UnauthorizedUser {
                expected_uid,
                actual_uid,
            } => write!(
                f,
                "unauthorized user: expected UID {expected_uid}, got {actual_uid}"
            ),
            Self::ForbiddenCapability { role, capability } => write!(
                f,
                "role {role} forbidden from exercising capability {capability}"
            ),
            Self::ForgedCredentials {
                declared_uid,
                socket_uid,
            } => write!(
                f,
                "forged credentials: header declared UID {declared_uid} but socket has UID {socket_uid}"
            ),
            Self::ChannelClosed => f.write_str("IPC channel closed"),
        }
    }
}

impl std::error::Error for IpcError {}

/// Verify an incoming message against kernel socket credentials, expected role,
/// active process generation, and role capability boundaries.
pub fn verify_incoming_message(
    header: &IpcHeader,
    socket_peer_cred: Option<&SocketCredentials>,
    expected_role: ProcessRole,
    expected_uid: u32,
    active_generation: ProcessGeneration,
) -> Result<(), IpcError> {
    // 1. Kernel socket peer credentials check (prevent UID spoofing).
    if let Some(cred) = socket_peer_cred
        && cred.uid != header.sender_uid
    {
        return Err(IpcError::ForgedCredentials {
            declared_uid: header.sender_uid,
            socket_uid: cred.uid,
        });
    }

    // 2. User authorization check (credentials alone identify a user, not a trusted helper).
    if header.sender_uid != expected_uid {
        return Err(IpcError::UnauthorizedUser {
            expected_uid,
            actual_uid: header.sender_uid,
        });
    }

    // 3. Process role check (ensure sender matches expected role on this channel).
    if header.sender_role != expected_role {
        return Err(IpcError::WrongRole {
            expected: expected_role,
            actual: header.sender_role,
        });
    }

    // 4. Process generation check (fence stale children and restart races).
    if header.process_generation.as_raw() < active_generation.as_raw() {
        return Err(IpcError::StaleProcessGeneration {
            expected: active_generation.as_raw(),
            actual: header.process_generation.as_raw(),
        });
    }

    // 5. Capability boundary enforcement (non-negotiable role isolation).
    if let Some(required_cap) = header.msg_kind.required_capability()
        && !header.sender_role.allows_capability(required_cap)
    {
        return Err(IpcError::ForbiddenCapability {
            role: header.sender_role,
            capability: required_cap,
        });
    }

    Ok(())
}

/// In-memory complete IPC message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IpcMessage {
    pub header: IpcHeader,
    pub payload: Vec<u8>,
}

impl IpcMessage {
    pub fn new(header: IpcHeader, payload: Vec<u8>) -> Result<Self, IpcError> {
        if payload.len() > MAX_IPC_PAYLOAD_BYTES {
            return Err(IpcError::PayloadTooLarge {
                length: payload.len(),
                maximum: MAX_IPC_PAYLOAD_BYTES,
            });
        }
        let mut h = header;
        h.payload_len = u32::try_from(payload.len()).unwrap_or(u32::MAX);
        Ok(Self { header: h, payload })
    }

    /// Encode header + payload into a contiguous byte buffer.
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(HEADER_BYTES + self.payload.len());
        buf.extend_from_slice(&self.header.encode());
        buf.extend_from_slice(&self.payload);
        buf
    }

    /// Decode header + payload from contiguous byte slice.
    pub fn decode(bytes: &[u8]) -> Result<Self, IpcError> {
        if bytes.len() < HEADER_BYTES {
            return Err(IpcError::TruncatedHeader);
        }
        let header = IpcHeader::decode(&bytes[0..HEADER_BYTES])?;
        let payload_len = header.payload_len as usize;
        if bytes.len() < HEADER_BYTES + payload_len {
            return Err(IpcError::TruncatedPayload);
        }
        let payload = bytes[HEADER_BYTES..HEADER_BYTES + payload_len].to_vec();
        Ok(Self { header, payload })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_UID: u32 = 1000;
    const TEST_PID: u32 = 4242;
    const TEST_PID_I32: i32 = 4242;

    fn make_header(
        role: ProcessRole,
        generation: u64,
        uid: u32,
        kind: IpcMessageKind,
    ) -> IpcHeader {
        IpcHeader::new(
            role,
            ProcessGeneration::from_raw(generation),
            uid,
            TEST_PID,
            kind,
            0,
        )
    }

    #[test]
    fn header_roundtrip_valid() {
        let header = make_header(
            ProcessRole::SessionAgent,
            5,
            TEST_UID,
            IpcMessageKind::InputLeaseRequest,
        );
        let encoded = header.encode();
        assert_eq!(encoded.len(), HEADER_BYTES);
        let decoded = IpcHeader::decode(&encoded).expect("decode succeeds");
        assert_eq!(header, decoded);
    }

    #[test]
    fn verify_valid_message_succeeds() {
        let header = make_header(
            ProcessRole::SessionAgent,
            2,
            TEST_UID,
            IpcMessageKind::InputLeaseRequest,
        );
        let cred = SocketCredentials::new(Some(TEST_PID_I32), TEST_UID, TEST_UID);
        let res = verify_incoming_message(
            &header,
            Some(&cred),
            ProcessRole::SessionAgent,
            TEST_UID,
            ProcessGeneration::from_raw(2),
        );
        assert!(res.is_ok());
    }

    #[test]
    fn ipc_forgery_wrong_role_is_refused() {
        // Media worker attempts to impersonate SessionAgent on an agent channel.
        let header = make_header(
            ProcessRole::MediaWorker,
            1,
            TEST_UID,
            IpcMessageKind::ConsentDecision,
        );
        let cred = SocketCredentials::new(Some(TEST_PID_I32), TEST_UID, TEST_UID);
        let res = verify_incoming_message(
            &header,
            Some(&cred),
            ProcessRole::SessionAgent, // Expected SessionAgent!
            TEST_UID,
            ProcessGeneration::INITIAL,
        );
        assert_eq!(
            res,
            Err(IpcError::WrongRole {
                expected: ProcessRole::SessionAgent,
                actual: ProcessRole::MediaWorker,
            })
        );
    }

    #[test]
    fn ipc_forgery_stale_generation_is_refused() {
        // Child sends a message with old generation after restart.
        let header = make_header(
            ProcessRole::SessionAgent,
            1, // Stale generation 1
            TEST_UID,
            IpcMessageKind::InputLeaseRequest,
        );
        let cred = SocketCredentials::new(Some(TEST_PID_I32), TEST_UID, TEST_UID);
        let res = verify_incoming_message(
            &header,
            Some(&cred),
            ProcessRole::SessionAgent,
            TEST_UID,
            ProcessGeneration::from_raw(2), // Active generation is 2!
        );
        assert_eq!(
            res,
            Err(IpcError::StaleProcessGeneration {
                expected: 2,
                actual: 1,
            })
        );
    }

    #[test]
    fn ipc_forgery_unauthorized_user_is_refused() {
        let other_uid = 1001;
        let header = make_header(
            ProcessRole::SessionAgent,
            1,
            other_uid,
            IpcMessageKind::InputLeaseRequest,
        );
        let cred = SocketCredentials::new(Some(TEST_PID_I32), other_uid, other_uid);
        let res = verify_incoming_message(
            &header,
            Some(&cred),
            ProcessRole::SessionAgent,
            TEST_UID, // Expected 1000, got 1001
            ProcessGeneration::INITIAL,
        );
        assert_eq!(
            res,
            Err(IpcError::UnauthorizedUser {
                expected_uid: TEST_UID,
                actual_uid: other_uid,
            })
        );
    }

    #[test]
    fn ipc_forgery_forged_credentials_is_refused() {
        // Message claims UID 1000, but kernel socket credentials show UID 1002.
        let header = make_header(
            ProcessRole::SessionAgent,
            1,
            1000,
            IpcMessageKind::InputLeaseRequest,
        );
        let cred = SocketCredentials::new(Some(TEST_PID_I32), 1002, 1002);
        let res = verify_incoming_message(
            &header,
            Some(&cred),
            ProcessRole::SessionAgent,
            1000,
            ProcessGeneration::INITIAL,
        );
        assert_eq!(
            res,
            Err(IpcError::ForgedCredentials {
                declared_uid: 1000,
                socket_uid: 1002,
            })
        );
    }

    #[test]
    fn ipc_forgery_media_worker_requesting_input_lease_is_forbidden() {
        // Worker attempts to send InputLeaseRequest on worker channel.
        let header = make_header(
            ProcessRole::MediaWorker,
            1,
            TEST_UID,
            IpcMessageKind::InputLeaseRequest, // FORBIDDEN TO WORKER!
        );
        let cred = SocketCredentials::new(Some(TEST_PID_I32), TEST_UID, TEST_UID);
        let res = verify_incoming_message(
            &header,
            Some(&cred),
            ProcessRole::MediaWorker,
            TEST_UID,
            ProcessGeneration::INITIAL,
        );
        assert_eq!(
            res,
            Err(IpcError::ForbiddenCapability {
                role: ProcessRole::MediaWorker,
                capability: RoleCapability::InputLease,
            })
        );
    }

    #[test]
    fn ipc_forgery_media_worker_requesting_tailscale_certs_is_forbidden() {
        // Worker attempts to query TLS certificates or Tailscale socket.
        let header = make_header(
            ProcessRole::MediaWorker,
            1,
            TEST_UID,
            IpcMessageKind::TailscaleCertRequest, // FORBIDDEN TO WORKER!
        );
        let cred = SocketCredentials::new(Some(TEST_PID_I32), TEST_UID, TEST_UID);
        let res = verify_incoming_message(
            &header,
            Some(&cred),
            ProcessRole::MediaWorker,
            TEST_UID,
            ProcessGeneration::INITIAL,
        );
        assert_eq!(
            res,
            Err(IpcError::ForbiddenCapability {
                role: ProcessRole::MediaWorker,
                capability: RoleCapability::CertificateKeys,
            })
        );
    }

    #[test]
    fn ipc_forgery_media_worker_sending_approval_decision_is_forbidden() {
        // Worker attempts to forge user approval.
        let header = make_header(
            ProcessRole::MediaWorker,
            1,
            TEST_UID,
            IpcMessageKind::ConsentDecision, // FORBIDDEN TO WORKER!
        );
        let cred = SocketCredentials::new(Some(TEST_PID_I32), TEST_UID, TEST_UID);
        let res = verify_incoming_message(
            &header,
            Some(&cred),
            ProcessRole::MediaWorker,
            TEST_UID,
            ProcessGeneration::INITIAL,
        );
        assert_eq!(
            res,
            Err(IpcError::ForbiddenCapability {
                role: ProcessRole::MediaWorker,
                capability: RoleCapability::ApprovalEndpoint,
            })
        );
    }

    #[test]
    fn message_oversize_is_rejected() {
        let header = make_header(ProcessRole::SessionAgent, 1, TEST_UID, IpcMessageKind::Ping);
        let big_payload = vec![0u8; MAX_IPC_PAYLOAD_BYTES + 1];
        let res = IpcMessage::new(header, big_payload);
        assert_eq!(
            res,
            Err(IpcError::PayloadTooLarge {
                length: MAX_IPC_PAYLOAD_BYTES + 1,
                maximum: MAX_IPC_PAYLOAD_BYTES,
            })
        );
    }
}

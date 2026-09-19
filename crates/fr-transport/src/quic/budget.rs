//! Inner MTU payload budget arithmetic and overhead breakdown.
//!
//! Tailscale documents an inner path MTU of 1280 bytes. To avoid IP fragmentation
//! (which QUIC datagrams do not survive and do not fragment for applications),
//! datagram payloads must fit within the 1280-byte MTU after subtracting:
//! 1. Outer IP header (20 bytes IPv4, 40 bytes IPv6).
//! 2. UDP header (8 bytes).
//! 3. QUIC 1-RTT short-header packet protection (1 byte header + max 20 byte DCID
//!    + max 4 byte packet number + 16 byte AEAD authentication tag = 41 bytes).
//! 4. QUIC DATAGRAM frame overhead (RFC 9221: 1 byte type + varint length up to 2 bytes = 3 bytes).
//! 5. FRD0 record header (`fr_wire::HEADER_BYTES` = 24 bytes).
//!
//! Under IPv4 (MTU 1280):
//!   Max UDP payload: 1280 - 20 - 8 = 1252 bytes.
//!   Max QUIC payload: 1252 - 41 = 1211 bytes.
//!   Max DATAGRAM frame payload (= max wire record): 1211 - 3 = 1208 bytes.
//!   Max application payload: 1208 - 24 = 1184 bytes.
//!
//! Under IPv6 (MTU 1280):
//!   Max UDP payload: 1280 - 40 - 8 = 1232 bytes.
//!   Max QUIC payload: 1232 - 41 = 1191 bytes.
//!   Max DATAGRAM frame payload (= max wire record): 1191 - 3 = 1188 bytes.
//!   Max application payload: 1188 - 24 = 1164 bytes.

use core::fmt;
use fr_wire::HEADER_BYTES as RECORD_HEADER_BYTES;

/// Standard Tailscale inner path MTU (RFC 8200 minimum IPv6 MTU).
pub const TAILNET_INNER_MTU: usize = 1280;

/// Standard IPv4 header length without options (RFC 791).
pub const IPV4_HEADER_BYTES: usize = 20;

/// Standard IPv6 fixed header length (RFC 8200).
pub const IPV6_HEADER_BYTES: usize = 40;

/// Standard UDP header length (RFC 768).
pub const UDP_HEADER_BYTES: usize = 8;

/// Maximal QUIC 1-RTT short-header packet protection overhead:
/// - 1 byte header flags (Header Form = 0, Fixed Bit = 1, Spin, Key Phase, PN length)
/// - 20 bytes Destination Connection ID (RFC 9000 max CID length)
/// - 4 bytes Packet Number (maximal 32-bit packet number)
/// - 16 bytes AEAD authentication tag (AES-GCM / ChaCha20-Poly1305)
pub const QUIC_SHORT_HEADER_PROTECTION_BYTES: usize = 1 + 20 + 4 + 16; // 41 bytes

/// Maximal QUIC DATAGRAM frame overhead (RFC 9221):
/// - 1 byte frame type (0x30 without length or 0x31 with length)
/// - 2 bytes varint length field (for lengths up to 16,383 bytes)
pub const QUIC_DATAGRAM_FRAME_OVERHEAD_BYTES: usize = 1 + 2; // 3 bytes

/// IP version for path budget calculation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IpVersion {
    V4,
    V6,
}

impl IpVersion {
    /// IP header size in bytes.
    pub const fn header_bytes(self) -> usize {
        match self {
            Self::V4 => IPV4_HEADER_BYTES,
            Self::V6 => IPV6_HEADER_BYTES,
        }
    }
}

/// Typed budget computation or validation error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetError {
    /// Specified MTU is smaller than the required transport and record framing overheads.
    MtuTooSmall { mtu: usize, minimum_required: usize },
    /// Record size exceeds the allowable datagram record budget for this path.
    RecordExceedsBudget { len: usize, max_allowed: usize },
    /// Application payload size exceeds the allowable datagram payload budget.
    PayloadExceedsBudget { len: usize, max_allowed: usize },
}

impl fmt::Display for BudgetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MtuTooSmall {
                mtu,
                minimum_required,
            } => {
                write!(
                    f,
                    "MTU {mtu} too small; minimum required overhead is {minimum_required}"
                )
            }
            Self::RecordExceedsBudget { len, max_allowed } => {
                write!(f, "record size {len} exceeds datagram budget {max_allowed}")
            }
            Self::PayloadExceedsBudget { len, max_allowed } => {
                write!(
                    f,
                    "payload size {len} exceeds application datagram budget {max_allowed}"
                )
            }
        }
    }
}

impl std::error::Error for BudgetError {}

/// Exact budget breakdown for an unfragmented datagram transmission path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketBudget {
    pub ip_version: IpVersion,
    pub path_mtu: usize,
    pub ip_header_bytes: usize,
    pub udp_header_bytes: usize,
    pub quic_protection_bytes: usize,
    pub datagram_frame_overhead: usize,
    pub record_header_bytes: usize,
    pub max_datagram_record: usize,
    pub max_application_payload: usize,
}

impl PacketBudget {
    /// Compute the packet budget for a given IP version and path MTU.
    ///
    /// Refuses with [`BudgetError::MtuTooSmall`] if the MTU cannot accommodate
    /// the outer IP, UDP, QUIC protection, DATAGRAM frame, and FRD0 record headers.
    pub fn compute(ip_version: IpVersion, path_mtu: usize) -> Result<Self, BudgetError> {
        let ip_header_bytes = ip_version.header_bytes();
        let udp_header_bytes = UDP_HEADER_BYTES;
        let quic_protection_bytes = QUIC_SHORT_HEADER_PROTECTION_BYTES;
        let datagram_frame_overhead = QUIC_DATAGRAM_FRAME_OVERHEAD_BYTES;
        let record_header_bytes = RECORD_HEADER_BYTES;

        let total_overhead = ip_header_bytes
            .checked_add(udp_header_bytes)
            .and_then(|v| v.checked_add(quic_protection_bytes))
            .and_then(|v| v.checked_add(datagram_frame_overhead))
            .and_then(|v| v.checked_add(record_header_bytes))
            .ok_or(BudgetError::MtuTooSmall {
                mtu: path_mtu,
                minimum_required: usize::MAX,
            })?;

        if path_mtu < total_overhead {
            return Err(BudgetError::MtuTooSmall {
                mtu: path_mtu,
                minimum_required: total_overhead,
            });
        }

        let non_record_overhead =
            ip_header_bytes + udp_header_bytes + quic_protection_bytes + datagram_frame_overhead;
        let max_datagram_record = path_mtu - non_record_overhead;
        let max_application_payload = max_datagram_record - record_header_bytes;

        Ok(Self {
            ip_version,
            path_mtu,
            ip_header_bytes,
            udp_header_bytes,
            quic_protection_bytes,
            datagram_frame_overhead,
            record_header_bytes,
            max_datagram_record,
            max_application_payload,
        })
    }

    /// Compute the budget for standard Tailscale inner path MTU (1280 bytes).
    pub fn for_tailnet(ip_version: IpVersion) -> Self {
        Self::compute(ip_version, TAILNET_INNER_MTU).expect("tailnet MTU 1280 is always valid")
    }

    /// Conservative budget that guarantees zero IP fragmentation across BOTH
    /// IPv4 and IPv6 paths on a standard Tailscale network (MTU 1280).
    ///
    /// Uses IPv6 overheads as the conservative baseline since IPv6 headers (40B)
    /// are larger than IPv4 headers (20B).
    pub fn conservative_tailnet() -> Self {
        Self::for_tailnet(IpVersion::V6)
    }

    /// Validate that a serialized FRD0 record fits within this budget.
    pub fn validate_record_len(&self, len: usize) -> Result<(), BudgetError> {
        if len > self.max_datagram_record {
            Err(BudgetError::RecordExceedsBudget {
                len,
                max_allowed: self.max_datagram_record,
            })
        } else {
            Ok(())
        }
    }

    /// Validate that an application payload (without FRD0 header) fits within this budget.
    pub fn validate_payload_len(&self, len: usize) -> Result<(), BudgetError> {
        if len > self.max_application_payload {
            Err(BudgetError::PayloadExceedsBudget {
                len,
                max_allowed: self.max_application_payload,
            })
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv4_tailnet_budget_arithmetic_exact() {
        let b = PacketBudget::for_tailnet(IpVersion::V4);
        assert_eq!(b.ip_version, IpVersion::V4);
        assert_eq!(b.path_mtu, 1280);
        assert_eq!(b.ip_header_bytes, 20);
        assert_eq!(b.udp_header_bytes, 8);
        assert_eq!(b.quic_protection_bytes, 41);
        assert_eq!(b.datagram_frame_overhead, 3);
        assert_eq!(b.record_header_bytes, 24);

        // 1280 - (20 + 8 + 41 + 3) = 1280 - 72 = 1208
        assert_eq!(b.max_datagram_record, 1208);
        // 1208 - 24 = 1184
        assert_eq!(b.max_application_payload, 1184);

        // Total wire size of maximum record + all overheads must equal exactly MTU 1280
        let total_packet_size = b.ip_header_bytes
            + b.udp_header_bytes
            + b.quic_protection_bytes
            + b.datagram_frame_overhead
            + b.max_datagram_record;
        assert_eq!(total_packet_size, 1280);
    }

    #[test]
    fn ipv6_tailnet_budget_arithmetic_exact() {
        let b = PacketBudget::for_tailnet(IpVersion::V6);
        assert_eq!(b.ip_version, IpVersion::V6);
        assert_eq!(b.path_mtu, 1280);
        assert_eq!(b.ip_header_bytes, 40);
        assert_eq!(b.udp_header_bytes, 8);
        assert_eq!(b.quic_protection_bytes, 41);
        assert_eq!(b.datagram_frame_overhead, 3);
        assert_eq!(b.record_header_bytes, 24);

        // 1280 - (40 + 8 + 41 + 3) = 1280 - 92 = 1188
        assert_eq!(b.max_datagram_record, 1188);
        // 1188 - 24 = 1164
        assert_eq!(b.max_application_payload, 1164);

        // Total wire size of maximum record + all overheads must equal exactly MTU 1280
        let total_packet_size = b.ip_header_bytes
            + b.udp_header_bytes
            + b.quic_protection_bytes
            + b.datagram_frame_overhead
            + b.max_datagram_record;
        assert_eq!(total_packet_size, 1280);
    }

    #[test]
    fn conservative_tailnet_fits_both_v4_and_v6_without_fragmentation() {
        let conservative = PacketBudget::conservative_tailnet();
        let v4 = PacketBudget::for_tailnet(IpVersion::V4);
        let v6 = PacketBudget::for_tailnet(IpVersion::V6);

        // Conservative uses IPv6 bounds
        assert_eq!(conservative, v6);
        assert!(conservative.max_datagram_record <= v4.max_datagram_record);
        assert!(conservative.max_application_payload <= v4.max_application_payload);

        // A record sized for conservative fits in both
        assert!(
            v4.validate_record_len(conservative.max_datagram_record)
                .is_ok()
        );
        assert!(
            v6.validate_record_len(conservative.max_datagram_record)
                .is_ok()
        );

        // A payload sized for conservative fits in both
        assert!(
            v4.validate_payload_len(conservative.max_application_payload)
                .is_ok()
        );
        assert!(
            v6.validate_payload_len(conservative.max_application_payload)
                .is_ok()
        );
    }

    #[test]
    fn validation_rejects_exceeding_bytes_and_accepts_exact_boundary() {
        let b = PacketBudget::for_tailnet(IpVersion::V6);

        // Exact boundary accepts
        assert!(b.validate_record_len(1188).is_ok());
        assert!(b.validate_payload_len(1164).is_ok());

        // Zero length accepts
        assert!(b.validate_record_len(0).is_ok());
        assert!(b.validate_payload_len(0).is_ok());

        // 1 byte over rejects with typed error
        assert_eq!(
            b.validate_record_len(1189),
            Err(BudgetError::RecordExceedsBudget {
                len: 1189,
                max_allowed: 1188,
            })
        );
        assert_eq!(
            b.validate_payload_len(1165),
            Err(BudgetError::PayloadExceedsBudget {
                len: 1165,
                max_allowed: 1164,
            })
        );
    }

    #[test]
    fn undersized_mtu_is_refused() {
        // Minimum overhead for IPv6 is 92 (IP 40 + UDP 8 + QUIC 41 + DATAGRAM 3) + 24 (FRD0) = 116 bytes
        assert_eq!(
            PacketBudget::compute(IpVersion::V6, 115),
            Err(BudgetError::MtuTooSmall {
                mtu: 115,
                minimum_required: 116,
            })
        );

        // Exactly 116 bytes yields 0 bytes application payload, 24 bytes record
        let exact = PacketBudget::compute(IpVersion::V6, 116).unwrap();
        assert_eq!(exact.max_datagram_record, 24);
        assert_eq!(exact.max_application_payload, 0);
    }
}

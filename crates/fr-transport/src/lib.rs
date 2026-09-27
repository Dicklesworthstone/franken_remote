#![forbid(unsafe_code)]
//! Native QUIC record transport, using Asupersync and no second network stack.
//! An established TLS connection and locally admitted channel bindings are
//! inputs, not credentials manufactured here. The optional cold native accept
//! primitive does not establish tailnet ingress or application admission.
//! Application session negotiation remains a separate boundary. See `QUIC_RECORDS.md` for exact guarantees and qualification scope.
#[cfg(not(target_arch = "wasm32"))]
#[path = "quic/accept.rs"]
pub mod native_accept;
#[cfg(not(target_arch = "wasm32"))]
pub mod quic;

pub mod wss;

#[cfg(not(target_arch = "wasm32"))]
/// Controller closure on the original session-control lane. The first validated
/// terminal report wins: a lease report is not fabricated into session cleanup,
/// and a session report does not prove release of the expected input lease.
/// Per-action receipt ledgers remain with the original input owner. Uncollected
/// receipts remain uncertain; this exchange never retries their effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlCloseOutcome {
    pub exchange: quic::CloseOutcome,
    pub revocation: Option<fr_wire::lease_revoked::Revoked>,
}

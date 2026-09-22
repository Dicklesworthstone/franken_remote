#![forbid(unsafe_code)]
//! Native QUIC record transport, using Asupersync and no second network stack.
//! An established TLS connection and locally admitted channel bindings are
//! inputs, not credentials manufactured here. The optional cold native accept
//! primitive does not establish tailnet ingress or application admission.
//! Application session negotiation remains a separate boundary. See `QUIC_RECORDS.md` for exact guarantees and qualification scope.
#[cfg(not(target_arch = "wasm32"))]
pub mod quic;
#[cfg(not(target_arch = "wasm32"))]
#[path = "quic/accept.rs"]
pub mod native_accept;

pub mod wss;

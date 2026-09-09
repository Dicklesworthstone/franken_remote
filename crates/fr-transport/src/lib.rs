#![forbid(unsafe_code)]
//! Native QUIC record transport, using Asupersync and no second network stack.
//! An established TLS connection and locally admitted channel bindings are
//! inputs, not credentials manufactured here. No listener or WSS fallback is
//! enabled by this crate. Application session negotiation remains a separate
//! boundary. See `QUIC_RECORDS.md` for exact guarantees and qualification scope.
#[cfg(not(target_arch = "wasm32"))]
pub mod quic;

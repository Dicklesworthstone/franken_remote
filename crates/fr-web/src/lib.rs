#![forbid(unsafe_code)]
//! Browser client core (`wasm32-unknown-unknown` clean, zero desktop FFI).
//!
//! Shares the same Rust protocol, HEVC parameter-set guard, session authority,
//! and receiver-credit state machines as native clients (Plan §§11, 12.5, 16;
//! `AGENTS.md` §§3.2, 3.3, 4, 5, 6).
//!
//! Key invariants enforced here:
//! - Observation authority, media readiness, and input authority are separate
//!   state variables. Receiving a first frame never grants input control.
//! - Non-circular media startup: `DecoderConfiguration` -> `DecoderConfigured`
//!   (API configured, not first frame) -> verified IDR -> `FirstFrameDecoded` ->
//!   `Presented` (only while visible).
//! - Exact `hvcC` and SPS-derived `hvc1.*` codec string via [`fr_media::hevc::HevcGuard`];
//!   never hard-coded profile/level strings; unannounced in-band parameter-set
//!   changes are rejected until explicit renegotiation.
//! - Bounded decode queue and renderer retention: cumulative `0x0081 ReceiverPressure`
//!   credits advance ONLY when the browser calls `VideoFrame.close()`, never on
//!   receipt, decode, or socket read alone.
//! - Browser lifecycle transitions (`visibilitychange` hidden, `pagehide`,
//!   `bfcache` restore, decoder reclamation) immediately suspend/revoke control
//!   and require fresh usable-view evidence before control may be requested again.

pub mod adapter;
pub mod credit;
pub mod hevc;
pub mod media_adapter;
pub mod protocol;
pub mod session;

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
mod wasm;

pub use credit::{BrowserCreditError, BrowserCreditTracker};
pub use hevc::{
    BrowserDecoderStage, BrowserHevcError, BrowserHevcPipeline, DecodedFrameRelease,
    DecoderInstance, PreparedWebCodecsChunk, WebCodecsConfig,
};
pub use protocol::{
    ATTACH_MAGIC, AUTH_MAGIC, BrowserChannelKind, BrowserProtocolError, BrowserTransportProfile,
    TICKET_GRANT_MAGIC, encode_attach_message, encode_auth_message, encode_ticket_grant,
    parse_attach_message, parse_auth_message, parse_ticket_grant,
};
pub use session::{
    BrowserControlGrant, BrowserLifecycleEvent, BrowserSessionCore, BrowserSessionSnapshot,
};

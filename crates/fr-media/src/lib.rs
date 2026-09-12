#![forbid(unsafe_code)]
//! `FrankenRemote` media contracts and encoded-picture delivery.
//!
//! This crate owns the bounded media interface, not a codec implementation.
//! Its dependencies are the first-party `fr-core` and `fr-wire` crates; no
//! foreign codec type appears in a session or wire signature.
//!
//! - [`config`]: declared HEVC baseline configuration, coded/visible geometry,
//!   color, and low-delay reference/GOP policy;
//! - [`surface`]: opaque GPU surfaces and observable copy accounting;
//! - [`access_unit`]: length-checked encoded bytes and identity metadata;
//! - [`codec`]: encoder/decoder submit/poll contracts with typed backpressure;
//! - [`capabilities`]: device-keyed capabilities and session admission;
//! - [`delivery`]: executable wire-to-picture reassembly, reliable recovery,
//!   bounded selective repair and decoder-held compressed-memory ownership.
//!
//! Delivery checks completeness and declared reference dependencies; it does
//! not certify HEVC syntax, platform capability, source freshness or authority.
//! Real backends must validate/decode before reporting completion. The test
//! double is feature-gated and is never evidence of hardware support.

pub mod access_unit;
pub mod capabilities;
pub mod codec;
pub mod config;
pub mod delivery;
pub mod freshness;
pub mod hevc;
pub mod pacing;
pub mod surface;

#[cfg(feature = "testing")]
pub mod fake;

/// Private media-process protocol; never accepted on network channels.
pub mod worker;

pub mod receiver_feedback;

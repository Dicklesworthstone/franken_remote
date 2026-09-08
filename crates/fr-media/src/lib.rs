#![forbid(unsafe_code)]
//! `FrankenRemote` media contracts (plan sections 8.1, 8.2, 9.1).
//!
//! `FrankenRemote` owns the media *interface*, not the codec. This crate is the
//! bounded, ownership-aware contract that every backend implements — Apple
//! `VideoToolbox`, the `FFmpeg` hardware bridge, an opt-in software encoder, or
//! the [`fake`] test double — so that no foreign codec type ever appears in a
//! session or wire signature, and any backend can be replaced without
//! rewriting `FrankenRemote`.
//!
//! What lives here:
//!
//! - [`config`]: the validated HEVC baseline configuration — coded vs. visible
//!   geometry, color signalling, profile, and the low-delay reference/GOP
//!   policy — with construction that refuses anything outside the admitted
//!   subset;
//! - [`surface`]: the opaque [`surface::GpuSurface`] contract and pixel
//!   formats, plus [`surface::CopyLedger`] so every CPU/GPU copy on a path is
//!   counted rather than hidden;
//! - [`access_unit`]: [`access_unit::EncodedAccessUnit`] and its frame
//!   identity/kind/dependency metadata, bounded by the shared limits;
//! - [`codec`]: the [`codec::Encoder`]/[`codec::Decoder`] send/receive state
//!   machines, expressed as runtime-agnostic submit/poll traits with typed
//!   backpressure/EOF/device-loss/fatal outcomes;
//! - [`capabilities`]: probed [`capabilities::MediaCapabilities`], the
//!   device-identity-keyed probe cache that invalidates on device loss, and
//!   session admission against current capacity.
//!
//! Two doctrines are enforced structurally: this crate depends only on
//! `fr-core` (so a foreign codec type *cannot* leak into a public signature),
//! and "hardware-required" is a typed refusal or an explicit software-profile
//! selection — never a silent library-internal fallback.

pub mod access_unit;
pub mod capabilities;
pub mod codec;
pub mod config;
pub mod surface;

#[cfg(feature = "testing")]
pub mod fake;

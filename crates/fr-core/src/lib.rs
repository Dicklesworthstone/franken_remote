#![forbid(unsafe_code)]
//! `FrankenRemote` core types and deterministic authority policy.
//!
//! This crate owns typed identities, generations, checked protocol limits,
//! host-clock session authorization, and bounded input-sequence accounting.
//! It has no runtime, network listener, OS input injector, or codec backend.
//! Integration must still authenticate peers and check the actual host clock,
//! geometry, mapping, and OS-session state immediately before external work.
//!
//! Identifiers of different kinds never interchange. Limits negotiate only
//! downward. Expired/refused authority cannot authorize a new submission, and
//! evicting a receipt never makes its already-consumed input sequence new.
//! These are core policy guarantees, not end-to-end hardware qualification.

pub mod authority;
pub mod ids;
pub mod input;
pub mod input_sequence;
pub mod limits;
pub mod time;

#![forbid(unsafe_code)]
//! `FrankenRemote` core types.
//!
//! This crate owns the vocabulary every other `FrankenRemote` crate speaks:
//! the distinct identity and generation types that fence stale work (plan
//! section 7.2), and the single tested limits structure that every parser,
//! allocator, and FFI boundary consults before doing work (plan section
//! 17.2), and clock-injected session authority state machines. Nothing here
//! depends on a runtime.
//!
//! Two rules travel with these types everywhere:
//!
//! - identifiers of different kinds never interchange, so a display-geometry
//!   generation can never be passed where an input lease is expected — the
//!   type system, not discipline, enforces the fencing;
//! - every limit is a negotiate-downward ceiling with checked arithmetic, so
//!   over-limit or overflow-adjacent input is a typed refusal before any
//!   allocation or foreign call.

pub mod authority;
pub mod ids;
pub mod limits;
pub mod time;

#![forbid(unsafe_code)]
//! Shared viewer input engine. Transport authentication, renderer evidence and
//! local lifecycle are supplied by the containing native/browser client. This
//! module opens no listener, invents no grant, and never retries an action.
pub mod authority;
pub mod input;

/// Shared native session startup, independent of the windowing/transport adapter.
pub mod startup;

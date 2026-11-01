//! One copy of the real transport and explicit policy fixtures per test crate.
//! Shared by daemon unit tests and the exact-source microphone integration target.
#[path = "../../fr-transport/tests/support/mod.rs"]
#[allow(dead_code)]
pub(crate) mod transport_support;

#[path = "../../fr-native/tests/clipboard_network/attachment.rs"]
#[allow(dead_code)]
pub(crate) mod attachment;

#[path = "../../fr-native/tests/clipboard_network/authority.rs"]
#[allow(dead_code)]
pub(crate) mod authority;

//! Real UDP/TLS/QUIC terminal reporting. The grant/fence is a local fixture;
//! these tests make no native-input, Tailscale, or cleanup-success claim.
#![cfg(target_os = "linux")]
#[allow(dead_code)]
mod support;
#[allow(dead_code)]
#[path = "support/terminal.rs"]
mod terminal;

use asupersync::{cx::Cx, types::CancelKind};
use fr_core::{
    ids::{InputLeaseId, RemoteSessionId},
    limits::ProtocolLimits,
};
use fr_transport::quic::*;
use fr_wire::{
    input::{InputDelivery, InputDirection},
    lease_revoked::{self, CleanupStage, EffectStage, REVOKED_BYTES, Reason, Revoked},
};
use std::{
    cell::Cell,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use support::{both, clock, runtime};

use crate::terminal::{Pair, binding, host as pair, report};
use crate::terminal::{final_report, request, request_bytes, request_then_report, routes};
use fr_wire::closure;

#[path = "terminal_revocation/exchange/controller.rs"]
mod checks;

#[path = "terminal_revocation/exchange/deferred_controller.rs"]
mod deferred_checks;

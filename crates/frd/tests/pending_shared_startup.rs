//! Bounded publisher-owned decoder startup on real TLS/UDP and child IPC.
//! The native codec responses are synthetic, not HEVC/hardware qualification.
#![cfg(target_os = "linux")]
#[path = "shared_startup/support.rs"]
mod support;
use asupersync::cx::Cx;
use fr_media::delivery::{BudgetUsage, ReceivePipeline, ReceivePolicy, SendPolicy};
use fr_transport::quic::{self, Disposition, Messages, Route};
use fr_wire::{
    decoder,
    input::{InputDelivery, InputDirection},
};
use frd::{
    media::{
        ObservationControl, Presenter, SharedCaptureUpdate,
        decoder_startup::{Error, Host, Viewer},
    },
    worker::Deadline,
};
use std::time::Duration;
use support::*;

async fn reap(presenter: &mut Presenter, cx: &Cx) {
    presenter.abort();
    presenter
        .reap(cx, Deadline::after(cx, Duration::from_secs(1)).unwrap())
        .await
        .unwrap();
}

#[path = "shared_startup/pending.rs"]
mod pending;

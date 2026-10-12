#![cfg(target_os = "linux")]
//! Same real UDP/TLS/ATP/disk owners as single-file sends. Admission/input are
//! explicitly local fixtures, not live-tailnet or graphical-selection evidence.
#[allow(dead_code)] // Shared support module: native_quic uses the remaining helpers.
mod native_support;
use asupersync::cx::Cx;
use fr_files::{
    quic::HostReceiver,
    receive::Publication,
    sender::{
        Error, Outcome, Policy, Sender, Stage,
        batch::{MAX_FILES, Report, Selection, Stop},
    },
};
use native_support::{Link, Running, clock, runtime};
use std::{
    fs::{self, File},
    path::Path,
    time::{Duration, Instant},
};

fn selection(path: &Path, names: &[&str]) -> Selection {
    let mut selection = Selection::new();
    for name in names {
        selection.push(File::open(path).unwrap(), name).unwrap();
    }
    selection
}
async fn step(sender: &mut Sender<'_>, host: &mut HostReceiver, link: &mut Link, cx: &Cx) {
    let _ = sender.service(&mut link.c, || true);
    let _ = host.service(&mut link.h, || true);
    link.drive(cx).await;
    let _ = host.service(&mut link.h, || true);
    let _ = sender.service(&mut link.c, || true);
}
async fn finish(
    sender: &mut Sender<'_>,
    host: &mut HostReceiver,
    link: &mut Link,
    cx: &Cx,
) -> Report {
    let until = clock(cx) + 2_000_000;
    loop {
        assert!(
            clock(cx) < until,
            "batch stalled: {:?} {:?}",
            sender.stage(),
            sender.batch_report()
        );
        step(sender, host, link, cx).await;
        if sender.batch_report().unwrap().complete {
            return sender.batch_report().unwrap();
        }
    }
}
fn cleanup(sender: &mut Sender<'_>) -> Report {
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        assert!(Instant::now() < until, "original source failed to drain");
        let _ = sender.try_finish_cleanup();
        if sender.batch_report().unwrap().complete {
            return sender.batch_report().unwrap();
        }
        std::thread::yield_now();
    }
}

#[path = "native_quic/batch/directory.rs"]
mod directory;

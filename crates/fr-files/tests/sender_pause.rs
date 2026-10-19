#![cfg(target_os = "linux")]
//! Real UDP/TLS/ATP/disk pause/resume on one original file lane. Authority and
//! admission fixtures are local; these are not live-tailnet qualification tests.
#[allow(dead_code)] // Shared support module: native_quic uses the remaining helpers.
mod native_support;
use asupersync::cx::Cx;
use fr_files::{
    receive::Publication,
    sender::{
        Error, Outcome, Policy, Sender, Stage,
        batch::{Selection, Stop},
    },
};
use native_support::{Running, clock, runtime};
use std::{
    fs::{self, File},
    time::{Duration, Instant},
};

async fn step(
    sender: &mut Sender<'_>,
    host: &mut fr_files::quic::HostReceiver,
    link: &mut native_support::Link,
    cx: &Cx,
) {
    sender.service(&mut link.c, || true).unwrap();
    host.service(&mut link.h, || true).unwrap();
    link.drive(cx).await;
    host.service(&mut link.h, || true).unwrap();
    sender.service(&mut link.c, || true).unwrap();
}
fn cleanup(sender: &mut Sender<'_>) {
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        assert!(Instant::now() < until, "original source did not stop");
        if let Some(result) = sender.try_finish_cleanup() {
            result.unwrap();
            return;
        }
        std::thread::yield_now();
    }
}

#[test]
fn paused_batch_starts_no_source_then_resumes_files_and_tree_in_order() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut f = Running::new(&cx).await;
        let file = f.path.join("source");
        fs::write(&file, b"file").unwrap();
        let tree = f.path.join("source-tree");
        fs::create_dir(&tree).unwrap();
        fs::create_dir(tree.join("sub")).unwrap();
        fs::write(tree.join("sub/content"), b"tree").unwrap();
        let mut chosen = Selection::new();
        chosen.push(File::open(&file).unwrap(), "first").unwrap();
        chosen
            .push_directory(File::open(&tree).unwrap(), "second")
            .unwrap();
        let mut sender =
            Sender::new(cx.clone(), &f.link.c, &mut f.client, Policy::default()).unwrap();
        sender.set_paused(&f.link.c, true).unwrap();
        sender
            .begin_batch(&f.link.c, chosen, Duration::from_secs(2))
            .unwrap();
        for _ in 0..8 {
            step(&mut sender, &mut f.host, &mut f.link, &cx).await;
            assert!(sender.is_paused());
            assert_eq!(sender.stage(), Stage::Queued);
            assert_eq!(sender.batch_report().unwrap().started, 0);
            assert!(sender.progress().is_none());
            assert!(!f.path.join("first").exists());
            assert!(!f.path.join("second").exists());
        }
        sender.set_paused(&f.link.c, false).unwrap();
        let until = clock(&cx) + 1_500_000;
        while !sender.batch_report().unwrap().complete {
            assert!(clock(&cx) < until, "{:?}", sender.batch_report());
            step(&mut sender, &mut f.host, &mut f.link, &cx).await;
        }
        let report = sender.take_batch_report().unwrap();
        assert_eq!(
            (report.started, report.not_started(), report.stop),
            (2, 0, None)
        );
        assert_eq!(report.receipts().map(|r| r.id).collect::<Vec<_>>(), [1, 2]);
        assert!(report.receipts().all(|r| matches!(
            r.outcome,
            Outcome::HostPublished {
                bytes: 4,
                publication: Publication::Durable
            }
        )));
        assert_eq!(fs::read(f.path.join("first")).unwrap(), b"file");
        assert_eq!(
            fs::read(f.path.join("second/sub/content")).unwrap(),
            b"tree"
        );
        drop(sender);
        f.input_live(&cx);
    });
}

#[test]
fn paused_stream_admits_no_more_payload_and_resumes_original_transfer() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut f = Running::new(&cx).await;
        let path = f.path.join("source");
        let bytes = vec![0x39; 120_007];
        fs::write(&path, &bytes).unwrap();
        let mut sender =
            Sender::new(cx.clone(), &f.link.c, &mut f.client, Policy::default()).unwrap();
        let id = sender
            .begin(&f.link.c, File::open(&path).unwrap(), "received")
            .unwrap();
        let until = clock(&cx) + 1_500_000;
        while sender.progress().unwrap().queued_bytes == 0 {
            assert!(clock(&cx) < until);
            step(&mut sender, &mut f.host, &mut f.link, &cx).await;
        }
        let paused_at = sender.progress().unwrap();
        assert!(paused_at.queued_bytes < bytes.len() as u64);
        sender.set_paused(&f.link.c, true).unwrap();
        for _ in 0..12 {
            step(&mut sender, &mut f.host, &mut f.link, &cx).await;
            assert_eq!(sender.progress(), Some(paused_at));
            assert_eq!(sender.stage(), Stage::Streaming);
            assert!(sender.result().is_none());
            assert!(!f.path.join("received").exists());
        }
        sender.set_paused(&f.link.c, false).unwrap();
        let receipt = loop {
            assert!(clock(&cx) < until, "{:?}", sender.stage());
            step(&mut sender, &mut f.host, &mut f.link, &cx).await;
            if let Some(receipt) = sender.take_result() {
                break receipt;
            }
        };
        assert_eq!(receipt.id, id);
        assert_eq!(
            receipt.outcome,
            Outcome::HostPublished {
                bytes: bytes.len() as u64,
                publication: Publication::Durable,
            }
        );
        assert_eq!(fs::read(f.path.join("received")).unwrap(), bytes);
        drop(sender);
        f.input_live(&cx);
        assert!(!f.link.c.is_closed() && !f.link.h.is_closed());
    });
}

#[test]
fn pausing_after_completion_does_not_hide_or_rewrite_the_host_proof() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut f = Running::new(&cx).await;
        let path = f.path.join("source");
        fs::write(&path, b"proof").unwrap();
        let mut sender =
            Sender::new(cx.clone(), &f.link.c, &mut f.client, Policy::default()).unwrap();
        sender
            .begin(&f.link.c, File::open(path).unwrap(), "received")
            .unwrap();
        let until = clock(&cx) + 1_500_000;
        while sender.stage() != Stage::AwaitingProof {
            assert!(clock(&cx) < until);
            sender.service(&mut f.link.c, || true).unwrap();
            if sender.stage() != Stage::AwaitingProof {
                f.host.service(&mut f.link.h, || true).unwrap();
                f.link.drive(&cx).await;
            }
        }
        sender.set_paused(&f.link.c, true).unwrap();
        loop {
            assert!(clock(&cx) < until);
            step(&mut sender, &mut f.host, &mut f.link, &cx).await;
            if let Some(receipt) = sender.take_result() {
                assert_eq!(
                    receipt.outcome,
                    Outcome::HostPublished {
                        bytes: 5,
                        publication: Publication::Durable,
                    }
                );
                break;
            }
        }
        assert!(sender.is_paused());
        assert_eq!(fs::read(f.path.join("received")).unwrap(), b"proof");
        drop(sender);
        f.input_live(&cx);
    });
}

#[test]
fn paused_single_file_still_observes_authority_loss_without_network_traffic() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut f = Running::new(&cx).await;
        let path = f.path.join("source");
        fs::write(&path, b"private").unwrap();
        let mut sender =
            Sender::new(cx.clone(), &f.link.c, &mut f.client, Policy::default()).unwrap();
        sender.set_paused(&f.link.c, true).unwrap();
        sender
            .begin(&f.link.c, File::open(path).unwrap(), "never")
            .unwrap();
        assert_eq!(
            sender.service(&mut f.link.c, || false),
            Err(Error::Cancelled)
        );
        cleanup(&mut sender);
        assert_eq!(
            sender.result().unwrap().outcome,
            Outcome::InterruptedBeforePublication(Error::Cancelled)
        );
        assert_eq!(sender.set_paused(&f.link.c, false), Err(Error::Closed));
        assert!(!f.path.join("never").exists());
        drop(sender);
        f.input_live(&cx);
    });
}

#[test]
fn toggling_pause_does_not_renew_queued_batch_deadline_or_start_sources() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut f = Running::new(&cx).await;
        let mut chosen = Selection::new();
        // A forbidden source would fail if the batch ever spawned it. Queuing
        // descriptors alone neither reads them nor invents a transfer receipt.
        chosen
            .push(File::open("/dev/null").unwrap(), "never")
            .unwrap();
        let mut sender =
            Sender::new(cx.clone(), &f.link.c, &mut f.client, Policy::default()).unwrap();
        sender.set_paused(&f.link.c, true).unwrap();
        sender
            .begin_batch(&f.link.c, chosen, Duration::from_millis(10))
            .unwrap();
        let until = clock(&cx) + 1_000_000;
        loop {
            assert!(clock(&cx) < until);
            sender.set_paused(&f.link.c, false).unwrap();
            sender.set_paused(&f.link.c, true).unwrap();
            match sender.service(&mut f.link.c, || true) {
                Err(Error::Expired) => break,
                Ok(Stage::Queued) => {}
                other => panic!("unexpected paused state: {other:?}"),
            }
            assert_eq!(sender.batch_report().unwrap().started, 0);
            f.link.drive(&cx).await;
        }
        cleanup(&mut sender);
        let report = sender.batch_report().unwrap();
        assert!(report.complete);
        assert_eq!(
            (
                report.started,
                report.not_started(),
                report.receipts().len()
            ),
            (0, 1, 0)
        );
        assert_eq!(report.stop, Some(Stop::Local(Error::Expired)));
        assert_eq!(sender.set_paused(&f.link.c, false), Err(Error::Closed));
        assert!(!f.path.join("never").exists());
        drop(sender);
        f.input_live(&cx);
    });
}

#[test]
fn foreign_connection_cannot_change_pause_and_cancel_cannot_be_resumed() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut f = Running::new(&cx).await;
        let foreign = native_support::Link::new(&cx).await;
        let mut sender =
            Sender::new(cx.clone(), &f.link.c, &mut f.client, Policy::default()).unwrap();
        assert_eq!(
            sender.set_paused(&foreign.c, true),
            Err(Error::WrongConnection)
        );
        assert!(!sender.is_paused());
        sender.set_paused(&f.link.c, true).unwrap();
        assert_eq!(
            sender.set_paused(&foreign.c, false),
            Err(Error::WrongConnection)
        );
        assert!(sender.is_paused());
        sender.cancel(&mut f.link.c).unwrap();
        cleanup(&mut sender);
        for pause in [false, true] {
            assert_eq!(sender.set_paused(&f.link.c, pause), Err(Error::Closed));
        }
        assert!(!foreign.c.is_closed() && !f.link.c.is_closed());
        drop(sender);
        f.input_live(&cx);
    });
}

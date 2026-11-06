//! Real file publication on the original UDP/TLS lane, followed by explicit
//! source-thread cleanup faults. No fault substitutes for an ATP proof.
#[allow(dead_code)]
#[path = "../../../tests/native_support/mod.rs"]
mod native_support;

use super::super::{Error, Outcome, Policy, Receipt, Sender, Stage, batch::Selection};
use super::*;
use crate::receive::Publication;
use native_support::{Running, clock, runtime};
use std::{fs, time::Instant};

async fn published(
    sender: &mut Sender<'_>,
    host: &mut crate::quic::HostReceiver,
    link: &mut native_support::Link,
    cx: &Cx,
) -> Receipt {
    let until = clock(cx) + 1_500_000;
    loop {
        assert!(clock(cx) < until, "publication proof did not arrive");
        sender.service(&mut link.c, || true).unwrap();
        host.service(&mut link.h, || true).unwrap();
        link.drive(cx).await;
        host.service(&mut link.h, || true).unwrap();
        sender.service(&mut link.c, || true).unwrap();
        if let Some(receipt) = sender.result() {
            return receipt;
        }
    }
}

fn joined_source(sender: &mut Sender<'_>) {
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(result) = sender.try_finish_cleanup() {
            result.unwrap();
            assert!(sender.transfer.as_ref().unwrap().source.thread.is_none());
            return;
        }
        assert!(Instant::now() < until, "original source did not join");
        thread::yield_now();
    }
}

fn wait_finished(sender: &Sender<'_>) {
    let until = Instant::now() + Duration::from_secs(2);
    while !sender.cleanup_finished() {
        assert!(Instant::now() < until, "fault worker did not finish");
        thread::yield_now();
    }
}

#[test]
fn failed_source_join_preserves_publication_and_never_reopens_the_sender() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut fixture = Running::new(&cx).await;
        let path = fixture.path.join("source");
        fs::write(&path, b"published").unwrap();
        let mut sender = Sender::new(
            cx.clone(),
            &fixture.link.c,
            &mut fixture.client,
            Policy::default(),
        )
        .unwrap();
        sender
            .begin(&fixture.link.c, File::open(&path).unwrap(), "received")
            .unwrap();
        let receipt = published(&mut sender, &mut fixture.host, &mut fixture.link, &cx).await;
        assert_eq!(
            receipt.outcome,
            Outcome::HostPublished {
                bytes: 9,
                publication: Publication::Durable,
            }
        );
        joined_source(&mut sender);
        // Inject a real failed JoinHandle only after the actual source joined.
        // The receipt above still came from the unchanged receiver and filesystem.
        let transfer = sender.transfer.as_mut().unwrap();
        transfer.source.thread = Some(thread::spawn(|| panic!("injected source cleanup failure")));
        transfer.cleanup = None;
        wait_finished(&sender);

        for _ in 0..3 {
            assert!(sender.take_result().is_none());
            assert_eq!(sender.result(), Some(receipt));
            assert_eq!(sender.try_finish_cleanup(), Some(Err(Error::Worker)));
            assert_eq!(sender.stage(), Stage::Finished);
            assert_eq!(
                sender.begin(&fixture.link.c, File::open(&path).unwrap(), "later"),
                Err(Error::Busy)
            );
            let mut next = Selection::new();
            next.push(File::open(&path).unwrap(), "batch-later")
                .unwrap();
            assert_eq!(
                sender.begin_batch(&fixture.link.c, next, Duration::from_secs(1)),
                Err(Error::Busy)
            );
        }
        sender.cancel(&mut fixture.link.c).unwrap();
        sender.stop();
        assert!(sender.take_result().is_none());
        assert_eq!(sender.result(), Some(receipt));
        assert_eq!(sender.try_finish_cleanup(), Some(Err(Error::Worker)));
        assert_eq!(sender.stage(), Stage::Finished);
        assert_eq!(
            fs::read(fixture.path.join("received")).unwrap(),
            b"published"
        );
        assert!(!fixture.path.join("later").exists());
        assert!(!fixture.path.join("batch-later").exists());
        drop(sender);
        fixture.input_live(&cx);
    });
}

#[test]
fn result_collection_waits_for_the_original_join_then_allows_a_new_source() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut fixture = Running::new(&cx).await;
        let path = fixture.path.join("source");
        fs::write(&path, b"published").unwrap();
        let mut sender = Sender::new(
            cx.clone(),
            &fixture.link.c,
            &mut fixture.client,
            Policy::default(),
        )
        .unwrap();
        let first = sender
            .begin(&fixture.link.c, File::open(&path).unwrap(), "first")
            .unwrap();
        let receipt = published(&mut sender, &mut fixture.host, &mut fixture.link, &cx).await;
        joined_source(&mut sender);
        let (release, finish) = mpsc::sync_channel(1);
        let transfer = sender.transfer.as_mut().unwrap();
        transfer.source.thread = Some(thread::spawn(move || finish.recv().unwrap()));
        transfer.cleanup = None;

        for _ in 0..16 {
            assert!(sender.take_result().is_none());
            assert_eq!(sender.result(), Some(receipt));
            assert_eq!(sender.try_finish_cleanup(), None);
            assert!(!sender.cleanup_finished());
            assert_eq!(sender.stage(), Stage::Finished);
            assert_eq!(
                sender.begin(&fixture.link.c, File::open(&path).unwrap(), "second"),
                Err(Error::Busy)
            );
        }
        release.send(()).unwrap();
        wait_finished(&sender);
        assert_eq!(sender.take_result(), Some(receipt));
        assert_eq!(sender.take_result(), None);
        assert_eq!(sender.try_finish_cleanup(), Some(Ok(())));
        assert_eq!(sender.stage(), Stage::Idle);
        assert_eq!(
            sender
                .begin(&fixture.link.c, File::open(&path).unwrap(), "second")
                .unwrap(),
            first + 1
        );
        let second = published(&mut sender, &mut fixture.host, &mut fixture.link, &cx).await;
        joined_source(&mut sender);
        assert_eq!(sender.take_result(), Some(second));
        assert_eq!(second.outcome, receipt.outcome);
        for name in ["first", "second"] {
            assert_eq!(fs::read(fixture.path.join(name)).unwrap(), b"published");
        }
        sender.cancel(&mut fixture.link.c).unwrap();
        drop(sender);
        fixture.input_live(&cx);
    });
}

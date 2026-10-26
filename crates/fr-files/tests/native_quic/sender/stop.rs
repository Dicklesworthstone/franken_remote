//! Synchronous source fencing precedes the session's independent control drain.
use super::*;
use fr_files::sender::batch::{Selection, Stop};

#[test]
fn stop_fences_idle_queued_and_preparing_sources_without_resetting_control() {
    runtime().block_on(async {
        for mode in 0..3 {
            let cx = Cx::current().unwrap();
            let mut f = Running::new(&cx).await;
            let source = f.path.join("source");
            fs::write(&source, b"selected local bytes").unwrap();
            let mut sender =
                Sender::new(cx.clone(), &f.link.c, &mut f.client, Policy::default()).unwrap();
            if mode == 1 {
                let mut selected = Selection::new();
                selected
                    .push(File::open(&source).unwrap(), "queued")
                    .unwrap();
                sender
                    .begin_batch(&f.link.c, selected, Duration::from_secs(1))
                    .unwrap();
            } else if mode == 2 {
                sender
                    .begin(&f.link.c, File::open(&source).unwrap(), "preparing")
                    .unwrap();
            }
            let before = f.link.c.usage();
            sender.stop();
            assert_eq!(f.link.c.usage(), before);
            assert_eq!(
                sender.begin(&f.link.c, File::open(&source).unwrap(), "retry"),
                Err(Error::Closed)
            );
            assert_eq!(
                sender.begin_directory(&f.link.c, File::open(&f.path).unwrap(), "retry-tree"),
                Err(Error::Closed)
            );
            if mode == 1 {
                let report = sender.batch_report().unwrap();
                assert_eq!(report.started, 0);
                assert_eq!(report.not_started(), 1);
                assert_eq!(report.stop, Some(Stop::Local(Error::Cancelled)));
                assert!(report.complete);
            } else if mode == 2 {
                assert_eq!(
                    cleanup(&mut sender).outcome,
                    Outcome::InterruptedBeforePublication(Error::Cancelled)
                );
            }
            drop(sender);
            f.link.c.tick(&cx, || true).unwrap();
            f.input_live(&cx);
            assert!(!f.path.join("queued").exists());
            assert!(!f.path.join("preparing").exists());
        }
    });
}

#[test]
fn requested_cancel_preserves_publication_and_finishes_already_retired_original_pair() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut f = Running::new(&cx).await;
        let source = f.path.join("source");
        fs::write(&source, b"durable").unwrap();
        let mut sender =
            Sender::new(cx.clone(), &f.link.c, &mut f.client, Policy::default()).unwrap();
        sender
            .begin(&f.link.c, File::open(&source).unwrap(), "received")
            .unwrap();
        let until = clock(&cx) + 1_500_000;
        while sender.result().is_none() {
            assert!(clock(&cx) < until, "publication proof did not arrive");
            step(&mut sender, &mut f.host, &mut f.link, &cx).await;
        }
        let actual = sender.result().unwrap();
        assert_eq!(
            actual.outcome,
            Outcome::HostPublished {
                bytes: 7,
                publication: Publication::Durable
            }
        );
        let before = f.link.c.usage();
        sender.request_cancel();
        sender.request_cancel();
        assert_eq!(f.link.c.usage(), before);
        assert_eq!(sender.result(), Some(actual));
        // The next original connection check services the request. Finishing
        // cancellation must accept that tombstone without resetting the parent.
        f.link.c.tick(&cx, || true).unwrap();
        sender.cancel(&mut f.link.c).unwrap();
        assert!(!f.link.c.is_closed());
        assert_eq!(cleanup(&mut sender), actual);
        assert_eq!(fs::read(f.path.join("received")).unwrap(), b"durable");
        drop(sender);
        f.input_live(&cx);
    });
}

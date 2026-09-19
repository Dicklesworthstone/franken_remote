//! Same real UDP/TLS/ATP/disk owners as single-file sends. Admission/input are
//! explicitly local fixtures, not live-tailnet or graphical-selection evidence.
use super::native_support;
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
#[test]
fn bounded_selection_rejects_duplicates_paths_and_overflow_without_source_io() {
    let path = Path::new("/dev/null"); // No read occurs while selecting a descriptor.
    let mut batch = selection(path, &["private-name"]);
    for name in [
        "private-name",
        "../escape",
        "/absolute",
        ".fr-part-private",
        "a/b",
        "",
    ] {
        assert_eq!(
            batch.push(File::open(path).unwrap(), name),
            Err(Error::Name)
        );
        assert_eq!(batch.len(), 1);
    }
    assert_eq!(
        batch.push(File::open(path).unwrap(), &"a".repeat(256)),
        Err(Error::Name)
    );
    assert!(!format!("{batch:?}").contains("private-name"));
    for n in 1..MAX_FILES {
        batch
            .push(File::open(path).unwrap(), &format!("item-{n}"))
            .unwrap();
    }
    assert_eq!(
        batch.push(File::open(path).unwrap(), "too-many"),
        Err(Error::Limits)
    );
    assert_eq!(batch.len(), MAX_FILES);
}
#[test]
fn actual_batch_publishes_selected_descriptors_and_empty_file_on_one_lane() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut f = Running::new(&cx).await;
        let path = f.path.join("source");
        let empty = f.path.join("empty-source");
        let data = vec![0x31; 120_007];
        fs::write(&path, &data).unwrap();
        fs::write(&empty, []).unwrap();
        let mut chosen = selection(&path, &["first", "second"]);
        chosen.push(File::open(&empty).unwrap(), "empty").unwrap();
        // Path replacement after selection cannot redirect even the queued source.
        fs::rename(&path, f.path.join("original")).unwrap();
        fs::write(&path, b"replacement").unwrap();
        let mut sender =
            Sender::new(cx.clone(), &f.link.c, &mut f.client, Policy::default()).unwrap();
        sender
            .begin_batch(&f.link.c, chosen, Duration::from_secs(2))
            .unwrap();
        assert_eq!(sender.stage(), Stage::Queued);
        assert_eq!(sender.batch_report().unwrap().started, 0);
        assert!(!sender.cleanup_finished());
        assert!(sender.try_finish_cleanup().is_none());
        assert_eq!(
            sender.begin(&f.link.c, File::open(&path).unwrap(), "interleaved"),
            Err(Error::Busy)
        );
        assert_eq!(
            sender.begin_batch(
                &f.link.c,
                selection(&path, &["other"]),
                Duration::from_secs(1)
            ),
            Err(Error::Busy)
        );
        let report = finish(&mut sender, &mut f.host, &mut f.link, &cx).await;
        assert_eq!(
            (report.total, report.started, report.not_started()),
            (3, 3, 0)
        );
        assert_eq!(report.stop, None);
        for (index, receipt) in report.receipts().enumerate() {
            assert_eq!(receipt.id, index as u64 + 1);
            assert_eq!(
                receipt.outcome,
                Outcome::HostPublished {
                    bytes: if index == 2 { 0 } else { 120_007 },
                    publication: Publication::Durable,
                }
            );
        }
        for name in ["first", "second"] {
            assert_eq!(fs::read(f.path.join(name)).unwrap(), data);
        }
        assert_eq!(fs::read(f.path.join("empty")).unwrap(), [] as [u8; 0]);
        assert!(sender.take_result().is_none()); // Cannot steal a batch's receipt.
        assert_eq!(sender.take_batch_report(), Some(report));
        assert_eq!(sender.stage(), Stage::Idle);
        // Same sender/sequence domain, no new attachment or quota reset.
        assert_eq!(
            sender
                .begin(&f.link.c, File::open(&empty).unwrap(), "after")
                .unwrap(),
            4
        );
        sender.cancel(&mut f.link.c).unwrap();
        let until = Instant::now() + Duration::from_secs(2);
        while sender.take_result().is_none() {
            assert!(Instant::now() < until);
            std::thread::yield_now();
        }
        drop(sender);
        f.input_live(&cx);
        assert!(!f.link.c.is_closed() && !f.link.h.is_closed());
    });
}
#[test]
fn a_host_conflict_preserves_prior_publication_and_never_starts_later_files() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut f = Running::new(&cx).await;
        let path = f.path.join("source");
        fs::write(&path, b"selected").unwrap();
        fs::write(f.path.join("conflict"), b"keep").unwrap();
        let mut sender =
            Sender::new(cx.clone(), &f.link.c, &mut f.client, Policy::default()).unwrap();
        sender
            .begin_batch(
                &f.link.c,
                selection(&path, &["first", "conflict", "never"]),
                Duration::from_secs(2),
            )
            .unwrap();
        let report = finish(&mut sender, &mut f.host, &mut f.link, &cx).await;
        assert_eq!(
            report.stop,
            Some(Stop::HostRefused(fr_wire::files::Reason::Conflict))
        );
        assert_eq!(
            (
                report.started,
                report.not_started(),
                report.receipts().len()
            ),
            (2, 1, 2)
        );
        assert!(matches!(
            report.receipts().next().unwrap().outcome,
            Outcome::HostPublished { .. }
        ));
        assert_eq!(fs::read(f.path.join("first")).unwrap(), b"selected");
        assert_eq!(fs::read(f.path.join("conflict")).unwrap(), b"keep");
        assert!(!f.path.join("never").exists());
        let _ = sender.take_batch_report().unwrap();
        assert_eq!(
            sender.begin(&f.link.c, File::open(&path).unwrap(), "retry"),
            Err(Error::Closed)
        );
        drop(sender);
        f.input_live(&cx);
    });
}
#[test]
fn deadline_starts_before_first_poll_and_does_not_spawn_a_source() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut f = Running::new(&cx).await;
        let mut sender =
            Sender::new(cx.clone(), &f.link.c, &mut f.client, Policy::default()).unwrap();
        sender
            .begin_batch(
                &f.link.c,
                selection(Path::new("/dev/null"), &["never"]),
                Duration::from_micros(1),
            )
            .unwrap();
        std::thread::sleep(Duration::from_millis(1));
        assert_eq!(sender.service(&mut f.link.c, || true), Err(Error::Expired));
        let report = sender.take_batch_report().unwrap();
        assert_eq!(
            (
                report.started,
                report.not_started(),
                report.receipts().len()
            ),
            (0, 1, 0)
        );
        assert_eq!(report.stop, Some(Stop::Local(Error::Expired)));
        assert!(sender.cleanup_finished());
        assert!(!f.link.c.is_closed());
    });
}
#[test]
fn denied_authority_and_unpolled_cancel_drop_pending_descriptors_without_reading() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        for cancel in [false, true] {
            let mut f = Running::new(&cx).await;
            let mut sender =
                Sender::new(cx.clone(), &f.link.c, &mut f.client, Policy::default()).unwrap();
            sender
                .begin_batch(
                    &f.link.c,
                    selection(Path::new("/dev/null"), &["never", "also-never"]),
                    Duration::from_secs(1),
                )
                .unwrap();
            if cancel {
                sender.cancel(&mut f.link.c).unwrap();
            } else {
                assert_eq!(
                    sender.service(&mut f.link.c, || false),
                    Err(Error::Cancelled)
                );
            }
            let report = cleanup(&mut sender);
            assert_eq!(
                (
                    report.started,
                    report.not_started(),
                    report.receipts().len()
                ),
                (0, 2, 0)
            );
            assert_eq!(report.stop, Some(Stop::Local(Error::Cancelled)));
            assert!(!f.link.c.is_closed());
        }
    });
}
#[test]
fn cancellation_after_first_proof_retains_it_and_never_starts_queued_sources() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut f = Running::new(&cx).await;
        let path = f.path.join("source");
        fs::write(&path, b"selected").unwrap();
        let mut sender =
            Sender::new(cx.clone(), &f.link.c, &mut f.client, Policy::default()).unwrap();
        sender
            .begin_batch(
                &f.link.c,
                selection(&path, &["first", "never"]),
                Duration::from_secs(2),
            )
            .unwrap();
        let until = clock(&cx) + 1_000_000;
        while sender.batch_report().unwrap().receipts().len() == 0 {
            assert!(clock(&cx) < until);
            sender.service(&mut f.link.c, || true).unwrap();
            f.host.service(&mut f.link.h, || true).unwrap();
            f.link.drive(&cx).await;
        }
        sender.cancel(&mut f.link.c).unwrap();
        let report = cleanup(&mut sender);
        assert_eq!((report.started, report.not_started()), (1, 1));
        assert!(matches!(
            report.receipts().next().unwrap().outcome,
            Outcome::HostPublished { .. }
        ));
        assert_eq!(fs::read(f.path.join("first")).unwrap(), b"selected");
        assert!(!f.path.join("never").exists());
    });
}
#[test]
fn source_refusal_stops_the_batch_and_preserves_its_original_receipt_after_close() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut f = Running::new(&cx).await;
        let mut sender =
            Sender::new(cx.clone(), &f.link.c, &mut f.client, Policy::default()).unwrap();
        sender
            .begin_batch(
                &f.link.c,
                selection(Path::new("/dev/null"), &["bad", "never"]),
                Duration::from_secs(1),
            )
            .unwrap();
        let until = clock(&cx) + 1_000_000;
        while sender.batch_report().unwrap().stop.is_none() {
            assert!(clock(&cx) < until);
            let _ = sender.service(&mut f.link.c, || true);
            std::thread::yield_now();
        }
        f.link.c.close();
        let report = cleanup(&mut sender);
        assert_eq!((report.started, report.not_started()), (1, 1));
        assert_eq!(
            report.receipts().next().unwrap().outcome,
            Outcome::InterruptedBeforePublication(Error::Source)
        );
        assert_eq!(sender.batch_report(), Some(report));
    });
}
#[test]
fn invalid_setup_preserves_the_original_idle_lane() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut f = Running::new(&cx).await;
        let mut sender =
            Sender::new(cx.clone(), &f.link.c, &mut f.client, Policy::default()).unwrap();
        assert_eq!(
            sender.begin_batch(&f.link.c, Selection::new(), Duration::from_secs(1)),
            Err(Error::Limits)
        );
        for lifetime in [Duration::ZERO, Duration::from_secs(3601)] {
            assert_eq!(
                sender.begin_batch(
                    &f.link.c,
                    selection(Path::new("/dev/null"), &["bad"]),
                    lifetime
                ),
                Err(Error::Limits)
            );
        }
        let foreign = Link::new(&cx).await;
        assert_eq!(
            sender.begin_batch(
                &foreign.c,
                selection(Path::new("/dev/null"), &["bad"]),
                Duration::from_secs(1)
            ),
            Err(Error::WrongConnection)
        );
        assert_eq!(sender.stage(), Stage::Idle);
        assert_eq!(sender.batch_report(), None);
        assert!(!f.link.c.is_closed());
    });
}

#[test]
fn lost_second_proof_keeps_first_receipt_and_second_unknown_effect_without_retry() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut f = Running::new(&cx).await;
        let path = f.path.join("source");
        fs::write(&path, b"selected").unwrap();
        let mut sender =
            Sender::new(cx.clone(), &f.link.c, &mut f.client, Policy::default()).unwrap();
        sender
            .begin_batch(
                &f.link.c,
                selection(&path, &["first", "second", "never"]),
                Duration::from_secs(2),
            )
            .unwrap();
        let until = clock(&cx) + 1_000_000;
        while sender.batch_report().unwrap().started != 2 || sender.stage() != Stage::AwaitingProof
        {
            assert!(clock(&cx) < until);
            sender.service(&mut f.link.c, || true).unwrap();
            f.host.service(&mut f.link.h, || true).unwrap();
            f.link.drive(&cx).await;
        }
        // Publish for real, but never deliver its proof to the sender.
        while !f.path.join("second").exists() {
            assert!(clock(&cx) < until);
            f.host.service(&mut f.link.h, || true).unwrap();
            f.link.drive(&cx).await;
        }
        sender.cancel(&mut f.link.c).unwrap();
        f.link.c.close();
        let report = cleanup(&mut sender);
        assert_eq!((report.started, report.not_started()), (2, 1));
        let receipts: Vec<_> = report.receipts().collect();
        assert!(matches!(receipts[0].outcome, Outcome::HostPublished { .. }));
        assert_eq!(receipts[1].outcome, Outcome::PublicationUnknown);
        for name in ["first", "second"] {
            assert_eq!(fs::read(f.path.join(name)).unwrap(), b"selected");
        }
        assert!(!f.path.join("never").exists());
        assert_eq!(sender.batch_report(), Some(report));
    });
}
#[test]
fn an_earlier_success_does_not_restart_the_batch_deadline_for_queued_files() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut f = Running::new(&cx).await;
        let path = f.path.join("source");
        fs::write(&path, b"selected").unwrap();
        let mut sender =
            Sender::new(cx.clone(), &f.link.c, &mut f.client, Policy::default()).unwrap();
        let begun = clock(&cx);
        sender
            .begin_batch(
                &f.link.c,
                selection(&path, &["first", "never"]),
                Duration::from_millis(200),
            )
            .unwrap();
        while sender.batch_report().unwrap().receipts().len() == 0 {
            assert!(clock(&cx) < begun + 200_000);
            sender.service(&mut f.link.c, || true).unwrap();
            f.host.service(&mut f.link.h, || true).unwrap();
            f.link.drive(&cx).await;
        }
        std::thread::sleep(Duration::from_micros(
            (begun + 201_000).saturating_sub(clock(&cx)),
        ));
        assert_eq!(sender.service(&mut f.link.c, || true), Err(Error::Expired));
        let report = cleanup(&mut sender);
        assert_eq!((report.started, report.not_started()), (1, 1));
        assert_eq!(report.stop, Some(Stop::Local(Error::Expired)));
        assert!(matches!(
            report.receipts().next().unwrap().outcome,
            Outcome::HostPublished { .. }
        ));
        assert!(!f.path.join("never").exists());
    });
}
#[test]
fn completed_batch_report_is_not_rewritten_by_later_parent_cleanup() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut f = Running::new(&cx).await;
        let path = f.path.join("source");
        fs::write(&path, b"selected").unwrap();
        let mut sender =
            Sender::new(cx.clone(), &f.link.c, &mut f.client, Policy::default()).unwrap();
        sender
            .begin_batch(
                &f.link.c,
                selection(&path, &["first"]),
                Duration::from_secs(1),
            )
            .unwrap();
        let report = finish(&mut sender, &mut f.host, &mut f.link, &cx).await;
        assert_eq!(report.stop, None);
        sender.cancel(&mut f.link.c).unwrap();
        assert_eq!(sender.batch_report(), Some(report));
        assert_eq!(sender.try_finish_cleanup(), Some(Ok(())));
        assert_eq!(sender.take_batch_report(), Some(report));
        assert_eq!(sender.stage(), Stage::Closed);
    });
}

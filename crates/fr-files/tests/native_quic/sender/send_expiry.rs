//! Real source, UDP/TLS, ATP and disk owners. A completed file pair may end
//! after a settled send expires; its original effects and cleanup still matter.
use super::*;
use fr_files::quic::{self, State};
use fr_transport::quic::{Error as TransportError, MAX_DELIVERY_ALLOWANCE_US, QuicRecords};

async fn quiet(link: &mut native_support::Link, cx: &Cx) {
    let until = clock(cx) + 1_000_000;
    while link.c.usage().retained_send_records != 0 || link.h.usage().retained_send_records != 0 {
        assert!(clock(cx) < until, "original file records did not drain");
        link.drive(cx).await;
    }
}

async fn expire_retained(q: &mut QuicRecords, cx: &Cx) {
    assert!(q.usage().retained_send_records > 0);
    assert_eq!(q.usage().critical_send_records, 0);
    let until = clock(cx) + MAX_DELIVERY_ALLOWANCE_US + 100_000;
    // Do not start native I/O. The original connection remains in settled
    // custody while its actual, unchanged retained-send deadline elapses.
    while q.usage().retained_send_records != 0 {
        assert!(clock(cx) < until, "file delivery deadline was renewed");
        asupersync::time::sleep(cx.now(), Duration::from_millis(5)).await;
        q.tick(cx, || true).unwrap();
    }
    assert_eq!(q.usage().retained_send_upper_bound, 0);
    assert!(!q.is_closed());
}

async fn retire_remote_receiver(f: &mut Running, cx: &Cx) {
    let until = clock(cx) + 500_000;
    loop {
        assert!(
            clock(cx) < until,
            "file reset never reached its original peer"
        );
        f.link.drive(cx).await;
        match f.host.service(&mut f.link.h, || true) {
            Err(error) => {
                assert_eq!(error, quic::Error::Transport(TransportError::SendExpired));
                break;
            }
            Ok(state) => assert_eq!(state, State::Active),
        }
    }
    assert_eq!(f.host.state(), State::Retired);
    super::super::clean(f);
    assert_eq!(f.host.try_finish_cleanup(), Some(Ok(())));
}

#[test]
fn a_later_send_expiry_keeps_prior_batch_receipts_and_never_starts_the_successor() {
    use fr_files::sender::batch::{Selection, Stop};
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut f = Running::new(&cx).await;
        let source = f.path.join("source");
        fs::write(&source, b"durable").unwrap();
        let mut selection = Selection::new();
        for name in ["first", "second", "third"] {
            selection.push(File::open(&source).unwrap(), name).unwrap();
        }
        let mut sender =
            Sender::new(cx.clone(), &f.link.c, &mut f.client, Policy::default()).unwrap();
        sender
            .begin_batch(&f.link.c, selection, Duration::from_secs(10))
            .unwrap();
        let until = clock(&cx) + 1_000_000;
        loop {
            assert!(clock(&cx) < until, "first batch publication did not finish");
            sender.service(&mut f.link.c, || true).unwrap();
            if sender.batch_report().unwrap().receipts().len() == 1 {
                break;
            }
            f.host.service(&mut f.link.h, || true).unwrap();
            f.link.drive(&cx).await;
            f.host.service(&mut f.link.h, || true).unwrap();
        }
        let first = sender.batch_report().unwrap().receipts().next().unwrap();
        assert_eq!(
            first.outcome,
            Outcome::HostPublished {
                bytes: 7,
                publication: Publication::Durable,
            }
        );
        let published = f.host.last_result().unwrap();
        quiet(&mut f.link, &cx).await;

        // The second selection really starts a source and admits its offer;
        // neither peer is driven again until that exact send has expired.
        let until = clock(&cx) + 1_000_000;
        while sender.stage() != Stage::AwaitingAcceptance {
            assert!(clock(&cx) < until, "second batch offer was not admitted");
            sender.service(&mut f.link.c, || true).unwrap();
            std::thread::yield_now();
        }
        assert_eq!(sender.batch_report().unwrap().started, 2);
        expire_retained(&mut f.link.c, &cx).await;
        let cause = Error::Transport(TransportError::SendExpired);
        assert_eq!(sender.service(&mut f.link.c, || true), Err(cause));
        let cleanup_until = Instant::now() + Duration::from_secs(2);
        while !sender.batch_report().unwrap().complete {
            assert!(
                Instant::now() < cleanup_until,
                "batch source cleanup stalled"
            );
            let _ = sender.try_finish_cleanup();
            std::thread::yield_now();
        }
        let report = sender.take_batch_report().unwrap();
        assert_eq!(report.stop, Some(Stop::Local(cause)));
        assert_eq!(
            (report.total, report.started, report.not_started()),
            (3, 2, 1)
        );
        assert_eq!(
            report.receipts().collect::<Vec<_>>(),
            vec![
                first,
                Receipt {
                    id: 2,
                    outcome: Outcome::InterruptedBeforePublication(cause),
                },
            ]
        );
        assert_eq!(
            sender.begin(&f.link.c, File::open(&source).unwrap(), "retry"),
            Err(Error::Closed)
        );
        drop(sender);
        retire_remote_receiver(&mut f, &cx).await;
        assert_eq!(f.host.last_result(), Some(published));
        assert_eq!(fs::read(f.path.join("first")).unwrap(), b"durable");
        assert!(!f.path.join("second").exists());
        assert!(!f.path.join("third").exists());
        assert!(!f.path.join("retry").exists());
        f.input_live(&cx);
        f.link.control_progress(&cx).await;
    });
}

#[test]
fn expired_retained_payload_cleans_real_staging_and_preserves_the_original_control() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut f = Running::new(&cx).await;
        let source = f.path.join("source");
        let bytes = vec![0x63; 200_000];
        fs::write(&source, &bytes).unwrap();
        let mut sender =
            Sender::new(cx.clone(), &f.link.c, &mut f.client, Policy::default()).unwrap();
        sender
            .begin(&f.link.c, File::open(&source).unwrap(), "received")
            .unwrap();

        // Let one real payload reach disk, then drain its acknowledgements.
        // The next retained payload will expire before completion is admitted.
        let until = clock(&cx) + 1_000_000;
        while sender.progress().is_none_or(|p| p.queued_bytes == 0) {
            assert!(clock(&cx) < until, "first source chunk was not admitted");
            step(&mut sender, &mut f.host, &mut f.link, &cx).await;
        }
        sender.set_paused(&f.link.c, true).unwrap();
        while f.host.progress().is_none_or(|p| p.staged_bytes == 0) {
            assert!(clock(&cx) < until, "first chunk never reached disk");
            step(&mut sender, &mut f.host, &mut f.link, &cx).await;
        }
        quiet(&mut f.link, &cx).await;
        let staged_bytes = f.host.progress().unwrap().staged_bytes;
        assert!(staged_bytes > 0 && staged_bytes < bytes.len() as u64);
        let staged = super::super::receive_cleanup::staged_handles(&f.path)
            .into_iter()
            .find(|fd| fs::metadata(fd).unwrap().len() == staged_bytes)
            .expect("original partial inode remains open");
        assert_eq!(
            fs::read(staged).unwrap(),
            bytes[..usize::try_from(staged_bytes).unwrap()]
        );
        assert!(!f.path.join("received").exists());

        sender.set_paused(&f.link.c, false).unwrap();
        let until = clock(&cx) + 300_000;
        while f.link.c.usage().retained_send_records == 0 {
            assert!(clock(&cx) < until, "next source chunk was not admitted");
            sender.service(&mut f.link.c, || true).unwrap();
            std::thread::yield_now();
        }
        let queued = sender.progress().unwrap().queued_bytes;
        assert!(queued > staged_bytes && queued < bytes.len() as u64);
        assert_eq!(sender.stage(), Stage::Streaming);
        expire_retained(&mut f.link.c, &cx).await;

        let cause = Error::Transport(TransportError::SendExpired);
        assert_eq!(sender.service(&mut f.link.c, || true), Err(cause));
        assert_eq!(
            cleanup(&mut sender).outcome,
            Outcome::InterruptedBeforePublication(cause)
        );
        assert_eq!(
            sender.begin(&f.link.c, File::open(&source).unwrap(), "retry"),
            Err(Error::Closed)
        );
        drop(sender);
        assert!(f.client.send_expired(&f.link.c));
        assert!(!f.client.receive_expired(&f.link.c));
        retire_remote_receiver(&mut f, &cx).await;
        assert!(super::super::receive_cleanup::staged_handles(&f.path).is_empty());
        assert_eq!(fs::read_dir(&f.path).unwrap().count(), 1);
        assert!(!f.path.join("received").exists());
        assert!(!f.path.join("retry").exists());
        assert_eq!(fs::read(&source).unwrap(), bytes);
        f.input_live(&cx);
        f.link.control_progress(&cx).await;
        assert!(!f.link.c.is_closed() && !f.link.h.is_closed());
    });
}

#[test]
fn expired_publication_reply_keeps_actual_disk_effect_and_sender_uncertainty() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut f = Running::new(&cx).await;
        let source = f.path.join("source");
        fs::write(&source, b"committed").unwrap();
        let mut sender =
            Sender::new(cx.clone(), &f.link.c, &mut f.client, Policy::default()).unwrap();
        sender
            .begin(&f.link.c, File::open(&source).unwrap(), "delivered")
            .unwrap();

        // Stop before the completing record enters a native I/O turn. Then
        // deliver and acknowledge it without dispatching to the disk worker.
        let until = clock(&cx) + 1_000_000;
        loop {
            assert!(clock(&cx) < until, "completion was not admitted");
            sender.service(&mut f.link.c, || true).unwrap();
            if sender.stage() == Stage::AwaitingProof {
                break;
            }
            f.host.service(&mut f.link.h, || true).unwrap();
            f.link.drive(&cx).await;
            f.host.service(&mut f.link.h, || true).unwrap();
        }
        quiet(&mut f.link, &cx).await;
        assert_eq!(f.link.c.usage().retained_send_records, 0);
        assert!(sender.result().is_none());
        assert!(!f.path.join("delivered").exists());

        // Consume the real completion, let atomic publication finish, and retain
        // its original proof without ever driving those outbound proof bytes.
        let until = clock(&cx) + 1_000_000;
        while !matches!(
            f.host.last_result().map(|receipt| receipt.outcome),
            Some(Ok(fr_files::worker::Completion::Published(_)))
        ) {
            assert!(clock(&cx) < until, "publication did not finish");
            f.host.service(&mut f.link.h, || true).unwrap();
            std::thread::yield_now();
        }
        let published = f.host.last_result().unwrap();
        assert!(matches!(
            published.outcome,
            Ok(fr_files::worker::Completion::Published(receipt)) if receipt.bytes == 9
        ));
        assert_eq!(fs::read(f.path.join("delivered")).unwrap(), b"committed");
        assert_eq!(f.link.h.usage().retained_send_records, 1);
        expire_retained(&mut f.link.h, &cx).await;
        assert_eq!(
            f.host.service(&mut f.link.h, || true),
            Err(quic::Error::Transport(TransportError::SendExpired))
        );
        assert_eq!(f.host.state(), State::Retired);
        assert_eq!(f.host.last_result(), Some(published));

        let until = clock(&cx) + 500_000;
        loop {
            assert!(clock(&cx) < until, "publication reset did not reach sender");
            f.link.drive(&cx).await;
            match sender.service(&mut f.link.c, || true) {
                Err(error) => {
                    assert_eq!(error, Error::Transport(TransportError::SendExpired));
                    break;
                }
                Ok(stage) => assert_eq!(stage, Stage::AwaitingProof),
            }
        }
        assert_eq!(cleanup(&mut sender).outcome, Outcome::PublicationUnknown);
        assert_eq!(
            sender.begin(&f.link.c, File::open(&source).unwrap(), "retry"),
            Err(Error::Closed)
        );
        drop(sender);
        super::super::clean(&mut f);
        assert_eq!(f.host.try_finish_cleanup(), Some(Ok(())));
        assert_eq!(f.host.last_result(), Some(published));
        assert_eq!(fs::read(f.path.join("delivered")).unwrap(), b"committed");
        assert!(!f.path.join("retry").exists());
        assert!(f.client.send_expired(&f.link.c));
        f.input_live(&cx);
        f.link.control_progress(&cx).await;
        assert!(!f.link.c.is_closed() && !f.link.h.is_closed());
    });
}

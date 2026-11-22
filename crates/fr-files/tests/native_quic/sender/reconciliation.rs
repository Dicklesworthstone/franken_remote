//! Actual source, UDP/TLS, ATP and disk publication with a delayed sender turn.
//! Only the sender's local clock is virtual; no file record or proof is invented.
use super::*;
use asupersync::{
    runtime::{Runtime, RuntimeBuilder},
    time::{TimerDriverHandle, VirtualClock},
    types::Budget,
};
use fr_files::sender::batch::{Selection, Stop};
use std::{io::Seek, sync::Arc};

const LIFETIME: Duration = Duration::from_secs(10);

fn sender_clock(cx: &Cx) -> (Runtime, Cx, Arc<VirtualClock>) {
    let clock = Arc::new(VirtualClock::starting_at(cx.timer_driver().unwrap().now()));
    let runtime = RuntimeBuilder::current_thread()
        .with_timer_driver(TimerDriverHandle::with_virtual_clock(Arc::clone(&clock)))
        .build()
        .unwrap();
    let context = runtime.request_cx_with_budget(Budget::INFINITE);
    (runtime, context, clock)
}

fn sample(clock: &VirtualClock, cx: &Cx) {
    clock.advance_to(cx.timer_driver().unwrap().now());
}

async fn awaiting_proof(
    sender: &mut Sender<'_>,
    host: &mut HostReceiver,
    link: &mut native_support::Link,
    cx: &Cx,
    virtual_clock: &VirtualClock,
) {
    let until = clock(cx) + 1_500_000;
    loop {
        assert!(clock(cx) < until, "source did not reach completion");
        sample(virtual_clock, cx);
        sender.service(&mut link.c, || true).unwrap();
        if sender.stage() == Stage::AwaitingProof {
            return;
        }
        host.service(&mut link.h, || true).unwrap();
        link.drive(cx).await;
        host.service(&mut link.h, || true).unwrap();
    }
}

async fn publication_queued(host: &mut HostReceiver, link: &mut native_support::Link, cx: &Cx) {
    let until = clock(cx) + 1_000_000;
    loop {
        assert!(
            clock(cx) < until,
            "original publication proof did not drain"
        );
        host.service(&mut link.h, || true).unwrap();
        link.drive(cx).await;
        host.service(&mut link.h, || true).unwrap();
        if matches!(
            host.last_result().map(|receipt| receipt.outcome),
            Some(Ok(fr_files::worker::Completion::Published(_)))
        ) && link.c.usage().retained_send_records == 0
            && link.h.usage().retained_send_records == 0
        {
            return;
        }
    }
}

fn expire(virtual_clock: &VirtualClock, cx: &Cx) {
    sample(virtual_clock, cx);
    virtual_clock.advance(u64::try_from(LIFETIME.as_nanos()).unwrap());
}

#[test]
fn transfer_expiry_retains_an_already_delivered_original_publication_proof() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut f = Running::new(&cx).await;
        let path = f.path.join("source");
        fs::write(&path, b"committed").unwrap();
        let (_runtime, sender_cx, virtual_clock) = sender_clock(&cx);
        let mut sender = Sender::new(
            sender_cx,
            &f.link.c,
            &mut f.client,
            Policy {
                transfer_lifetime: LIFETIME,
                ..Policy::default()
            },
        )
        .unwrap();
        sender
            .begin(&f.link.c, File::open(&path).unwrap(), "delivered")
            .unwrap();
        awaiting_proof(&mut sender, &mut f.host, &mut f.link, &cx, &virtual_clock).await;
        publication_queued(&mut f.host, &mut f.link, &cx).await;
        assert_eq!(fs::read(f.path.join("delivered")).unwrap(), b"committed");
        assert!(sender.result().is_none());

        // The original proof arrived and was ACKed, but the application did
        // not dispatch it before its own transfer deadline. Native transport
        // still owns the exact bytes; no expired stream is revived.
        expire(&virtual_clock, &cx);
        assert_eq!(sender.service(&mut f.link.c, || true), Err(Error::Expired));
        assert_eq!(
            cleanup(&mut sender),
            Receipt {
                id: 1,
                outcome: Outcome::HostPublished {
                    bytes: 9,
                    publication: Publication::Durable,
                },
            }
        );
        assert_eq!(
            sender.begin(&f.link.c, File::open(&path).unwrap(), "retry"),
            Err(Error::Closed)
        );
        assert!(!f.path.join("retry").exists());
        assert!(!f.link.c.is_closed());
        drop(sender);
        f.input_live(&cx);
    });
}

#[test]
fn batch_expiry_retains_queued_proof_without_starting_the_next_selection() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut f = Running::new(&cx).await;
        let path = f.path.join("source");
        fs::write(&path, b"committed").unwrap();
        let (_runtime, sender_cx, virtual_clock) = sender_clock(&cx);
        let mut sender =
            Sender::new(sender_cx, &f.link.c, &mut f.client, Policy::default()).unwrap();
        let mut selection = Selection::new();
        for name in ["delivered", "must-not-start"] {
            selection.push(File::open(&path).unwrap(), name).unwrap();
        }
        sender.begin_batch(&f.link.c, selection, LIFETIME).unwrap();
        awaiting_proof(&mut sender, &mut f.host, &mut f.link, &cx, &virtual_clock).await;
        publication_queued(&mut f.host, &mut f.link, &cx).await;

        expire(&virtual_clock, &cx);
        assert_eq!(sender.service(&mut f.link.c, || true), Err(Error::Expired));
        let until = Instant::now() + Duration::from_secs(2);
        while !sender.batch_report().unwrap().complete {
            assert!(Instant::now() < until, "original source cleanup stalled");
            let _ = sender.try_finish_cleanup();
            std::thread::yield_now();
        }
        let report = sender.take_batch_report().unwrap();
        assert_eq!((report.started, report.not_started()), (1, 1));
        assert_eq!(report.stop, Some(Stop::Local(Error::Expired)));
        assert_eq!(
            report.receipts().collect::<Vec<_>>(),
            [Receipt {
                id: 1,
                outcome: Outcome::HostPublished {
                    bytes: 9,
                    publication: Publication::Durable,
                },
            }]
        );
        assert_eq!(fs::read(f.path.join("delivered")).unwrap(), b"committed");
        assert!(!f.path.join("must-not-start").exists());
        assert!(!f.link.c.is_closed());
        drop(sender);
        f.input_live(&cx);
    });
}

#[test]
fn acceptance_delivered_before_expiry_cannot_start_a_late_source_read_or_send() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut f = Running::new(&cx).await;
        let path = f.path.join("source");
        fs::write(&path, b"private-payload").unwrap();
        let selected = File::open(&path).unwrap();
        // A duplicate shares the selected descriptor's position. Preparation
        // hashes and rewinds it; a subsequent payload read would move it again.
        let mut position = selected.try_clone().unwrap();
        let (_runtime, sender_cx, virtual_clock) = sender_clock(&cx);
        let mut sender = Sender::new(
            sender_cx,
            &f.link.c,
            &mut f.client,
            Policy {
                transfer_lifetime: LIFETIME,
                ..Policy::default()
            },
        )
        .unwrap();
        sender
            .begin(&f.link.c, selected, "must-not-publish")
            .unwrap();
        let until = clock(&cx) + 1_500_000;
        while sender.stage() != Stage::AwaitingAcceptance {
            assert!(clock(&cx) < until, "original offer was not admitted");
            sample(&virtual_clock, &cx);
            sender.service(&mut f.link.c, || true).unwrap();
            f.link.drive(&cx).await;
        }
        let mut acceptance_admitted = false;
        loop {
            assert!(clock(&cx) < until, "original acceptance did not drain");
            f.host.service(&mut f.link.h, || true).unwrap();
            acceptance_admitted |=
                f.host.progress().is_some() && f.link.h.usage().retained_send_records > 0;
            f.link.drive(&cx).await;
            if acceptance_admitted
                && f.link.c.usage().retained_send_records == 0
                && f.link.h.usage().retained_send_records == 0
            {
                break;
            }
        }
        assert_eq!(position.stream_position().unwrap(), 0);
        assert_eq!(sender.progress().unwrap().queued_bytes, 0);
        expire(&virtual_clock, &cx);
        assert_eq!(sender.service(&mut f.link.c, || true), Err(Error::Expired));
        assert_eq!(sender.progress().unwrap().queued_bytes, 0);
        assert_eq!(
            cleanup(&mut sender).outcome,
            Outcome::InterruptedBeforePublication(Error::Expired)
        );
        assert_eq!(position.stream_position().unwrap(), 0);
        assert_eq!(f.link.c.usage().retained_send_records, 0);
        assert!(!f.path.join("must-not-publish").exists());
        assert!(!f.link.c.is_closed());
        drop(sender);
        f.input_live(&cx);
    });
}

#[test]
fn lost_authorization_does_not_admit_a_terminal_reply_at_expiry() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut f = Running::new(&cx).await;
        let path = f.path.join("source");
        fs::write(&path, b"committed").unwrap();
        let (_runtime, sender_cx, virtual_clock) = sender_clock(&cx);
        let mut sender = Sender::new(
            sender_cx,
            &f.link.c,
            &mut f.client,
            Policy {
                transfer_lifetime: LIFETIME,
                ..Policy::default()
            },
        )
        .unwrap();
        sender
            .begin(&f.link.c, File::open(&path).unwrap(), "delivered")
            .unwrap();
        awaiting_proof(&mut sender, &mut f.host, &mut f.link, &cx, &virtual_clock).await;
        publication_queued(&mut f.host, &mut f.link, &cx).await;
        expire(&virtual_clock, &cx);
        assert_eq!(
            sender.service(&mut f.link.c, || false),
            Err(Error::Cancelled)
        );
        assert_eq!(cleanup(&mut sender).outcome, Outcome::PublicationUnknown);
        assert_eq!(fs::read(f.path.join("delivered")).unwrap(), b"committed");
        assert!(!f.link.c.is_closed());
        drop(sender);
        f.input_live(&cx);
    });
}

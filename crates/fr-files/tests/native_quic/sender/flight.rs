//! Real source/UDP/TLS/ATP/disk composition. Withholding both native drives
//! proves the queued content cannot have been acknowledged by the peer.
use super::*;

fn bulk_flight(link: &native_support::Link) -> (usize, usize) {
    let usage = link.c.usage();
    // Usage includes the separately reserved critical lane. Only the original
    // bulk sender's retained bytes/records prove custody of this file flight.
    (
        usage
            .retained_send_upper_bound
            .checked_sub(usage.critical_send_bytes)
            .expect("critical bytes belong to total retained custody"),
        usage
            .retained_send_records
            .checked_sub(usage.critical_send_records)
            .expect("critical records belong to total retained custody"),
    )
}

async fn accepted_without_payload(
    sender: &mut Sender<'_>,
    host: &mut HostReceiver,
    link: &mut native_support::Link,
    cx: &Cx,
) {
    let until = clock(cx) + 1_000_000;
    loop {
        assert!(clock(cx) < until, "file acceptance stalled");
        sender.service(&mut link.c, || true).unwrap();
        if sender.stage() == Stage::Streaming {
            break;
        }
        host.service(&mut link.h, || true).unwrap();
        link.drive(cx).await;
        host.service(&mut link.h, || true).unwrap();
    }
    // Flush only the admitted offer. Pausing does not cancel a pending source
    // read, but neither it nor the final emptying drive may admit payload.
    sender.set_paused(&link.c, true).unwrap();
    while link.c.usage().retained_send_records != 0 {
        assert!(clock(cx) < until);
        step(sender, host, link, cx).await;
    }
    sender.service(&mut link.c, || true).unwrap();
    assert_eq!(sender.progress().unwrap().queued_bytes, 0);
    sender.set_paused(&link.c, false).unwrap();
}

fn fill_without_network(sender: &mut Sender<'_>, link: &mut native_support::Link, cx: &Cx) {
    let until = clock(cx) + 300_000;
    while bulk_flight(link).1 < 8 {
        assert!(clock(cx) < until, "sender still waits for each chunk's ACK");
        sender.service(&mut link.c, || true).unwrap();
        let (bytes, records) = bulk_flight(link);
        assert!(records <= 8);
        assert!(bytes <= 128 * 1024);
        std::thread::yield_now();
    }
}

#[test]
fn eight_chunks_can_wait_for_acks_then_publish_once_on_the_original_lane() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut f = Running::new(&cx).await;
        let path = f.path.join("source");
        let data: Vec<u8> = (0..120_007)
            .map(|i| u8::try_from(i % 251).unwrap())
            .collect();
        fs::write(&path, &data).unwrap();
        let record_bytes = f.client.limits().record_bytes();
        let mut sender =
            Sender::new(cx.clone(), &f.link.c, &mut f.client, Policy::default()).unwrap();
        let id = sender
            .begin(&f.link.c, File::open(&path).unwrap(), "received")
            .unwrap();
        accepted_without_payload(&mut sender, &mut f.host, &mut f.link, &cx).await;
        fill_without_network(&mut sender, &mut f.link, &cx);
        let filled = sender.progress().unwrap();
        assert!(filled.queued_bytes > record_bytes as u64);
        assert!(filled.queued_bytes < data.len() as u64);
        assert!(bulk_flight(&f.link).0 > usize::try_from(filled.queued_bytes).unwrap());
        assert_eq!(sender.stage(), Stage::Streaming);
        assert!(!f.path.join("received").exists());

        // A full flight cannot grow by further producer turns or a pause toggle.
        for paused in [false, true, false] {
            sender.set_paused(&f.link.c, paused).unwrap();
            for _ in 0..16 {
                sender.service(&mut f.link.c, || true).unwrap();
                assert_eq!(sender.progress(), Some(filled));
                assert_eq!(bulk_flight(&f.link).1, 8);
            }
        }
        let receipt = result(&mut sender, &mut f.host, &mut f.link, &cx).await;
        assert_eq!(
            receipt,
            Receipt {
                id,
                outcome: Outcome::HostPublished {
                    bytes: data.len() as u64,
                    publication: Publication::Durable,
                },
            }
        );
        assert_eq!(fs::read(f.path.join("received")).unwrap(), data);
        assert_eq!(sender.stage(), Stage::Idle);
        assert!(!f.link.c.is_closed() && !f.link.h.is_closed());
        drop(sender);
        f.input_live(&cx);
    });
}

#[test]
fn verified_source_eof_cannot_commit_before_its_content_drains() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut f = Running::new(&cx).await;
        let path = f.path.join("source");
        fs::write(&path, b"verified").unwrap();
        let mut sender =
            Sender::new(cx.clone(), &f.link.c, &mut f.client, Policy::default()).unwrap();
        sender
            .begin(&f.link.c, File::open(&path).unwrap(), "received")
            .unwrap();
        accepted_without_payload(&mut sender, &mut f.host, &mut f.link, &cx).await;
        let until = clock(&cx) + 300_000;
        while !sender.cleanup_finished() {
            assert!(
                clock(&cx) < until,
                "original source never reached verified EOF"
            );
            sender.service(&mut f.link.c, || true).unwrap();
            std::thread::yield_now();
        }
        for _ in 0..8 {
            sender.service(&mut f.link.c, || true).unwrap();
            assert_eq!(sender.progress().unwrap().queued_bytes, 8);
            assert_eq!(sender.stage(), Stage::Streaming);
            assert_eq!(bulk_flight(&f.link).1, 1);
            assert!(sender.result().is_none());
        }
        assert!(!f.path.join("received").exists());
        assert_eq!(
            result(&mut sender, &mut f.host, &mut f.link, &cx)
                .await
                .outcome,
            Outcome::HostPublished {
                bytes: 8,
                publication: Publication::Durable,
            }
        );
        assert_eq!(fs::read(f.path.join("received")).unwrap(), b"verified");
    });
}

#[test]
fn authority_loss_fences_a_full_flight_without_admitting_completion() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut f = Running::new(&cx).await;
        let path = f.path.join("source");
        fs::write(&path, vec![0x39; 120_007]).unwrap();
        let mut sender =
            Sender::new(cx.clone(), &f.link.c, &mut f.client, Policy::default()).unwrap();
        sender
            .begin(&f.link.c, File::open(&path).unwrap(), "never")
            .unwrap();
        accepted_without_payload(&mut sender, &mut f.host, &mut f.link, &cx).await;
        fill_without_network(&mut sender, &mut f.link, &cx);
        let queued = sender.progress().unwrap().queued_bytes;
        assert_eq!(
            sender.service(&mut f.link.c, || false),
            Err(Error::Cancelled)
        );
        assert_eq!(sender.progress().unwrap().queued_bytes, queued);
        assert_eq!(
            cleanup(&mut sender).outcome,
            Outcome::InterruptedBeforePublication(Error::Cancelled)
        );
        assert_eq!(
            sender.begin(&f.link.c, File::open(path).unwrap(), "retry"),
            Err(Error::Closed)
        );
        assert!(!f.path.join("never").exists());
        assert!(!f.link.c.is_closed() && !f.link.h.is_closed());
        drop(sender);
        f.input_live(&cx);
    });
}

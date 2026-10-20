//! Real record admission and UDP/TLS drain; publication is an explicit probe.
//! This does not qualify WAN throughput, a live tailnet, or native OS clipboard.
use super::*;
use fr_core::clipboard::{ClipboardSink, MAX_CHUNK_BYTES, Publication, Stamp};
use fr_wire::clipboard::{CHUNK_OVERHEAD, session::Offer};

fn inflight(f: &Fixture) -> usize {
    f.bridge.inflight.iter().flatten().count()
}

fn offer_text(f: &mut Fixture, id: u128, text: &str) -> (Stamp, HostInstant) {
    let now = HostInstant::from_micros(clock(&f.cx));
    let owner = channel(&mut f.seed);
    let Offer::Queued(stamp) = owner.offer(id, text, None, now).unwrap() else {
        panic!("a fresh local item must be queued");
    };
    (stamp, owner.outgoing_deadline().unwrap())
}

fn retained_until(f: &Fixture, until: HostInstant) {
    for (permit, local_until) in f.bridge.inflight.iter().flatten() {
        assert_eq!(permit.deadline(), until);
        // This is the host bridge's actual timer domain, so conversion cannot
        // add lifetime. Every accepted native record retains that same bound.
        assert_eq!(*local_until, until.as_micros());
    }
}

async fn drain(f: &mut Fixture, expected: usize) -> Vec<Vec<u8>> {
    let until = clock(&f.cx) + 2_000_000;
    let mut records = Vec::new();
    while !f.bridge.lane.send_drained(&f.link.h).unwrap()
        || records.len() < expected
        || f.link.h.usage().critical_send_records != 0
        || f.link.c.usage().critical_send_records != 0
    {
        assert!(
            clock(&f.cx) < until,
            "the original native lane did not drain"
        );
        f.link.drive(&f.cx).await;
        f.peer
            .dispatch(
                &mut f.link.c,
                &f.cx,
                || true,
                |bytes| {
                    records.push(bytes.to_vec());
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
    }
    assert_eq!(records.len(), expected);
    records
}

fn assert_chunk(f: &Fixture, bytes: &[u8], index: u32) {
    let message = decode(bytes, f.bridge.lane.outgoing(), &f.bridge.lane.limits()).unwrap();
    let Body::Chunk {
        index: actual,
        offset,
        bytes,
    } = message.body
    else {
        panic!("expected the original chunk, never Commit");
    };
    assert_eq!(actual, index);
    assert_eq!(offset as usize, index as usize * MAX_CHUNK_BYTES);
    assert_eq!(bytes.len(), MAX_CHUNK_BYTES);
}

#[test]
fn eight_records_keep_original_permits_until_the_actual_native_lane_drains() {
    runtime().block_on(async {
        let mut f = Fixture::with_bulk_records(true).await;
        drain(&mut f, 0).await;
        let (_, until) = offer_text(&mut f, 701, &"a".repeat(9 * MAX_CHUNK_BYTES));
        assert!(f.bridge.lane.limits().max_control_message_bytes() as usize > MAX_CHUNK_BYTES);
        for accepted in 1..=8 {
            assert_eq!(f.pump(), Pump::RecordAccepted);
            assert_eq!(f.bridge.service(&mut f.link.h, || true), Ok(State::Active));
            assert_eq!(inflight(&f), accepted);
            assert_eq!(f.link.h.usage().retained_send_records, accepted);
            assert!(f.link.h.usage().retained_send_upper_bound <= 128 * 1024);
            retained_until(&f, until);
        }
        // The ninth record can occupy the ONE worker handoff, but no native
        // admission or another worker record can bypass the eight live permits.
        assert_eq!(f.pump(), Pump::RecordAccepted);
        let before = f.link.h.usage();
        assert_eq!(f.bridge.service(&mut f.link.h, || true), Ok(State::Active));
        assert_eq!(f.link.h.usage(), before);
        assert_eq!(inflight(&f), 8);
        assert_eq!(f.pump(), Pump::Backpressure);
        assert!(f.bridge.shared.outbox.lock().unwrap().busy);
        assert!(f.bridge.pending.is_none());

        let records = drain(&mut f, 8).await;
        for (index, record) in records.iter().skip(1).enumerate() {
            assert_chunk(&f, record, u32::try_from(index).unwrap());
        }
        // An application receipt or successful flush did not release these.
        // Maintenance now consumes the actual native empty-send witness.
        assert_eq!(inflight(&f), 8);
        retained_until(&f, until);
        assert_eq!(f.bridge.maintain(&mut f.link.h), Ok(State::Active));
        assert_eq!(inflight(&f), 0);
        assert_eq!(channel(&mut f.seed).outgoing_deadline(), Some(until));
        f.stop();
    });
}

#[derive(Default)]
struct PublicationProbe(Vec<String>);
impl ClipboardSink for PublicationProbe {
    fn prepare(&mut self, _: &str, _: Stamp) -> Result<(), PlatformError> {
        Ok(())
    }
    fn publish(&mut self, text: &str, _: Stamp) -> Publication {
        self.0.push(text.to_owned());
        Publication::SubmittedToOs
    }
}

#[test]
#[allow(clippy::too_many_lines)] // Both outcomes retain the same real blocked final Chunk.
fn the_existing_byte_ceiling_blocks_the_final_chunk_without_releasing_commit() {
    runtime().block_on(async {
        for supersede in [false, true] {
            let mut f = Fixture::with_bulk_records(true).await;
            drain(&mut f, 0).await;
            let mut input = authority::viewer(&f.cx, f.grant);
            let mut receiver = input
                .attach_clipboard_lane(
                    f.peer.parent(),
                    f.peer.outgoing(),
                    f.peer.limits(),
                    true,
                    ClientInstant(clock(&f.cx)),
                )
                .unwrap();
            let mut publication = PublicationProbe::default();
            let text = "b".repeat(8 * MAX_CHUNK_BYTES);
            let (stamp, until) = offer_text(&mut f, 801, &text);
            assert_eq!(f.pump(), Pump::RecordAccepted);
            f.bridge.service(&mut f.link.h, || true).unwrap();
            for begin in drain(&mut f, 1).await {
                assert!(
                    receiver
                        .receive(&begin, &mut publication, || ClientInstant(clock(&f.cx)))
                        .unwrap()
                        .is_none()
                );
            }
            f.bridge.maintain(&mut f.link.h).unwrap();
            assert_eq!(inflight(&f), 0);

            let maximum = quic::Policy::default().retained_send_bytes;
            assert_eq!(maximum, 128 * 1024);
            let chunk_record_bytes = MAX_CHUNK_BYTES + CHUNK_OVERHEAD;
            assert_eq!(maximum / chunk_record_bytes, 7);
            for accepted in 1..=7 {
                assert_eq!(f.pump(), Pump::RecordAccepted);
                assert_eq!(f.bridge.service(&mut f.link.h, || true), Ok(State::Active));
                assert_eq!(inflight(&f), accepted);
            }
            let before = f.link.h.usage();
            assert_eq!(before.retained_send_upper_bound, 7 * chunk_record_bytes);
            assert!(before.retained_send_upper_bound + chunk_record_bytes > maximum);
            assert_eq!(f.pump(), Pump::RecordAccepted);
            assert_eq!(f.bridge.service(&mut f.link.h, || true), Ok(State::Active));
            assert_eq!(
                inflight(&f),
                7,
                "byte credit wins with one permit still free"
            );
            assert_eq!(f.link.h.usage(), before);
            let pending = f.bridge.pending.as_ref().unwrap();
            assert_chunk(&f, &pending.bytes.0, 7);
            assert_eq!(pending.permit.deadline(), until);
            let unchanged = pending.bytes.0.clone();
            assert!(f.bridge.shared.outbox.lock().unwrap().busy);
            // The wire Sender has prepared its final Chunk. Its next record is
            // Commit, but the unadmitted Chunk still owns this single busy slot.
            assert_eq!(f.pump(), Pump::Backpressure);
            assert_eq!(f.bridge.service(&mut f.link.h, || true), Ok(State::Active));
            assert_eq!(f.bridge.pending.as_ref().unwrap().bytes.0, unchanged);
            retained_until(&f, until);

            for (index, record) in drain(&mut f, 7).await.into_iter().enumerate() {
                assert_chunk(&f, &record, u32::try_from(index).unwrap());
                assert!(
                    receiver
                        .receive(&record, &mut publication, || ClientInstant(clock(&f.cx)))
                        .unwrap()
                        .is_none()
                );
            }
            assert_eq!(publication.0, Vec::<String>::new());
            assert_eq!(inflight(&f), 7);
            if supersede {
                offer_text(&mut f, 802, "a newer selection");
                assert_eq!(f.bridge.maintain(&mut f.link.h), Ok(State::Active));
                assert!(f.bridge.pending.is_none());
                assert!(!f.bridge.shared.outbox.lock().unwrap().busy);
                assert_eq!(inflight(&f), 0);
                assert_eq!(f.pump(), Pump::CancelAccepted(stamp));
                f.bridge.service(&mut f.link.h, || true).unwrap();
                for cancel in drain(&mut f, 1).await {
                    let message =
                        decode(&cancel, f.bridge.lane.outgoing(), &f.bridge.lane.limits()).unwrap();
                    assert_eq!(message.stamp, stamp);
                    assert_eq!(message.body, Body::Cancel(CancelReason::Superseded));
                    assert!(
                        receiver
                            .receive(&cancel, &mut publication, || ClientInstant(clock(&f.cx)))
                            .unwrap()
                            .is_none()
                    );
                }
                assert!(
                    publication.0.is_empty(),
                    "the discarded chunk cannot become Commit"
                );
            } else {
                f.bridge.service(&mut f.link.h, || true).unwrap();
                assert!(f.bridge.pending.is_none());
                assert_eq!(inflight(&f), 1);
                assert_eq!(f.pump(), Pump::ItemAccepted(stamp));
                f.bridge.service(&mut f.link.h, || true).unwrap();
                assert_eq!(inflight(&f), 2);
                retained_until(&f, until);
                let records = drain(&mut f, 2).await;
                assert_chunk(&f, &records[0], 7);
                assert!(
                    receiver
                        .receive(&records[0], &mut publication, || ClientInstant(clock(
                            &f.cx
                        )))
                        .unwrap()
                        .is_none()
                );
                assert_eq!(publication.0, Vec::<String>::new());
                let receipt = receiver
                    .receive(&records[1], &mut publication, || {
                        ClientInstant(clock(&f.cx))
                    })
                    .unwrap()
                    .unwrap();
                assert_eq!(receipt.stamp, stamp);
                assert_eq!(receipt.publication, Publication::SubmittedToOs);
                assert_eq!(publication.0, [text]);
            }
            f.stop();
        }
    });
}

#[test]
fn expiry_of_an_eight_record_batch_retires_original_permits_without_resurrection() {
    runtime().block_on(async {
        let mut f = Fixture::with_bulk_records(true).await;
        drain(&mut f, 0).await;
        let (_, until) = offer_text(&mut f, 901, &"c".repeat(9 * MAX_CHUNK_BYTES));
        for _ in 0..8 {
            assert_eq!(f.pump(), Pump::RecordAccepted);
            assert_eq!(f.bridge.service(&mut f.link.h, || true), Ok(State::Active));
        }
        assert_eq!(f.pump(), Pump::RecordAccepted);
        assert_eq!(inflight(&f), 8);
        retained_until(&f, until);
        asupersync::time::sleep_until(asupersync::types::Time::from_nanos(
            until.as_micros().checked_mul(1000).unwrap(),
        ))
        .await;
        assert!(clock(&f.cx) >= until.as_micros());
        assert_eq!(f.bridge.maintain(&mut f.link.h), Ok(State::Retired));
        assert_eq!(
            f.bridge.reason(),
            Some(Error::Session(SessionError::Clipboard(
                fr_core::clipboard::Error::Expired
            )))
        );
        assert_eq!(inflight(&f), 0);
        assert!(f.bridge.pending.is_none());
        assert!(f.bridge.shared.outbox.lock().unwrap().item.is_none());
        assert_eq!(f.link.h.usage().retained_send_upper_bound, 0);
        assert_eq!(f.link.h.usage().retained_send_records, 0);
        assert!(
            !f.link.h.is_closed(),
            "only the optional clipboard lane retires"
        );
        assert!(!f.bridge.shared.gate.is_open());
        let now = HostInstant::from_micros(clock(&f.cx));
        assert!(
            channel(&mut f.seed)
                .offer(902, "too late", None, now)
                .is_err()
        );
        assert_eq!(channel(&mut f.seed).retained_bytes(), 0);
        assert_eq!(f.bridge.service(&mut f.link.h, || true), Ok(State::Retired));
        assert_eq!(inflight(&f), 0);
        f.stop();
    });
}

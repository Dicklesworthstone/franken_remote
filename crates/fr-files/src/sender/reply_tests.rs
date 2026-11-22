//! Reply-handler coverage of both directions and durability states at expiry.
//! The source reaches AwaitingProof on the real upload lane; reply bytes below
//! are protocol fixtures, not claims of a second real filesystem publication.
#[allow(dead_code)]
#[path = "../../tests/native_support/mod.rs"]
mod native_support;

use super::*;
use native_support::{Running, clock, runtime};
use std::{fs, time::Instant};

fn completion(
    context: files::Context,
    limits: files::Limits,
    id: u64,
    disposition: Disposition,
    sha_ok: bool,
) -> Vec<u8> {
    let proof = ReceiveReceipt {
        committed: true,
        bytes_received: 9,
        files: 1,
        sha_ok,
        merkle_ok: true,
        symbols_accepted: 0,
        feedback_rounds: 0,
        decode_count: 0,
        decode_micros: 0,
        reason: None,
        committed_paths: Vec::new(),
    };
    let proof = Frame::new(
        ProtocolVersion::CURRENT,
        FrameType::Proof,
        serde_json::to_vec(&proof).unwrap(),
    )
    .unwrap()
    .to_wire_bytes()
    .unwrap();
    let mut bytes = vec![0; limits.record_bytes()];
    let length = files::encode(
        Message {
            id,
            body: Body::Complete {
                disposition,
                reason: Reason::None,
                published_bytes: 9,
                atp: &proof,
            },
        },
        context,
        limits,
        &mut bytes,
    )
    .unwrap();
    bytes.truncate(length);
    bytes
}

#[test]
fn expired_reply_keeps_original_identity_direction_and_durability() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut fixture = Running::new(&cx).await;
        let path = fixture.path.join("source");
        fs::write(&path, b"committed").unwrap();
        let mut sender = Sender::new(
            cx.clone(),
            &fixture.link.c,
            &mut fixture.client,
            Policy::default(),
        )
        .unwrap();
        let id = sender
            .begin(&fixture.link.c, File::open(&path).unwrap(), "delivered")
            .unwrap();
        let until = clock(&cx) + 1_500_000;
        while sender.stage() != Stage::AwaitingProof {
            assert!(clock(&cx) < until, "original source did not finish");
            sender.service(&mut fixture.link.c, || true).unwrap();
            if sender.stage() != Stage::AwaitingProof {
                fixture.host.service(&mut fixture.link.h, || true).unwrap();
                fixture.link.drive(&cx).await;
            }
        }
        let limits = sender.lane.limits();
        let incoming = sender.lane.incoming();
        let transfer = sender.transfer.as_mut().unwrap();
        let deadline = transfer.deadline;
        for direction in [Direction::ToHost, Direction::ToController] {
            let context = files::Context {
                direction,
                sender: direction.receiver(),
                ..incoming
            };
            for (disposition, publication) in [
                (Disposition::PublishedDurable, Publication::Durable),
                (
                    Disposition::PublishedDurabilityUnknown,
                    Publication::DurabilityUnknown,
                ),
            ] {
                transfer.stage = Stage::AwaitingProof;
                transfer.result = None;
                // Expiry does not weaken original-ID or ATP proof validation.
                for (reply_id, sha_ok) in [(id + 1, true), (id, false)] {
                    let bytes = completion(context, limits, reply_id, disposition, sha_ok);
                    assert_eq!(
                        transfer.reply(&bytes, context, limits, deadline),
                        Err(Error::Protocol)
                    );
                    assert!(transfer.result.is_none());
                }
                let bytes = completion(context, limits, id, disposition, true);
                transfer.reply(&bytes, context, limits, deadline).unwrap();
                let outcome = match direction {
                    Direction::ToHost => Outcome::HostPublished {
                        bytes: 9,
                        publication,
                    },
                    Direction::ToController => Outcome::ControllerPublished {
                        bytes: 9,
                        publication,
                    },
                };
                assert_eq!(transfer.result, Some(Receipt { id, outcome }));
                assert_eq!(transfer.deadline, deadline);
                assert_eq!(
                    transfer.reply(&bytes, context, limits, deadline),
                    Err(Error::Protocol)
                );
                assert_eq!(transfer.result, Some(Receipt { id, outcome }));
            }
        }
        sender.cancel(&mut fixture.link.c).unwrap();
        let until = Instant::now() + Duration::from_secs(2);
        while sender.try_finish_cleanup().is_none() {
            assert!(Instant::now() < until, "original source cleanup stalled");
            std::thread::yield_now();
        }
    });
}

#![cfg(target_os = "linux")]
//! Real UDP/TLS and disk owners with explicit authority fixtures, not a tailnet.
#[allow(dead_code)]
mod native_support;

use asupersync::cx::Cx;
use fr_files::{quic::State, worker::Completion};
use fr_wire::files::{self, Body, Disposition, Reason};
use native_support::{Running, clock, data, manifest, offered, runtime};
use std::fs;

fn assert_refused(f: &Running, bytes: &[u8], id: u64) {
    let message = files::decode(bytes, f.client.incoming(), f.client.limits()).unwrap();
    assert_eq!(message.id, id);
    assert_eq!(
        message.body,
        Body::Complete {
            disposition: Disposition::Refused,
            reason: Reason::Invalid,
            published_bytes: 0,
            atp: &[],
        }
    );
}

async fn drain(f: &mut Running, cx: &Cx) {
    let until = clock(cx) + 1_000_000;
    while f.host.state() != State::Retired || !f.host.cleanup_finished() {
        assert!(clock(cx) < until, "refusal cleanup did not finish");
        f.pump(cx).await;
    }
    assert_eq!(f.host.try_finish_cleanup(), Some(Ok(())));
    f.input_live(cx);
    assert!(!f.link.c.is_closed() && !f.link.h.is_closed());
}

async fn publish(f: &mut Running, cx: &Cx) {
    let offer = offered(&manifest("received.bin", b"committed"));
    f.send(
        cx,
        Body::Offer {
            profile: files::ATP_PORTABLE_FULL,
            atp: &offer,
        },
    )
    .await;
    let accepted = f.reply(cx).await;
    assert!(matches!(
        files::decode(&accepted, f.client.incoming(), f.client.limits())
            .unwrap()
            .body,
        Body::Accept { .. }
    ));
    f.send(
        cx,
        Body::Chunk {
            atp: &data(0, 0, b"committed"),
        },
    )
    .await;
    f.staged(cx, 9).await;
    f.send(
        cx,
        Body::Chunk {
            atp: &fr_files::atp::encode_complete().unwrap(),
        },
    )
    .await;
    let complete = f.reply(cx).await;
    assert!(matches!(
        files::decode(&complete, f.client.incoming(), f.client.limits())
            .unwrap()
            .body,
        Body::Complete {
            disposition: Disposition::PublishedDurable,
            published_bytes: 9,
            ..
        }
    ));
}

#[test]
fn malformed_offer_gets_a_terminal_refusal_without_a_fabricated_disk_receipt() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut f = Running::new(&cx).await;
        f.send(
            &cx,
            Body::Offer {
                profile: files::ATP_PORTABLE_FULL,
                atp: b"invalid ATP frame",
            },
        )
        .await;
        let reply = f.reply(&cx).await;
        assert_refused(&f, &reply, 1);
        drain(&mut f, &cx).await;
        assert!(f.host.last_result().is_none());
        assert!(f.host.progress().is_none());
        assert_eq!(fs::read_dir(&f.path).unwrap().count(), 0);
    });
}

#[test]
fn profile_mismatch_refuses_before_staging_without_silent_downgrade() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut f = Running::new(&cx).await;
        let offer = offered(&manifest("received.bin", b"not a directory"));
        f.send(
            &cx,
            Body::Offer {
                profile: files::ATP_PORTABLE_DIRECTORY_FULL,
                atp: &offer,
            },
        )
        .await;
        let reply = f.reply(&cx).await;
        assert_refused(&f, &reply, 1);
        drain(&mut f, &cx).await;
        assert!(f.host.last_result().is_none());
        assert_eq!(fs::read_dir(&f.path).unwrap().count(), 0);
    });
}

#[test]
fn a_new_offer_refusal_preserves_the_previous_published_receipt() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut f = Running::new(&cx).await;
        publish(&mut f, &cx).await;
        let published = f.host.last_result().unwrap();
        assert_eq!(published.id, 1);
        assert!(matches!(published.outcome, Ok(Completion::Published(_))));
        let bytes = f.packet(
            2,
            Body::Offer {
                profile: files::ATP_PORTABLE_FULL,
                atp: b"invalid next offer",
            },
        );
        f.client
            .send(&mut f.link.c, &cx, &bytes, clock(&cx) + 1_000_000, || true)
            .unwrap();
        let reply = f.reply(&cx).await;
        assert_refused(&f, &reply, 2);
        drain(&mut f, &cx).await;
        assert_eq!(f.host.last_result(), Some(published));
        assert_eq!(fs::read(f.path.join("received.bin")).unwrap(), b"committed");
    });
}

#[test]
fn replayed_offer_cannot_replace_publication_with_a_no_effect_reply() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut f = Running::new(&cx).await;
        publish(&mut f, &cx).await;
        let published = f.host.last_result().unwrap();
        f.send(
            &cx,
            Body::Offer {
                profile: files::ATP_PORTABLE_FULL,
                atp: b"invalid replay",
            },
        )
        .await;
        let until = clock(&cx) + 1_000_000;
        loop {
            assert!(clock(&cx) < until);
            f.link.drive(&cx).await;
            if let Err(error) = f.host.service(&mut f.link.h, || true) {
                assert!(matches!(error, fr_files::quic::Error::File(_)));
                break;
            }
        }
        assert_eq!(f.host.state(), State::Retired);
        assert_eq!(f.host.last_result(), Some(published));
        assert_eq!(fs::read(f.path.join("received.bin")).unwrap(), b"committed");
        f.input_live(&cx);
        assert!(!f.link.c.is_closed() && !f.link.h.is_closed());
    });
}

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

// Exercise serialization failures directly: the native transport deliberately
// supplies a full-sized buffer. Reuse its original input owner and negotiated
// bindings, but give this wire-only receiver a separate, private test directory.
fn wire_receiver(
    f: &Running,
    cx: &Cx,
) -> (fr_files::wire::HostReceiver, fr_files::worker::Task) {
    use std::os::unix::fs::DirBuilderExt;
    let directory = f.path.join("wire-only");
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&directory)
        .unwrap();
    let directory = fr_files::receive::DropDirectory::open(
        &directory,
        fr_files::receive::Limits {
            max_file_bytes: 1_000_000,
            max_reserved_bytes: 1_000_000,
            max_transfers: 1,
        },
    )
    .unwrap();
    fr_files::wire::HostReceiver::spawn(
        cx.clone(),
        &f.input,
        directory,
        fr_files::session::Permission::new(true),
        fr_files::session::Policy::conservative(),
        fr_files::wire::Settings {
            incoming: f.client.outgoing(),
            outgoing: f.client.incoming(),
            limits: f.client.limits(),
        },
    )
    .unwrap()
}

fn finish_wire_worker(task: &mut fr_files::worker::Task) {
    let until = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        if let Some(result) = task.try_finish_cleanup() {
            assert_eq!(result, Ok(()));
            break;
        }
        assert!(std::time::Instant::now() < until, "wire worker cleanup stalled");
        std::thread::yield_now();
    }
}

#[test]
fn short_buffers_and_following_cancellation_cannot_consume_the_retained_refusal() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let f = Running::new(&cx).await;
        let (mut receiver, mut task) = wire_receiver(&f, &cx);
        let offer = f.packet(
            1,
            Body::Offer {
                profile: files::ATP_PORTABLE_FULL,
                atp: b"invalid ATP frame",
            },
        );
        assert!(matches!(
            receiver.receive(&offer),
            Err(fr_files::wire::Error::Atp(_))
        ));
        assert!(receiver.is_closed());
        for length in [0, files::COMMON_BYTES, files::COMPLETE_OVERHEAD - 1] {
            assert!(matches!(
                receiver.poll_reply(&mut vec![0; length]),
                Err(fr_files::wire::Error::Wire(_))
            ));
            assert!(receiver.is_busy());
            assert!(receiver.last_result().is_none());
        }
        let cancel = f.packet(1, Body::Cancel(Reason::User));
        assert_eq!(receiver.receive(&cancel), Err(fr_files::wire::Error::Busy));
        assert_eq!(receiver.receive(&offer), Err(fr_files::wire::Error::Busy));
        let mut reply = vec![0; f.client.limits().record_bytes()];
        let length = receiver.poll_reply(&mut reply).unwrap().unwrap();
        assert_refused(&f, &reply[..length], 1);
        assert!(!receiver.is_busy());
        assert_eq!(receiver.poll_reply(&mut reply), Ok(None));
        assert!(receiver.last_result().is_none());
        finish_wire_worker(&mut task);
        assert_eq!(fs::read_dir(f.path.join("wire-only")).unwrap().count(), 0);
        f.input_live(&cx);
    });
}

#[test]
fn a_foreign_binding_never_gets_a_pre_admission_refusal() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let f = Running::new(&cx).await;
        let (mut receiver, mut task) = wire_receiver(&f, &cx);
        let mut context = f.client.outgoing();
        context.channel = context.channel.checked_add(1).unwrap();
        let mut bytes = vec![0; f.client.limits().record_bytes()];
        let length = files::encode(
            files::Message {
                id: 1,
                body: Body::Offer {
                    profile: files::ATP_PORTABLE_FULL,
                    atp: b"invalid ATP frame",
                },
            },
            context,
            f.client.limits(),
            &mut bytes,
        )
        .unwrap();
        assert!(matches!(
            receiver.receive(&bytes[..length]),
            Err(fr_files::wire::Error::Wire(_))
        ));
        assert!(!receiver.is_busy());
        assert!(receiver.last_result().is_none());
        assert_eq!(receiver.poll_reply(&mut bytes), Ok(None));
        receiver.stop();
        finish_wire_worker(&mut task);
        assert_eq!(fs::read_dir(f.path.join("wire-only")).unwrap().count(), 0);
        f.input_live(&cx);
    });
}

#![cfg(target_os = "linux")]
//! Real UDP/TLS/file I/O with explicit controller/admission fixtures, not Tailscale.
mod native_support;
use asupersync::cx::Cx;
use fr_files::{
    quic::{self, State},
    worker::Completion,
};
use fr_wire::files::{self, Body, Disposition, Reason};
use native_support::{Link, Running, clock, data, manifest, offered, runtime};
use std::{
    fs,
    time::{Duration, Instant},
};

async fn begin(f: &mut Running, cx: &Cx, content: &[u8]) {
    let frame = offered(&manifest("received.bin", content));
    f.send(
        cx,
        Body::Offer {
            profile: files::ATP_PORTABLE_FULL,
            atp: &frame,
        },
    )
    .await;
    let bytes = f.reply(cx).await;
    assert!(
        matches!(files::decode(&bytes,f.client.incoming(),f.client.limits()).unwrap().body,
        Body::Accept{size,..} if size == content.len() as u64)
    );
}
async fn chunks(f: &mut Running, cx: &Cx, content: &[u8]) {
    let chunk = f.client.limits().atp_bytes() - files::ATP_DATA_OVERHEAD;
    let mut offset = 0;
    for bytes in content.chunks(chunk) {
        f.send(
            cx,
            Body::Chunk {
                atp: &data(0, offset, bytes),
            },
        )
        .await;
        offset += bytes.len() as u64;
        f.staged(cx, offset).await;
        assert!(!f.path.join("received.bin").exists());
    }
}
async fn complete(f: &mut Running, cx: &Cx) -> Vec<u8> {
    f.send(
        cx,
        Body::Chunk {
            atp: &fr_files::atp::encode_complete().unwrap(),
        },
    )
    .await;
    f.reply(cx).await
}
fn clean(f: &mut Running) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !f.host.cleanup_finished() {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    let _ = f.host.try_finish_cleanup();
    f.host.collect_after_close().unwrap();
}

#[test]
fn original_native_connection_publishes_large_file_across_stream_credit_windows() {
    runtime().block_on(async {
        let cx=Cx::current().unwrap();let mut f=Running::new(&cx).await;
        let bytes:Vec<u8>=(0..120_007).map(|n|u8::try_from(n%251).unwrap()).collect();
        begin(&mut f,&cx,&bytes).await;chunks(&mut f,&cx,&bytes).await;
        let reply=complete(&mut f,&cx).await;
        assert!(matches!(files::decode(&reply,f.client.incoming(),f.client.limits()).unwrap().body,
            Body::Complete{disposition:Disposition::PublishedDurable,published_bytes,..} if published_bytes==bytes.len() as u64));
        assert_eq!(fs::read(f.path.join("received.bin")).unwrap(),bytes);
        f.input_live(&cx);
        assert!(!f.link.c.is_closed() && !f.link.h.is_closed());
    });
}
#[test]
fn real_corrupted_content_gets_integrity_refusal_not_publication() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut f = Running::new(&cx).await;
        begin(&mut f, &cx, b"correct").await;
        chunks(&mut f, &cx, b"corrupt").await;
        let reply = complete(&mut f, &cx).await;
        assert!(matches!(
            files::decode(&reply, f.client.incoming(), f.client.limits())
                .unwrap()
                .body,
            Body::Complete {
                disposition: Disposition::Refused,
                reason: Reason::Integrity,
                published_bytes: 0,
                ..
            }
        ));
        assert!(!f.path.join("received.bin").exists());
        assert_eq!(fs::read_dir(&f.path).unwrap().count(), 0);
        f.input_live(&cx);
    });
}
#[test]
fn resetting_partial_file_cleans_disk_and_preserves_live_control() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut f = Running::new(&cx).await;
        begin(&mut f, &cx, b"complete").await;
        chunks(&mut f, &cx, b"part").await;
        f.client.retire(&mut f.link.c, &cx).unwrap();
        let deadline = clock(&cx) + 1_000_000;
        while f.host.state() != State::Retired {
            assert!(clock(&cx) < deadline);
            f.link.drive(&cx).await;
            let _ = f.host.service(&mut f.link.h, || true);
        }
        clean(&mut f);
        assert_eq!(fs::read_dir(&f.path).unwrap().count(), 0);
        f.input_live(&cx);
        assert!(!f.link.c.is_closed() && !f.link.h.is_closed());
    });
}
#[test]
fn native_owner_rejects_foreign_connection_without_damaging_either_session() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut f = Running::new(&cx).await;
        let mut foreign = Link::new(&cx).await;
        assert_eq!(
            f.host.service(&mut foreign.h, || true),
            Err(quic::Error::WrongConnection)
        );
        assert_eq!(
            f.host.retire(&mut foreign.h),
            Err(quic::Error::WrongConnection)
        );
        assert!(!foreign.h.is_closed() && !f.link.h.is_closed());
        begin(&mut f, &cx, b"legit").await;
        chunks(&mut f, &cx, b"legit").await;
        complete(&mut f, &cx).await;
        assert_eq!(fs::read(f.path.join("received.bin")).unwrap(), b"legit");
    });
}
#[test]
fn published_receipt_survives_local_retirement_and_completed_cleanup() {
    runtime().block_on(async {
        let cx=Cx::current().unwrap();let mut f=Running::new(&cx).await;
        begin(&mut f,&cx,b"durable").await;chunks(&mut f,&cx,b"durable").await;
        complete(&mut f,&cx).await;
        f.host.retire(&mut f.link.h).unwrap();clean(&mut f);
        assert!(matches!(f.host.last_result().unwrap().outcome,Ok(Completion::Published(r)) if r.bytes==7));
        assert_eq!(fs::read(f.path.join("received.bin")).unwrap(),b"durable");f.input_live(&cx);
    });
}

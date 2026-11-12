//! The independent session cleanup context must preserve the control transport.
use super::*;
use asupersync::types::{Budget, CancelKind};
use std::path::{Path, PathBuf};

pub(super) fn staged_handles(root: &Path) -> Vec<PathBuf> {
    let root = fs::canonicalize(root).unwrap();
    fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(|entry| {
            let descriptor = entry.unwrap().path();
            let target = fs::read_link(&descriptor).ok()?;
            (target.parent() == Some(root.as_path())
                && fs::metadata(&descriptor).is_ok_and(|metadata| metadata.is_file()))
            .then_some(descriptor)
        })
        .collect()
}

#[test]
fn cancelled_disk_owner_retires_on_live_cleanup_context_without_closing_parent() {
    let runtime = runtime();
    let operation = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async move {
        let cleanup = Cx::current().unwrap();
        let mut f = Running::new(&operation).await;
        begin(&mut f, &operation, b"complete").await;
        chunks(&mut f, &operation, b"part").await;
        // Both named staging and O_TMPFILE must retain the real partial inode.
        let staged = staged_handles(&f.path);
        assert_eq!(staged.len(), 1);
        assert_eq!(fs::read(&staged[0]).unwrap(), b"part");
        assert!(!f.path.join("received.bin").exists());
        operation.cancel_fast(CancelKind::User);
        assert!(operation.is_cancel_requested());
        assert!(cleanup.checkpoint().is_ok());
        f.host.retire_on(&mut f.link.h, &cleanup).unwrap();
        assert_eq!(f.host.state(), State::Retired);
        clean(&mut f);
        assert_eq!(f.host.try_finish_cleanup(), Some(Ok(())));
        assert_eq!(staged_handles(&f.path), [] as [PathBuf; 0]);
        assert_eq!(fs::read_dir(&f.path).unwrap().count(), 0);
        f.link.h.tick(&cleanup, || true).unwrap();
        f.link.c.tick(&cleanup, || true).unwrap();
        f.input_live(&cleanup);
        assert!(!f.link.h.is_closed());
        assert!(!f.link.c.is_closed());
    });
}

#[test]
fn expired_partial_file_record_cleans_staging_without_ending_control() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut f = Running::with_record_lifetime(&cx, 200_000).await;
        let remaining = f.client.limits().atp_bytes() - files::ATP_DATA_OVERHEAD;
        let content = vec![0x73; 4 + remaining];
        begin(&mut f, &cx, &content).await;
        chunks(&mut f, &cx, &content[..4]).await;
        let staged = staged_handles(&f.path);
        assert_eq!(staged.len(), 1);
        assert_eq!(fs::read(&staged[0]).unwrap(), content[..4]);

        // One native send burst cannot deliver this large Files record. Stop
        // driving the sender after its first bounded burst to plant a stalled
        // body, while the receiver continues servicing its real connection.
        f.send(
            &cx,
            Body::Chunk {
                atp: &data(0, 4, &content[4..]),
            },
        )
        .await;
        f.link.c.drive(&cx, Duration::ZERO, || true).await.unwrap();
        let before = f.link.h.usage().framed_capacity;
        let until = clock(&cx) + 1_000_000;
        while f.link.h.usage().framed_capacity <= before {
            assert!(clock(&cx) < until, "file prefix never reached framing");
            f.link
                .h
                .drive(&cx, Duration::from_millis(1), || true)
                .await
                .unwrap();
            f.host.service(&mut f.link.h, || true).unwrap();
        }
        assert_eq!(f.host.progress().unwrap().staged_bytes, 4);
        asupersync::time::sleep(cx.now(), Duration::from_millis(210)).await;

        f.link.h.tick(&cx, || true).unwrap();
        assert_eq!(
            f.host.service(&mut f.link.h, || true),
            Err(quic::Error::Transport(fr_transport::quic::Error::Stream(
                fr_wire::stream::StreamError::Expired,
            )))
        );
        assert_eq!(f.host.state(), State::Retired);
        clean(&mut f);
        assert_eq!(f.host.try_finish_cleanup(), Some(Ok(())));
        assert_eq!(staged_handles(&f.path), [] as [PathBuf; 0]);
        assert_eq!(fs::read_dir(&f.path).unwrap().count(), 0);
        assert!(!f.link.h.is_closed());
        assert!(f.link.h.usage().framed_capacity < before);
        f.link.drive(&cx).await;
        f.input_live(&cx);
        assert!(!f.link.c.is_closed());
    });
}

#[test]
fn later_receive_expiry_preserves_a_real_publication_receipt() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut f = Running::with_record_lifetime(&cx, 200_000).await;
        begin(&mut f, &cx, b"durable").await;
        chunks(&mut f, &cx, b"durable").await;
        complete(&mut f, &cx).await;
        let published = f.host.last_result().unwrap();
        assert!(matches!(published.outcome, Ok(Completion::Published(r)) if r.bytes == 7));

        let offer = offered(&manifest("later.bin", b"never admitted"));
        let bytes = f.packet(
            2,
            Body::Offer {
                profile: files::ATP_PORTABLE_FULL,
                atp: &offer,
            },
        );
        f.client
            .send(&mut f.link.c, &cx, &bytes, clock(&cx) + 1_000_000, || true)
            .unwrap();
        let mut blocked = false;
        let until = clock(&cx) + 1_000_000;
        while !blocked {
            assert!(clock(&cx) < until);
            f.link.drive(&cx).await;
            f.link
                .h
                .receive_ready(
                    &cx,
                    || true,
                    |route| f.host.owns_inbound(route),
                    |_, received| {
                        assert_eq!(received, bytes);
                        blocked = true;
                        Ok(fr_transport::quic::Disposition::Blocked)
                    },
                )
                .unwrap();
        }
        asupersync::time::sleep(cx.now(), Duration::from_millis(210)).await;
        f.link.h.tick(&cx, || true).unwrap();
        assert_eq!(
            f.host.service(&mut f.link.h, || true),
            Err(quic::Error::Transport(fr_transport::quic::Error::Stream(
                fr_wire::stream::StreamError::Expired,
            )))
        );
        clean(&mut f);
        assert_eq!(f.host.last_result(), Some(published));
        assert_eq!(fs::read(f.path.join("received.bin")).unwrap(), b"durable");
        assert!(!f.path.join("later.bin").exists());
        assert!(!f.link.h.is_closed());
        f.input_live(&cx);
    });
}

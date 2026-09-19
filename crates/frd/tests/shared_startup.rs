//! Two independently authorized TLS/UDP peers start from one native-source IDR.
//! Child encoder/decoder replies are synthetic; HEVC parameter validation is real.
#![cfg(target_os = "linux")]
#[path = "shared_startup/support.rs"]
mod support;
use asupersync::{cx::Cx, time::sleep};
use fr_core::limits::{LimitOverrides, ProtocolLimits};
use fr_media::delivery::{BudgetUsage, ReceivePipeline, ReceivePolicy, SendPolicy};
use fr_transport::quic::{self, Disposition, Messages, Route};
use fr_wire::{
    decoder,
    input::{InputDelivery, InputDirection},
};
use frd::{
    media::{
        ObservationControl, Presenter, SharedCaptureUpdate,
        decoder_startup::{Error, Host, Viewer},
    },
    media_egress::Lane,
    media_quic::QuicEgress,
    worker::Deadline,
};
use std::time::Duration;
use support::*;

async fn configuration_bytes(link: &mut Link, media: &Media, host: &mut Host, cx: &Cx) -> Vec<u8> {
    let mut sent = false;
    loop {
        assert!(clock(cx) < host.deadline_us());
        if !sent {
            sent = host.transmit(&mut link.h).unwrap();
        }
        link.drive(cx).await;
        let mut result = None;
        link.c
            .receive_ready(
                cx,
                || true,
                |r| matches!(r,Route::Stream(s) if s.messages==Messages::Exact(0x30)),
                |_, bytes| {
                    assert!(matches!(
                        decoder::decode(
                            bytes,
                            media.viewer.binding(),
                            media.viewer.limits().protocol(),
                            InputDirection::HostToViewer,
                            InputDelivery::Reliable
                        )
                        .unwrap(),
                        decoder::Message::Configuration(_)
                    ));
                    result = Some(bytes.to_vec());
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
        if let Some(bytes) = result {
            return bytes;
        }
    }
}
async fn reply(link: &mut Link, media: &Media, cx: &Cx, message: decoder::Message<'_>) {
    let until = clock(cx) + 1_000_000;
    let mut bytes = [0; 512];
    let n = decoder::encode(
        message,
        media.viewer.binding(),
        media.viewer.limits().protocol(),
        &mut bytes,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .unwrap();
    loop {
        match link
            .c
            .send(cx, Route::Stream(media.reply), &bytes[..n], until, || true)
        {
            Ok(()) => return,
            Err(quic::Error::Backpressure) => link.drive(cx).await,
            error => panic!("reply refused: {error:?}"),
        }
    }
}
async fn configured(
    link: &mut Link,
    media: &Media,
    host: &mut Host,
    cx: &Cx,
) -> (Viewer, SharedCaptureUpdate) {
    assert!(host.take_shared_recovery().unwrap().is_none());
    let bytes = configuration_bytes(link, media, host, cx).await;
    assert!(host.take_shared_recovery().unwrap().is_none());
    let mut viewer = Viewer::start(
        cx.clone(),
        &link.c,
        media
            .viewer
            .decoder_setup(&link.c, Duration::from_secs(2))
            .unwrap(),
        &bytes,
        decoder(),
        media
            .viewer
            .receiver_config(&link.c, ReceivePolicy::default())
            .unwrap(),
    )
    .await
    .unwrap();
    while !viewer.transmit(&mut link.c).unwrap() {
        link.drive(cx).await;
    }
    loop {
        link.drive(cx).await;
        host.dispatch(&mut link.h).unwrap();
        if let Some(update) = host.take_shared_recovery().unwrap() {
            return (viewer, update);
        }
    }
}
async fn finish(
    link: &mut Link,
    media: &Media,
    host: &mut Host,
    mut viewer: Viewer,
    update: &SharedCaptureUpdate,
    control: &ObservationControl,
    cx: &Cx,
) -> (QuicEgress, Presenter, ReceivePipeline) {
    let mut sender = media
        .host
        .sender(&link.h, control.clone(), SendPolicy::default())
        .unwrap();
    sender.enqueue_shared_capture(update).unwrap();
    loop {
        assert!(!host.is_complete());
        sender.transmit(cx, &mut link.h, Lane::Original).unwrap();
        link.drive(cx).await;
        media
            .viewer
            .receive_ready(
                cx,
                &mut link.c,
                || true,
                |channel, bytes| {
                    viewer.receive_media(channel, bytes).unwrap();
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
        if let Some(receipt) = viewer.present_first().await.unwrap() {
            assert_eq!(receipt.frame, update.frame());
            break;
        }
    }
    assert!(!host.is_complete());
    while !viewer.transmit(&mut link.c).unwrap() {
        link.drive(cx).await;
    }
    while !host.is_complete() {
        link.drive(cx).await;
        host.dispatch(&mut link.h).unwrap();
    }
    let (presenter, receiver) = viewer.finish().unwrap();
    (sender, presenter, receiver)
}
async fn reap(presenter: &mut Presenter, cx: &Cx) {
    presenter.abort();
    presenter
        .reap(cx, Deadline::after(cx, Duration::from_secs(1)).unwrap())
        .await
        .unwrap();
}

#[test]
fn two_viewers_configure_independently_decode_and_retain_one_native_allocation() {
    let rt = runtime();
    rt.block_on(async {
        let cx = Cx::current().unwrap();
        let (owner, a, b) = (gate(&rt, 1), gate(&rt, 13), gate(&rt, 14));
        let mut al = Link::new(&cx, 13).await;
        let mut bl = Link::new(&cx, 14).await;
        let am = al.media(&cx).await;
        let bm = bl.media(&cx).await;
        let mut source = source(&owner, true).await;
        let pid = source.worker_id();
        let pool = pool();
        let initial = source
            .prepare_shared_capture(&owner, &pool)
            .unwrap()
            .capture_if_changed(true)
            .await
            .unwrap();
        let pointer = initial.encoded().unwrap().bytes().as_ptr();
        let physical = pool.usage();
        let mut ah = Host::new_shared(
            a.clone(),
            &al.h,
            am.host
                .decoder_setup(&al.h, Duration::from_secs(2))
                .unwrap(),
            configuration(),
            initial.clone(),
        )
        .unwrap();
        let mut bh = Host::new_shared(
            b.clone(),
            &bl.h,
            bm.host
                .decoder_setup(&bl.h, Duration::from_secs(2))
                .unwrap(),
            configuration(),
            initial.clone(),
        )
        .unwrap();
        drop(initial);
        assert_eq!(pool.usage(), physical);
        let (av, au) = configured(&mut al, &am, &mut ah, &cx).await;
        assert_eq!(au.encoded().unwrap().bytes().as_ptr(), pointer);
        let (sa, mut pa, ra) = Box::pin(finish(&mut al, &am, &mut ah, av, &au, &a, &cx)).await;
        assert!(ah.is_complete());
        assert!(!bh.is_complete());
        assert!(bh.take_shared_recovery().unwrap().is_none());
        assert!(!a.view_ready().unwrap()); // A peer decode report cannot grant input or visibility.
        let (bv, bu) = configured(&mut bl, &bm, &mut bh, &cx).await;
        assert!(
            au.encoded()
                .unwrap()
                .shares_storage_with(bu.encoded().unwrap())
        );
        let (sb, mut pb, rb) = Box::pin(finish(&mut bl, &bm, &mut bh, bv, &bu, &b, &cx)).await;
        assert_eq!(bu.encoded().unwrap().bytes().as_ptr(), pointer);
        assert_eq!(pool.usage(), physical);
        assert_ne!(pa.worker_id(), pb.worker_id());
        assert_eq!(source.worker_id(), pid);
        assert!(!b.view_ready().unwrap());
        drop((au, bu, sa));
        assert_eq!(pool.usage(), physical);
        drop(sb);
        assert_eq!(pool.usage(), BudgetUsage::default());
        // Static source verification continues on the same original worker,
        // without another encoded picture or phantom startup reservation.
        let idle = source
            .prepare_shared_capture(&owner, &pool)
            .unwrap()
            .capture_if_changed(false)
            .await
            .unwrap();
        assert!(idle.is_unchanged());
        assert_eq!(pool.usage(), BudgetUsage::default());
        drop((ra, rb));
        reap(&mut pa, &cx).await;
        reap(&mut pb, &cx).await;
        stop(&mut source, &cx).await;
    });
}

#[test]
fn cancelling_one_startup_releases_only_its_alias_and_other_viewer_still_decodes() {
    let rt = runtime();
    rt.block_on(async {
        let cx = Cx::current().unwrap();
        let (owner, a, b) = (gate(&rt, 1), gate(&rt, 13), gate(&rt, 14));
        let mut al = Link::new(&cx, 13).await;
        let mut bl = Link::new(&cx, 14).await;
        let am = al.media(&cx).await;
        let bm = bl.media(&cx).await;
        let mut source = source(&owner, true).await;
        let pool = pool();
        let initial = source
            .prepare_shared_capture(&owner, &pool)
            .unwrap()
            .capture_if_changed(true)
            .await
            .unwrap();
        let used = pool.usage();
        let mut ah = Host::new_shared(
            a.clone(),
            &al.h,
            am.host
                .decoder_setup(&al.h, Duration::from_secs(2))
                .unwrap(),
            configuration(),
            initial.clone(),
        )
        .unwrap();
        let mut bh = Host::new_shared(
            b.clone(),
            &bl.h,
            bm.host
                .decoder_setup(&bl.h, Duration::from_secs(2))
                .unwrap(),
            configuration(),
            initial,
        )
        .unwrap();
        a.revoke();
        assert!(ah.tick().is_err());
        drop(ah);
        assert_eq!(pool.usage(), used);
        owner.check().unwrap();
        b.check().unwrap();
        let (viewer, update) = configured(&mut bl, &bm, &mut bh, &cx).await;
        let (sender, mut presenter, receiver) =
            Box::pin(finish(&mut bl, &bm, &mut bh, viewer, &update, &b, &cx)).await;
        drop((sender, update, receiver));
        assert_eq!(pool.usage(), BudgetUsage::default());
        reap(&mut presenter, &cx).await;
        stop(&mut source, &cx).await;
    });
}

#[test]
fn a_delayed_configured_reply_cannot_renew_the_original_shared_frame_deadline() {
    let rt = runtime();
    rt.block_on(async {
        let cx = Cx::current().unwrap();
        let (owner, a, b) = (gate(&rt, 1), gate(&rt, 13), gate(&rt, 14));
        let mut al = Link::new(&cx, 13).await;
        let mut bl = Link::new(&cx, 14).await;
        let am = al.media(&cx).await;
        let bm = bl.media(&cx).await;
        let mut source = source(&owner, true).await;
        let pool = pool();
        let initial = source
            .prepare_shared_capture(&owner, &pool)
            .unwrap()
            .capture_if_changed(true)
            .await
            .unwrap();
        let until = initial.observed_micros() + 100_000;
        let mut ah = Host::new_shared(
            a.clone(),
            &al.h,
            am.host
                .decoder_setup(&al.h, Duration::from_secs(2))
                .unwrap()
                .capped_at(until),
            configuration(),
            initial.clone(),
        )
        .unwrap();
        let mut bh = Host::new_shared(
            b.clone(),
            &bl.h,
            bm.host
                .decoder_setup(&bl.h, Duration::from_secs(2))
                .unwrap(),
            configuration(),
            initial,
        )
        .unwrap();
        configuration_bytes(&mut al, &am, &mut ah, &cx).await;
        sleep(cx.now(), Duration::from_millis(110)).await;
        reply(&mut al, &am, &cx, decoder::Message::Configured).await;
        al.drive(&cx).await;
        assert!(matches!(ah.dispatch(&mut al.h), Err(Error::Expired)));
        assert_eq!(ah.deadline_us(), until);
        assert!(ah.take_shared_recovery().is_err());
        assert_eq!(pool.usage().pictures, 1);
        let (viewer, update) = configured(&mut bl, &bm, &mut bh, &cx).await;
        let (sender, mut presenter, receiver) =
            Box::pin(finish(&mut bl, &bm, &mut bh, viewer, &update, &b, &cx)).await;
        drop((sender, update, receiver));
        assert_eq!(pool.usage(), BudgetUsage::default());
        reap(&mut presenter, &cx).await;
        stop(&mut source, &cx).await;
    });
}

#[test]
fn wrong_transfer_api_preserves_the_ready_update_and_each_transfer_is_single_use() {
    let rt = runtime();
    rt.block_on(async {
        let cx = Cx::current().unwrap();
        let owner = gate(&rt, 1);
        let control = gate(&rt, 13);
        let mut link = Link::new(&cx, 13).await;
        let media = link.media(&cx).await;
        let mut source = source(&owner, true).await;
        let pool = pool();
        let first = source
            .prepare_shared_capture(&owner, &pool)
            .unwrap()
            .capture_if_changed(true)
            .await
            .unwrap();
        let mut h = Host::new_shared(
            control.clone(),
            &link.h,
            media
                .host
                .decoder_setup(&link.h, Duration::from_secs(2))
                .unwrap(),
            configuration(),
            first.clone(),
        )
        .unwrap();
        configuration_bytes(&mut link, &media, &mut h, &cx).await;
        reply(&mut link, &media, &cx, decoder::Message::Configured).await;
        loop {
            link.drive(&cx).await;
            h.dispatch(&mut link.h).unwrap();
            match h.take_recovery() {
                Err(Error::WrongState) => break,
                Ok(None) => {}
                _ => panic!("unique API may not consume shared output"),
            }
        }
        let u = h.take_shared_recovery().unwrap().unwrap();
        assert!(
            u.encoded()
                .unwrap()
                .shares_storage_with(first.encoded().unwrap())
        );
        assert!(h.take_shared_recovery().unwrap().is_none());
        assert!(h.take_recovery().unwrap().is_none());
        assert!(!h.is_complete());
        // A completed peer must report this exact first frame, not an arbitrary ID.
        reply(
            &mut link,
            &media,
            &cx,
            decoder::Message::FirstDecoded {
                frame: 99,
                decoder_micros: clock(&cx),
            },
        )
        .await;
        let mut refused = false;
        for _ in 0..50 {
            link.drive(&cx).await;
            if h.dispatch(&mut link.h).is_err() {
                refused = true;
                break;
            }
        }
        assert!(refused);
        assert!(!h.is_complete());
        drop((u, first));
        assert_eq!(pool.usage(), BudgetUsage::default());
        stop(&mut source, &cx).await;
    });
}

#[test]
fn unique_bootstrap_still_uses_the_same_acknowledgements_and_refuses_shared_transfer() {
    let rt = runtime();
    rt.block_on(async {
        let cx = Cx::current().unwrap();
        let owner = gate(&rt, 1);
        let control = gate(&rt, 13);
        let mut link = Link::new(&cx, 13).await;
        let media = link.media(&cx).await;
        let mut source = source(&owner, true).await;
        let first = source.capture_if_changed(&owner, true).await.unwrap();
        let frame = first.frame();
        let mut h = Host::new(
            control.clone(),
            &link.h,
            media
                .host
                .decoder_setup(&link.h, Duration::from_secs(2))
                .unwrap(),
            configuration(),
            first,
        )
        .unwrap();
        configuration_bytes(&mut link, &media, &mut h, &cx).await;
        reply(&mut link, &media, &cx, decoder::Message::Configured).await;
        loop {
            link.drive(&cx).await;
            h.dispatch(&mut link.h).unwrap();
            match h.take_shared_recovery() {
                Err(Error::WrongState) => break,
                Ok(None) => {}
                _ => panic!("shared API may not consume unique output"),
            }
        }
        assert_eq!(h.take_recovery().unwrap().unwrap().frame(), frame);
        assert!(h.take_recovery().unwrap().is_none());
        reply(
            &mut link,
            &media,
            &cx,
            decoder::Message::FirstDecoded {
                frame: frame.as_raw(),
                decoder_micros: clock(&cx),
            },
        )
        .await;
        while !h.is_complete() {
            link.drive(&cx).await;
            h.dispatch(&mut link.h).unwrap();
        }
        assert!(!control.view_ready().unwrap());
        stop(&mut source, &cx).await;
    });
}

#[test]
fn opaque_bytes_and_static_observations_do_not_become_shared_bootstrap_idrs() {
    let rt = runtime();
    rt.block_on(async {
        let cx = Cx::current().unwrap();
        let owner = gate(&rt, 1);
        let control = gate(&rt, 13);
        let mut link = Link::new(&cx, 13).await;
        let media = link.media(&cx).await;
        let pool = pool();
        let mut source = source(&owner, false).await;
        let first = source
            .prepare_shared_capture(&owner, &pool)
            .unwrap()
            .capture_if_changed(true)
            .await
            .unwrap();
        let setup = media
            .host
            .decoder_setup(&link.h, Duration::from_secs(2))
            .unwrap();
        assert!(matches!(
            Host::new_shared(control.clone(), &link.h, setup, configuration(), first),
            Err(Error::Hevc(_))
        ));
        assert_eq!(pool.usage(), BudgetUsage::default());
        let idle = source
            .prepare_shared_capture(&owner, &pool)
            .unwrap()
            .capture_if_changed(false)
            .await
            .unwrap();
        assert!(idle.is_unchanged());
        assert!(matches!(
            Host::new_shared(control.clone(), &link.h, setup, configuration(), idle),
            Err(Error::WrongState)
        ));
        assert_eq!(pool.usage(), BudgetUsage::default());
        assert!(!control.view_ready().unwrap());
        stop(&mut source, &cx).await;
    });
}

#[test]
fn a_waiting_viewer_must_admit_the_full_shared_retention_charge() {
    let rt = runtime();
    rt.block_on(async {
        let cx = Cx::current().unwrap();
        let owner = gate(&rt, 1);
        let control = gate(&rt, 13);
        let mut link = Link::new(&cx, 13).await;
        link.selection.limits = ProtocolLimits::with_overrides(LimitOverrides {
            max_encoded_access_unit_bytes: Some(160),
            per_viewer_compressed_bytes: Some(160),
            ..Default::default()
        })
        .unwrap();
        let media = link.media(&cx).await;
        let pool = pool();
        let mut source = source(&owner, true).await;
        let first = source
            .prepare_shared_capture(&owner, &pool)
            .unwrap()
            .capture_if_changed(true)
            .await
            .unwrap();
        assert!(first.encoded().unwrap().bytes().len() < 160);
        assert!(first.encoded().unwrap().allocation_charge() > 160);
        let setup = media
            .host
            .decoder_setup(&link.h, Duration::from_secs(2))
            .unwrap();
        assert!(matches!(
            Host::new_shared(control.clone(), &link.h, setup, configuration(), first),
            Err(Error::Wire(fr_wire::WireError::ResourceLimit))
        ));
        assert_eq!(pool.usage(), BudgetUsage::default());
        control.check().unwrap();
        stop(&mut source, &cx).await;
    });
}

#[path = "shared_startup/publisher.rs"]
mod publisher;

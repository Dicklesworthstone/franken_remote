//! Real source process, transport and cache; opaque encoded payloads and
//! simulated decoder completion remain explicit, not hardware qualification.
use super::*;
use fr_media::{
    delivery::SendPolicy,
    worker::{Backend, Configuration, Role as WorkerRole},
};
use frd::{
    media::{CaptureSource, ObservationControl},
    media_egress::Lane,
    worker::{Deadline, Launch},
};
use std::fmt::Write;
use std::{
    os::unix::fs::PermissionsExt,
    sync::atomic::{AtomicU64, Ordering},
};

async fn source(cx: &Cx, control: &ObservationControl) -> CaptureSource {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "fr-handoff-{}-{}.py",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    // Valid admitted parameter-set/IDR fixture used only for host configuration
    // parsing. The child is not an actual encoder and no native decode is claimed.
    let mut au = Vec::new();
    for nal in [
        "40010c01ffff01600000030090000003000003003cba0240",
        "42010101600000030090000003000003003ca00a080f165ba4a4c2f016a020202080000003008000000f04",
        "4401c0718112",
        "2801ade06702f86753c11ead2f1f6a69",
    ] {
        au.extend_from_slice(&u32::try_from(nal.len() / 2).unwrap().to_be_bytes());
        for h in nal.as_bytes().as_chunks::<2>().0 {
            au.push(u8::from_str_radix(std::str::from_utf8(h).unwrap(), 16).unwrap());
        }
    }
    let mut payload = String::new();
    for value in au {
        write!(payload, "{value:02x}").unwrap();
    }
    let script = include_str!("../support/recovery_worker_fixture.py")
        .replace("@MODE@", "healthy")
        .replace(
            "b\"test-only-unit\"",
            &format!("bytes.fromhex('{payload}')"),
        );
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(net::clock(cx) > 0);
    CaptureSource::start(
        control,
        Launch::new(&path, ":0", None, WorkerRole::Capture, 45).unwrap(),
        configuration(),
    )
    .await
    .unwrap()
}
fn configuration() -> Configuration {
    Configuration {
        width: 320,
        height: 240,
        fps: 30,
        backend: Backend::SoftwareExplicit,
        bitrate: 2_000_000,
        max_access_unit_bytes: ProtocolLimits::ABSOLUTE.max_encoded_access_unit_bytes(),
        generation: CodecConfigurationGeneration::INITIAL,
    }
}
fn request(view: Binding) -> Vec<u8> {
    let mut bytes = vec![0; wire::REQUEST_BYTES];
    wire::encode(
        wire::Request {
            reason: wire::Reason::ReferenceExpired,
            last_useful_frame: Some(0),
        },
        Binding {
            parent: parent(),
            ..view
        },
        &ProtocolLimits::ABSOLUTE,
        &mut bytes,
        fr_wire::input::InputDirection::ViewerToHost,
        fr_wire::input::InputDelivery::Reliable,
    )
    .unwrap();
    bytes
}
async fn stop(source: &mut CaptureSource, cx: &Cx) {
    let worker = source.worker_mut();
    let deadline = Deadline::after(cx, Duration::from_secs(1)).unwrap();
    worker
        .request(cx, fr_media::worker::Kind::Stop, vec![], deadline)
        .await
        .unwrap();
    worker.reap(cx, deadline).await.unwrap();
}
#[test]
#[allow(clippy::too_many_lines)] // One ordered scenario retains the same actual owners.
fn retained_sender_replaces_real_channels_and_preserves_chronic_failure_history() {
    net::runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut link = Link::new(&cx, true).await;
        let (hm, vm, mut receiver) = link.media(&cx).await;
        let control = observation(&cx);
        let mut source = source(&cx, &control).await;
        let pid = source.worker_id();
        let mut sender = hm
            .sender(
                &link.h,
                control.clone(),
                SendPolicy {
                    max_recoveries_per_window: 1,
                    ..SendPolicy::default()
                },
            )
            .unwrap();
        sender
            .enqueue_capture(source.capture_if_changed(&control, true).await.unwrap())
            .unwrap();
        assert!(
            hm.request_recovery(
                &link.h,
                link.hr,
                parent(),
                &mut sender,
                &mut source,
                Route::Stream(link.hr.inbound),
                &request(hm.binding())
            )
            .unwrap()
        );
        let until = sender.next_deadline().unwrap().as_micros();
        assert!(
            !hm.request_recovery(
                &link.h,
                link.hr,
                parent(),
                &mut sender,
                &mut source,
                Route::Stream(link.hr.inbound),
                &request(hm.binding())
            )
            .unwrap()
        );
        assert_eq!(sender.next_deadline().unwrap().as_micros(), until);
        let (new_h, new_v) = replace(&mut link, &cx, hm, vm).await;
        let setup = new_h.recover_sender(&link.h, &mut sender).unwrap();
        let update = source.capture_if_changed(&control, false).await.unwrap();
        assert!(update.encoded().unwrap().is_idr());
        assert_eq!(pid, source.worker_id());
        let mut startup = frd::media::decoder_startup::Host::new(
            control.clone(),
            &link.h,
            setup,
            configuration(),
            update,
        )
        .unwrap();
        assert!(startup.deadline_us() <= until);
        assert!(!control.view_ready().unwrap());
        // Complete configuration over its actual role-specific channel before
        // draining the exact retained update to the reused packetizer.
        let mut configured = false;
        loop {
            assert!(net::clock(&cx) < until);
            if !configured {
                configured = startup.transmit(&mut link.h).unwrap();
            }
            link.drive(&cx).await;
            let mut reply = None;
            link.c
                .receive_ready(
                    &cx,
                    || true,
                    |r| matches!(r,Route::Stream(s) if s.messages==Messages::Exact(0x30)),
                    |_, b| {
                        let fr_wire::decoder::Message::Configuration(_) = fr_wire::decoder::decode(
                            b,
                            new_v.binding(),
                            &ProtocolLimits::ABSOLUTE,
                            fr_wire::input::InputDirection::HostToViewer,
                            fr_wire::input::InputDelivery::Reliable,
                        )
                        .unwrap() else {
                            panic!("configuration expected")
                        };
                        reply = Some(b.len());
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
            if reply.is_some() {
                break;
            }
        }
        // Configuration parsing and its capped deadline are checked above;
        // the following packetizer checks deliberately simulate decoder startup.
        startup.close();
        let cfg = new_v
            .receiver_config(&link.c, ReceivePolicy::default())
            .unwrap();
        receiver
            .replace(cfg.epoch, cfg.bindings, net::clock(&cx))
            .unwrap();
        receiver.decoder_configured(net::clock(&cx)).unwrap();
        // Host startup owned the previous capture; take a new explicit IDR for
        // delivery-state verification. This is NOT a complete startup proof.
        sender
            .enqueue_capture(source.capture_if_changed(&control, true).await.unwrap())
            .unwrap();
        loop {
            assert!(net::clock(&cx) < until);
            sender.transmit(&cx, &mut link.h, Lane::Original).unwrap();
            link.drive(&cx).await;
            new_v
                .receive_ready(
                    &cx,
                    &mut link.c,
                    || true,
                    |ch, b| {
                        receiver.receive(ch, b, net::clock(&cx)).unwrap();
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
            if let Some(p) = receiver.take_decodable(net::clock(&cx)).unwrap() {
                receiver.complete_decode(&p, net::clock(&cx)).unwrap();
                break;
            }
        }
        assert!(matches!(
            new_h.request_recovery(
                &link.h,
                link.hr,
                parent(),
                &mut sender,
                &mut source,
                Route::Stream(link.hr.inbound),
                &request(new_h.binding())
            ),
            Err(frd::media_quic::Error::Media(frd::media::Error::Send(
                fr_media::delivery::SendError::RecoveryLimitExceeded
            )))
        ));
        assert!(!control.view_ready().unwrap());
        stop(&mut source, &cx).await;
    });
}
#[test]
fn healthy_or_foreign_senders_are_not_rebound_by_a_replacement() {
    net::runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut link = Link::new(&cx, true).await;
        let (hm, vm, _) = link.media(&cx).await;
        let control = observation(&cx);
        let mut source = source(&cx, &control).await;
        let mut sender = hm
            .sender(&link.h, control.clone(), SendPolicy::default())
            .unwrap();
        sender
            .enqueue_capture(source.capture_if_changed(&control, true).await.unwrap())
            .unwrap();
        let usage = sender.cache_usage();
        assert!(hm.recover_sender(&link.h, &mut sender).is_err());
        let foreign = Link::new(&cx, true).await;
        assert!(matches!(
            hm.request_recovery(
                &foreign.h,
                foreign.hr,
                parent(),
                &mut sender,
                &mut source,
                Route::Stream(link.hr.inbound),
                &request(hm.binding())
            ),
            Err(frd::media_quic::Error::ForeignConnection)
        ));
        assert_eq!(sender.cache_usage(), usage);
        assert!(!sender.is_closed());
        assert!(!link.h.is_closed());
        assert!(!foreign.h.is_closed());
        let (new_h, _) = replace(&mut link, &cx, hm, vm).await;
        assert!(new_h.recover_sender(&link.h, &mut sender).is_err());
        assert_eq!(sender.cache_usage(), usage);
        stop(&mut source, &cx).await;
    });
}

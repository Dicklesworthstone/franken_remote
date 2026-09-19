//! Production decoder handshake and real child IPC. The child deliberately
//! simulates completion; only the bounded HEVC configuration parser is real.
use super::*;
use fr_media::{delivery::*, hevc::HevcGuard};
use fr_wire::{
    Channel, Fragment, FrameDescriptor, RecoveryChunk, encode_fragment, encode_recovery,
};
use frd::media::{
    Presenter,
    decoder_startup::{self, Viewer, ViewerRecovery},
};

fn config() -> Configuration {
    Configuration {
        width: 320,
        height: 240,
        fps: 30,
        backend: Backend::SoftwareExplicit,
        bitrate: 10_000,
        max_access_unit_bytes: ProtocolLimits::ABSOLUTE.max_encoded_access_unit_bytes(),
        generation: CodecConfigurationGeneration::INITIAL,
    }
}
fn declaration(media: &NegotiatedMedia, fps: u16) -> Vec<u8> {
    let mut guard = HevcGuard::new(config().codec().unwrap(), ProtocolLimits::ABSOLUTE, 4).unwrap();
    let mut au = Vec::new();
    for nal in [
        "40010c01ffff01600000030090000003000003003cba0240",
        "42010101600000030090000003000003003ca00a080f165ba4a4c2f016a020202080000003008000000f04",
        "4401c0718112",
        "2801ade06702f86753c11ead2f1f6a69",
    ] {
        au.extend_from_slice(&[0, 0, 0, 1]);
        for hex in nal.as_bytes().as_chunks::<2>().0 {
            au.push(u8::from_str_radix(std::str::from_utf8(hex).unwrap(), 16).unwrap());
        }
    }
    guard.validate_annex_b(&au, true).unwrap();
    let record = guard.decoder_record().unwrap();
    let codec = record.codec().replacen("hvc1.", "hev1.", 1);
    let mut bytes = vec![0; 16_384];
    let n = decoder::encode(
        decoder::Message::Configuration(decoder::Configuration {
            coded_width: 320,
            coded_height: 240,
            crop_width: 320,
            crop_height: 240,
            fps,
            primaries: 1,
            transfer: 1,
            matrix: 1,
            full_range: false,
            decoded_pictures: 4,
            codec: &codec,
            hvcc: record.bytes(),
        }),
        media.binding(),
        &ProtocolLimits::ABSOLUTE,
        &mut bytes,
        fr_wire::input::InputDirection::HostToViewer,
        fr_wire::input::InputDelivery::Reliable,
    )
    .unwrap();
    bytes.truncate(n);
    bytes
}
fn recovery_record(media: &NegotiatedMedia, frame: u64, cx: &Cx) -> Vec<u8> {
    let mut bytes = vec![0; media.limits().record_bytes()];
    let n = encode_recovery(
        RecoveryChunk {
            frame,
            total_bytes: 4,
            offset: 0,
            capture_micros: net::clock(cx),
            bytes: b"fake",
        },
        media.bindings().for_channel(Channel::Recovery),
        &media.limits(),
        &mut bytes,
    )
    .unwrap();
    bytes.truncate(n);
    bytes
}
async fn initial(
    link: &mut Link,
    media: &NegotiatedMedia,
    cx: &Cx,
) -> (Presenter, ReceivePipeline) {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "fr-recovery-decoder-{}-{}.py",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(
        &path,
        include_str!("../../src/media/presentation/decoder_fixture.py").replace(
            "time.sleep(.09)",
            "time.sleep(.5 if int.from_bytes(b[:8], 'big') == 9 else .01)",
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut startup = Viewer::start(
        cx.clone(),
        &link.c,
        media
            .decoder_setup(&link.c, Duration::from_secs(2))
            .unwrap(),
        &declaration(media, 30),
        Launch::new(&path, ":0", None, WorkerRole::Present, 9).unwrap(),
        media
            .receiver_config(&link.c, ReceivePolicy::default())
            .unwrap(),
    )
    .await
    .unwrap();
    assert!(startup.transmit(&mut link.c).unwrap());
    startup
        .receive_media(Channel::Recovery, &recovery_record(media, 0, cx))
        .unwrap();
    assert!(startup.present_first().await.unwrap().is_some());
    while !startup.transmit(&mut link.c).unwrap() {
        link.drive(cx).await;
    }
    // Consume actual acknowledgements before retiring these initial lanes.
    for _ in 0..4 {
        link.drive(cx).await;
        link.h
            .receive_ready(cx, || true, |_| true, |_, _| Ok(Disposition::Consumed))
            .unwrap();
    }
    startup.finish().unwrap()
}
fn fail_decode(
    receiver: &mut ReceivePipeline,
    media: &NegotiatedMedia,
    cx: &Cx,
) -> ReceivedPicture {
    let mut bytes = [0; 1150];
    let n = encode_fragment(
        Fragment {
            descriptor: FrameDescriptor {
                frame: 1,
                total_bytes: 4,
                stride: 4,
                capture_micros: net::clock(cx),
                reference: Some(0),
            },
            index: 0,
            bytes: b"lost",
        },
        media.bindings().for_channel(Channel::Video),
        &media.limits(),
        &mut bytes,
    )
    .unwrap();
    receiver
        .receive(Channel::Video, &bytes[..n], net::clock(cx))
        .unwrap();
    let held = receiver.take_decodable(net::clock(cx)).unwrap().unwrap();
    assert_eq!(
        receiver.acknowledge_decode(&held, false, net::clock(cx)),
        Err(DeliveryError::DecodeFailed)
    );
    held
}
async fn reported(
    link: &mut Link,
    cx: &Cx,
    report: &mut frd::media_quic::recovery::Receiver,
    receiver: &mut ReceivePipeline,
    mut view: Binding,
) {
    let until = net::clock(cx) + 1_000_000;
    view.parent = parent();
    loop {
        assert!(net::clock(cx) < until);
        if report.service(cx, &mut link.c, receiver, || true).unwrap()
            == frd::media_quic::recovery::State::Requested
        {
            break;
        }
        link.drive(cx).await;
    }
    let mut seen = false;
    while !seen {
        assert!(net::clock(cx) < until);
        link.drive(cx).await;
        link.h
            .receive_ready(
                cx,
                || true,
                |r| r == Route::Stream(link.hr.inbound),
                |_, bytes| {
                    wire::decode(
                        bytes,
                        view,
                        &ProtocolLimits::ABSOLUTE,
                        fr_wire::input::InputDirection::ViewerToHost,
                        fr_wire::input::InputDelivery::Reliable,
                    )
                    .unwrap();
                    seen = true;
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
    }
}
async fn reap(presenter: &mut Presenter, cx: &Cx) {
    presenter.abort();
    presenter
        .reap(cx, Deadline::after(cx, Duration::from_secs(1)).unwrap())
        .await
        .unwrap();
}
#[test]
fn decoder_restart_preserves_actual_worker_receiver_budget_and_two_ack_gate() {
    net::runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut link = Link::new(&cx, true).await;
        let (hm, cm, _) = link.media(&cx).await;
        let (mut presenter, mut receiver) = initial(&mut link, &cm, &cx).await;
        let worker = presenter.worker_id();
        let mut report = cm
            .recovery_receiver(&link.c, link.cr, parent(), &receiver)
            .unwrap();
        let held = fail_decode(&mut receiver, &cm, &cx);
        reported(&mut link, &cx, &mut report, &mut receiver, cm.binding()).await;
        let charged = receiver.budget_usage();
        assert!(charged.bytes != 0 && charged.pictures == 1);
        let (_, cm) = replace(&mut link, &cx, hm, cm).await;
        let until = net::clock(&cx) + 1_000_000;
        let mut startup = ViewerRecovery::prepare(
            cx.clone(),
            &link.c,
            &cm,
            &declaration(&cm, 30),
            until,
            &mut report,
            &mut presenter,
            &mut receiver,
        )
        .unwrap();
        assert_eq!(startup.deadline_us(), until);
        assert_eq!(startup.worker_id(), worker);
        assert!(!startup.is_complete());
        assert!(startup.pending_acknowledgement());
        assert!(startup.transmit(&mut link.c).unwrap());
        assert!(!startup.is_complete());
        startup
            .receive_media(Channel::Recovery, &recovery_record(&cm, 2, &cx))
            .unwrap();
        assert!(!startup.is_complete());
        assert_eq!(
            startup
                .present_first()
                .await
                .unwrap()
                .unwrap()
                .frame
                .as_raw(),
            2
        );
        assert!(!startup.is_complete());
        while !startup.transmit(&mut link.c).unwrap() {
            link.drive(&cx).await;
        }
        assert!(startup.is_complete());
        startup.finish(&link.c, &cm).unwrap();
        assert_eq!(presenter.worker_id(), worker);
        assert_eq!(receiver.state(), ReceiveState::Streaming);
        assert_eq!(receiver.budget_usage(), charged);
        assert!(!held.is_live());
        drop(held);
        assert_eq!(receiver.budget_usage().bytes, 0);
        assert_eq!(receiver.budget_usage().pictures, 0);
        reap(&mut presenter, &cx).await;
    });
}
#[test]
fn wrong_parameters_and_foreign_receivers_are_refused_before_native_reuse() {
    net::runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut link = Link::new(&cx, true).await;
        let (hm, cm, mut foreign) = link.media(&cx).await;
        let (mut presenter, mut receiver) = initial(&mut link, &cm, &cx).await;
        let mut report = cm
            .recovery_receiver(&link.c, link.cr, parent(), &receiver)
            .unwrap();
        let held = fail_decode(&mut receiver, &cm, &cx);
        reported(&mut link, &cx, &mut report, &mut receiver, cm.binding()).await;
        let worker = presenter.worker_id();
        let (_, cm) = replace(&mut link, &cx, hm, cm).await;
        let until = net::clock(&cx) + 1_000_000;
        assert!(matches!(
            ViewerRecovery::prepare(
                cx.clone(),
                &link.c,
                &cm,
                &declaration(&cm, 60),
                until,
                &mut report,
                &mut presenter,
                &mut receiver
            ),
            Err(decoder_startup::Error::UnsupportedConfiguration)
        ));
        assert_eq!(presenter.worker_id(), worker);
        assert_eq!(receiver.state(), ReceiveState::NeedsRecovery);
        assert!(
            ViewerRecovery::prepare(
                cx.clone(),
                &link.c,
                &cm,
                &declaration(&cm, 30),
                until,
                &mut report,
                &mut presenter,
                &mut foreign
            )
            .is_err()
        );
        assert_eq!(foreign.state(), ReceiveState::AwaitingConfiguration);
        assert_eq!(receiver.state(), ReceiveState::NeedsRecovery);
        // A valid retry is still possible: preflight failures mutated no owner.
        let startup = ViewerRecovery::prepare(
            cx.clone(),
            &link.c,
            &cm,
            &declaration(&cm, 30),
            until,
            &mut report,
            &mut presenter,
            &mut receiver,
        )
        .unwrap();
        drop(startup);
        assert_eq!(receiver.state(), ReceiveState::Closed);
        assert!(!held.is_live());
        drop(held);
        reap(&mut presenter, &cx).await;
    });
}
#[test]
fn recovery_deadline_cannot_be_refilled_by_startup_or_configured_acknowledgement() {
    net::runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut link = Link::new(&cx, true).await;
        let (hm, cm, _) = link.media(&cx).await;
        let (mut presenter, mut receiver) = initial(&mut link, &cm, &cx).await;
        let mut report = cm
            .recovery_receiver(&link.c, link.cr, parent(), &receiver)
            .unwrap();
        let held = fail_decode(&mut receiver, &cm, &cx);
        reported(&mut link, &cx, &mut report, &mut receiver, cm.binding()).await;
        let (_, cm) = replace(&mut link, &cx, hm, cm).await;
        let until = net::clock(&cx) + 40_000;
        let mut startup = ViewerRecovery::prepare(
            cx.clone(),
            &link.c,
            &cm,
            &declaration(&cm, 30),
            until,
            &mut report,
            &mut presenter,
            &mut receiver,
        )
        .unwrap();
        assert_eq!(startup.deadline_us(), until);
        startup.transmit(&mut link.c).unwrap();
        while net::clock(&cx) < until {
            link.drive(&cx).await;
        }
        assert_eq!(startup.tick(&link.c), Err(decoder_startup::Error::Expired));
        assert!(!startup.is_complete());
        drop(startup);
        assert_eq!(receiver.state(), ReceiveState::Closed);
        assert!(
            ViewerRecovery::prepare(
                cx.clone(),
                &link.c,
                &cm,
                &declaration(&cm, 30),
                net::clock(&cx) + 2_000_000,
                &mut report,
                &mut presenter,
                &mut receiver
            )
            .is_err()
        );
        drop(held);
        reap(&mut presenter, &cx).await;
    });
}

#[test]
fn in_flight_native_decode_is_cancelled_at_the_original_recovery_deadline() {
    net::runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut link = Link::new(&cx, true).await;
        let (hm, cm, _) = link.media(&cx).await;
        let (mut presenter, mut receiver) = initial(&mut link, &cm, &cx).await;
        let mut report = cm
            .recovery_receiver(&link.c, link.cr, parent(), &receiver)
            .unwrap();
        let held = fail_decode(&mut receiver, &cm, &cx);
        reported(&mut link, &cx, &mut report, &mut receiver, cm.binding()).await;
        let (_, cm) = replace(&mut link, &cx, hm, cm).await;
        let until = net::clock(&cx) + 40_000;
        let mut startup = ViewerRecovery::prepare(
            cx.clone(),
            &link.c,
            &cm,
            &declaration(&cm, 30),
            until,
            &mut report,
            &mut presenter,
            &mut receiver,
        )
        .unwrap();
        startup.transmit(&mut link.c).unwrap();
        // Frame nine deliberately blocks the synthetic native peer for 500 ms.
        startup
            .receive_media(Channel::Recovery, &recovery_record(&cm, 9, &cx))
            .unwrap();
        assert!(matches!(
            startup.present_first().await,
            Err(decoder_startup::Error::Expired)
        ));
        assert!(!startup.is_complete());
        drop(startup);
        assert_eq!(receiver.state(), ReceiveState::Closed);
        assert!(!held.is_live());
        drop(held);
        reap(&mut presenter, &cx).await;
    });
}

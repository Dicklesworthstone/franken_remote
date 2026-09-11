//! Real receiver grammar and authenticated QUIC. Payloads are explicit opaque
//! fixtures, not HEVC; native decoding/pixels have separate executed regressions.
use super::*;
use crate::session_startup::{
    running::{
        controlled::tests::attach,
        tests::{pair_initialized, run},
    },
    tests::support,
};
use fr_core::{
    ids::{CodecConfigurationGeneration, RecoveryGeneration},
    limits::ProtocolLimits,
};
use fr_media::delivery::{MediaBindings, MediaBudget, MediaEpoch, ReceiveConfig, ReceivePolicy};
use fr_wire::*;

fn config() -> ReceiveConfig {
    ReceiveConfig {
        limits: MediaLimits::new(ProtocolLimits::ABSOLUTE, 1150, 16384, 64).unwrap(),
        bindings: MediaBindings::new(1, 2, 3, 4).unwrap(),
        epoch: MediaEpoch {
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
        policy: ReceivePolicy::default(),
    }
}
fn running(config: ReceiveConfig, stamp: u64) -> ReceivePipeline {
    let mut receiver =
        ReceivePipeline::new(config, MediaBudget::new(config.limits.protocol()).unwrap()).unwrap();
    receiver.decoder_configured(stamp).unwrap();
    let mut bytes = [0; 1150];
    let n = encode_recovery(
        RecoveryChunk {
            frame: 0,
            total_bytes: 4,
            offset: 0,
            capture_micros: stamp,
            bytes: b"data",
        },
        config.bindings.for_channel(Channel::Recovery),
        &config.limits,
        &mut bytes,
    )
    .unwrap();
    receiver
        .receive(Channel::Recovery, &bytes[..n], stamp)
        .unwrap();
    let picture = receiver.take_decodable(stamp).unwrap().unwrap();
    receiver.complete_decode(&picture, stamp).unwrap();
    receiver
}
fn progress(config: ReceiveConfig, stamp: u64) -> (FrameDescriptor, Vec<u8>) {
    let descriptor = FrameDescriptor {
        frame: 1,
        reference: Some(0),
        total_bytes: 4,
        stride: 4,
        capture_micros: stamp,
    };
    let mut bytes = vec![0; 1150];
    let n = encode_progress(
        Progress {
            descriptor,
            observed_micros: stamp,
            observation: SourceObservation::Captured,
            pipeline: PipelineState::Running,
        },
        config.bindings.for_channel(Channel::MediaConfig),
        &config.limits,
        &mut bytes,
    )
    .unwrap();
    bytes.truncate(n);
    (descriptor, bytes)
}
fn fragments(config: ReceiveConfig, descriptor: FrameDescriptor) -> Vec<u8> {
    let mut bytes = vec![0; 1150];
    let n = encode_fragment(
        Fragment {
            descriptor,
            index: 0,
            bytes: b"next",
        },
        config.bindings.for_channel(Channel::Video),
        &config.limits,
        &mut bytes,
    )
    .unwrap();
    bytes.truncate(n);
    bytes
}
#[test]
fn pending_repair_keeps_original_bytes_deadline_and_attempt() {
    let cfg = config();
    let mut receiver = running(cfg, 0);
    let (d, bytes) = progress(cfg, 0);
    receiver.receive(Channel::MediaConfig, &bytes, 0).unwrap();
    let mut repair = Repair::default();
    repair.prepare(&mut receiver, 20_000).unwrap();
    let bytes = repair.bytes;
    let until = repair.until;
    assert!(repair.len > 0);
    assert_eq!(until, 100_000);
    for stamp in [30_000, 50_000, 80_000, 99_999] {
        repair.prepare(&mut receiver, stamp).unwrap();
        assert_eq!(repair.bytes, bytes);
        assert_eq!(repair.until, until);
        assert_eq!(repair.frame, 1);
    }
    receiver
        .receive(Channel::Video, &fragments(cfg, d), 99_999)
        .unwrap();
    repair.prepare(&mut receiver, 99_999).unwrap();
    assert_eq!(repair.len, 0);
    assert!(repair.bytes.iter().all(|&b| b == 0));
}
#[test]
fn expired_pending_repair_is_not_replaced_or_retimed() {
    let cfg = config();
    let mut receiver = running(cfg, 0);
    let (_, bytes) = progress(cfg, 0);
    receiver.receive(Channel::MediaConfig, &bytes, 0).unwrap();
    let mut repair = Repair::default();
    repair.prepare(&mut receiver, 20_000).unwrap();
    let bytes = repair.bytes;
    let until = repair.until;
    assert_eq!(repair.prepare(&mut receiver, until), Err(Error::Closed));
    assert_eq!(repair.bytes, bytes);
    assert_eq!(repair.until, until);
}
#[test]
fn delayed_repair_never_extends_the_missing_pictures_reference_deadline() {
    let cfg = config();
    let mut receiver = running(cfg, 0);
    let (_, bytes) = progress(cfg, 0);
    receiver.receive(Channel::MediaConfig, &bytes, 0).unwrap();
    let mut repair = Repair::default();
    repair.prepare(&mut receiver, 230_000).unwrap();
    assert_eq!(repair.until, 250_000);
    assert!(repair.prepare(&mut receiver, 250_000).is_err());
}
fn capabilities() -> Vec<negotiation::Capability> {
    [
        decoder::CAPABILITY,
        attachment::CAPABILITY,
        attachment::DELIVERY_CAPABILITY,
    ]
    .into_iter()
    .map(|name| negotiation::Capability {
        name: name.into(),
        version: 1,
        required: true,
    })
    .collect()
}
#[allow(clippy::unnecessary_wraps)]
fn block(_: Route, _: &[u8]) -> Result<Disposition, ()> {
    Ok(Disposition::Blocked)
}
#[test]
#[allow(clippy::too_many_lines)]
fn actual_quic_viewer_service_repairs_an_entirely_lost_final_picture() {
    run(|c, h| async move {
        let (mut host, mut viewer) = pair_initialized(&c, &h, capabilities(), |_| {}).await;
        let (hc, vc) = attach(
            &mut host,
            &mut viewer,
            &c,
            &h,
            attachment::MediaRole::Configuration,
            18,
        )
        .await;
        let (hr, vr) = attach(
            &mut host,
            &mut viewer,
            &c,
            &h,
            attachment::MediaRole::Recovery,
            19,
        )
        .await;
        let (hv, vv) = attach(
            &mut host,
            &mut viewer,
            &c,
            &h,
            attachment::MediaRole::Video,
            20,
        )
        .await;
        let selection = host.selection().clone();
        let hm = NegotiatedMedia::new(host.io().unwrap().0, &selection, &hc, &hr, &hv).unwrap();
        let vm = NegotiatedMedia::new(viewer.io().unwrap().0, &selection, &vc, &vr, &vv).unwrap();
        let cfg = vm
            .receiver_config(viewer.io().unwrap().0, ReceivePolicy::default())
            .unwrap();
        let mut receiver = running(cfg, now(&c).unwrap());
        let (descriptor, bytes) = progress(cfg, now(&h).unwrap());
        let q = host.io().unwrap().0;
        let route = Route::Stream(hm.progress_for_test(q));
        let progress_until = now(&h).unwrap() + 200_000;
        let mut progress_sent = false;
        let repair_route = Route::Stream(hm.repair_stream(q).unwrap());
        let datagram = Route::Datagram(hv.completed_on(q).unwrap().datagram.unwrap());
        let mut peer = Peer::Observe {
            session: viewer,
            media: vm,
        };
        let mut repair = Repair::default();
        let mut stats = Statistics::default();
        let mut n = 7000_u128;
        let mut missing = false;
        let mut sent = false;
        let until = now(&c).unwrap() + 500_000;
        loop {
            assert!(now(&c).unwrap() < until);
            if !progress_sent {
                match host
                    .io()
                    .unwrap()
                    .0
                    .send(&h, route, &bytes, progress_until, || true)
                {
                    Ok(()) => progress_sent = true,
                    Err(quic::Error::Backpressure) => {}
                    Err(error) => panic!("progress send failed: {error:?}"),
                }
            }
            let (a, b) = Box::pin(support::both(
                host.drive(
                    Duration::from_millis(1),
                    || {
                        n += 1;
                        Ok(n)
                    },
                    |r, bytes| {
                        if r != repair_route {
                            return Ok(Disposition::Blocked);
                        }
                        let record = Record::decode(
                            bytes,
                            &cfg.limits,
                            cfg.bindings.for_channel(Channel::Control),
                            Channel::Control,
                        )
                        .unwrap();
                        let request = decode_repair(record, 1, &cfg.limits).unwrap();
                        assert_eq!(request.frame, 1);
                        missing = true;
                        Ok(Disposition::Consumed)
                    },
                ),
                network(
                    &mut peer,
                    &mut receiver,
                    &mut repair,
                    &mut stats,
                    &c,
                    &mut |_| {},
                    &mut block,
                ),
            ))
            .await;
            a.unwrap();
            b.unwrap();
            if missing && !sent {
                let bytes = fragments(cfg, descriptor);
                host.io()
                    .unwrap()
                    .0
                    .send(&h, datagram, &bytes, now(&h).unwrap() + 100_000, || true)
                    .unwrap();
                sent = true;
            }
            if let Some(picture) = receiver.take_decodable(now(&c).unwrap()).unwrap() {
                assert_eq!(picture.descriptor().frame, 1);
                assert_eq!(picture.bytes(), b"next");
                assert!(sent && missing);
                assert_eq!(stats.repair_requests, 1);
                break;
            }
        }
        host.close();
        peer.close();
        receiver.close();
    });
}

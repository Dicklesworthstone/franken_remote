//! Real TLS/UDP attachments, shared publisher and supervised audio worker IPC.
//! The worker emits synthetic compressed payloads, not Opus/device evidence.
use super::*;
use fr_media::worker::audio::Monitor;
use fr_wire::audio as wire;
use frd::{
    media::shared_publisher::{AudioProfile, AudioSource, RecoveryState},
    media_quic::AudioLanes,
    worker::Retirement,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};

const AUDIO_WORKER: &str = r#"#!/usr/bin/python3
import os, struct, time
def read(n):
    data = b""
    while len(data) < n:
        chunk = os.read(0, n - len(data))
        if not chunk:
            raise SystemExit(0)
        data += chunk
    return data
def reply(h, kind, body):
    packet = h[:6] + struct.pack(">H", kind) + h[8:32] + struct.pack(">I", len(body)) + body
    while packet:
        packet = packet[os.write(1, packet):]
h = read(36)
config = read(struct.unpack(">I", h[32:])[0])
assert struct.unpack(">H", h[6:8])[0] == 18
reply(h, 276, config)
sequence, next_packet = 0, time.monotonic()
while True:
    h = read(36)
    body = read(struct.unpack(">I", h[32:])[0])
    kind = struct.unpack(">H", h[6:8])[0]
    if kind == 5:
        reply(h, 262, b"")
        raise SystemExit(0)
    assert kind == 19 and body == b""
    now = time.monotonic()
    if now < next_packet:
        reply(h, 277, bytes(12))
        continue
    body = struct.pack(">B3xIIQQHH", 1, 0, 0, sequence, sequence * 960, 960, 4) + b"test"
    reply(h, 277, body)
    sequence += 1
    next_packet = now + 0.02
"#;

struct Probe {
    lanes: AudioLanes,
    configuration: Option<wire::AudioConfiguration>,
    acknowledgement: Option<(Vec<u8>, u64)>,
    last_sequence: Option<u64>,
    packets: u64,
}
impl Probe {
    fn new(peer: &Peer) -> Self {
        Self {
            lanes: peer.media.audio_lanes(&peer.link.c).unwrap().unwrap(),
            configuration: None,
            acknowledgement: None,
            last_sequence: None,
            packets: 0,
        }
    }
    async fn turn(&mut self, cx: &Cx, peer: &mut Peer) {
        self.pump(cx, &mut peer.link, peer.subscriber.as_mut().unwrap())
            .await;
    }
    async fn pump(&mut self, cx: &Cx, link: &mut Link, subscriber: &mut Subscriber) {
        let report = subscriber.service(cx, &mut link.h, 8).unwrap();
        assert!(report.accepted <= 12); // Eight video plus four bounded audio records.
        link.drive(cx).await;
        let lanes = self.lanes;
        link.c
            .receive_ready(
                cx,
                || true,
                |route| {
                    route == Route::Stream(lanes.control) || route == Route::Datagram(lanes.packets)
                },
                |_, bytes| {
                    match wire::record_kind(bytes) {
                        Some(0x0060) => {
                            assert!(self.configuration.is_none(), "audio epoch restarted");
                            let config = wire::decode_configuration(bytes, lanes.binding).unwrap();
                            let mut ack = vec![0; wire::AUDIO_CONFIGURED_RECORD_BYTES];
                            wire::encode_configured(
                                &wire::AudioConfigured {
                                    direction: config.direction,
                                    generation: config.generation,
                                    accepted: true,
                                    actual_channels: config.channels,
                                    actual_sample_rate: config.sample_rate,
                                    actual_frame_duration_ms: config.frame_duration_ms,
                                },
                                lanes.binding,
                                &mut ack,
                            )
                            .unwrap();
                            self.configuration = Some(config);
                            self.acknowledgement = Some((ack, clock(cx) + 1_000_000));
                        }
                        Some(0x0062) => {
                            let packet = wire::decode_packet(bytes, lanes.binding).unwrap();
                            assert_eq!(packet.generation, self.configuration.unwrap().generation);
                            assert!(self.last_sequence.is_none_or(|last| packet.sequence > last));
                            assert_eq!(packet.payload, b"test");
                            self.last_sequence = Some(packet.sequence);
                            self.packets += 1;
                        }
                        kind => panic!("unexpected audio record during video recovery: {kind:?}"),
                    }
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
        if let Some((bytes, until)) = &self.acknowledgement {
            match link
                .c
                .send(cx, Route::Stream(lanes.replies), bytes, *until, || true)
            {
                Ok(()) => self.acknowledgement = None,
                Err(quic::Error::Backpressure) => {}
                error => panic!("audio acknowledgement refused: {error:?}"),
            }
        }
        link.drive(cx).await;
        link.h
            .receive_ready(
                cx,
                || true,
                |route| {
                    matches!(route, Route::Stream(stream) if stream.messages == Messages::AudioReplies)
                },
                |route, bytes| Ok(subscriber.audio_record(route, bytes).unwrap().unwrap()),
            )
            .unwrap();
    }
}

async fn more_packets(cx: &Cx, peers: &mut [Peer], probes: &mut [Probe]) {
    let targets: Vec<_> = probes.iter().map(|probe| probe.packets + 2).collect();
    let until = clock(cx) + 1_000_000;
    while probes
        .iter()
        .zip(&targets)
        .any(|(probe, target)| probe.packets < *target)
    {
        assert!(clock(cx) < until, "audio stopped during video recovery");
        for (peer, probe) in peers.iter_mut().zip(probes.iter_mut()) {
            probe.turn(cx, peer).await;
        }
    }
}

async fn replace_video(
    cx: &Cx,
    mut failed: Peer,
    probe: &mut Probe,
    healthy: &mut [Peer],
    healthy_probes: &mut [Probe],
) -> Peer {
    recovery::admit(&mut failed);
    // Hold the actual recovery boundary long enough for two new source pulls.
    // A sole viewer must retain demand here, before tickets even exist.
    let packets = probe.packets;
    let until = failed
        .subscriber
        .as_mut()
        .unwrap()
        .recovery_deadline(&failed.link.h)
        .unwrap()
        .unwrap();
    while probe.packets < packets + 2 {
        assert!(clock(cx) < until, "fenced video suppressed audio");
        probe.turn(cx, &mut failed).await;
        for (peer, probe) in healthy.iter_mut().zip(healthy_probes.iter_mut()) {
            probe.turn(cx, peer).await;
        }
    }
    let Peer {
        mut link,
        media,
        presenter,
        receiver,
        control,
        subscriber,
    } = failed;
    let mut subscriber = subscriber.unwrap();
    subscriber
        .advance_recovery(cx, &mut link.h, Some(recovery::tickets()))
        .unwrap();
    let mut replacement = media
        .begin_replacement(
            cx,
            &mut link.c,
            link.cr,
            parent(link.session),
            until,
            None,
            || true,
        )
        .unwrap();
    loop {
        assert!(clock(cx) < until);
        if subscriber.recovery_state(&link.h).unwrap() == RecoveryState::Attaching {
            subscriber.advance_recovery(cx, &mut link.h, None).unwrap();
        }
        replacement.advance(cx, &mut link.c, || true).unwrap();
        probe.pump(cx, &mut link, &mut subscriber).await;
        for (peer, probe) in healthy.iter_mut().zip(healthy_probes.iter_mut()) {
            probe.turn(cx, peer).await;
        }
        if replacement.is_complete()
            && subscriber.recovery_state(&link.h).unwrap() == RecoveryState::DecoderStartup
        {
            break;
        }
    }
    let media = replacement.finish(cx, &mut link.c).unwrap();
    assert_eq!(media.audio_lanes(&link.c).unwrap(), Some(probe.lanes));
    assert_eq!(subscriber.recovery_deadline(&link.h).unwrap(), Some(until));
    assert!(!subscriber.startup_complete(&link.h).unwrap());
    assert!(!control.view_ready().unwrap());
    Peer {
        link,
        media,
        presenter,
        receiver,
        control,
        subscriber: Some(subscriber),
    }
}

async fn continuity(rt: &Runtime, keep_healthy: bool) {
    let cx = Cx::current().unwrap();
    let mut cohort = Box::pin(joined_with_capabilities(
        rt,
        false,
        false,
        &[fr_wire::recovery_request::CAPABILITY, wire::CAPABILITY],
    ))
    .await;
    if !keep_healthy {
        let mut departed = cohort.peers.pop().unwrap();
        drop(departed.subscriber.take());
        reap(&mut departed.presenter, &cx).await;
    }
    let healthy_view = cohort.peers.last().unwrap().media.binding();
    let mut probes: Vec<_> = cohort.peers.iter().map(Probe::new).collect();
    let launches = Arc::new(AtomicU64::new(0));
    let counted = Arc::clone(&launches);
    let retired: Arc<Mutex<Option<Retirement>>> = Arc::default();
    let profile = AudioProfile::new(
        fixture(AUDIO_WORKER, "audio"),
        ":0".into(),
        "/test-only-audio-monitor".into(),
        Monitor::DefaultSink,
        Arc::new(move || Ok(u128::from(counted.fetch_add(1, Ordering::SeqCst)) + 100)),
        Arc::clone(&retired),
    )
    .unwrap();
    let source = AudioSource::new(profile, cohort.publisher.audio_feed());
    let scenario = async {
        more_packets(&cx, &mut cohort.peers, &mut probes).await;
        let failed = cohort.peers.remove(0);
        let mut probe = probes.remove(0);
        let recovered = Box::pin(replace_video(
            &cx,
            failed,
            &mut probe,
            &mut cohort.peers,
            &mut probes,
        ))
        .await;
        cohort.peers.insert(0, recovered);
        probes.insert(0, probe);
        // No new video capture or decoder acknowledgement has occurred yet.
        more_packets(&cx, &mut cohort.peers, &mut probes).await;
        assert_eq!(launches.load(Ordering::SeqCst), 1);
        if keep_healthy {
            assert_eq!(cohort.peers[1].media.binding(), healthy_view);
            let mut revoked = cohort.peers.remove(0);
            probes.remove(0);
            revoked.control.revoke();
            assert!(revoked.service(&cx, 8).is_err());
            more_packets(&cx, &mut cohort.peers, &mut probes).await;
            cohort.peers[0].control.check().unwrap();
            cohort.owner.check().unwrap();
            cohort.peers.push(revoked);
        } else {
            cohort.owner.suspend();
            assert!(cohort.peers[0].service(&cx, 8).is_err());
            assert!(cohort.peers[0].control.check().is_err());
        }
        stop_cohort(&mut cohort, &cx).await;
    };
    let (result, ()) = Box::pin(crate::support::net::both(source.serve(), scenario)).await;
    assert!(result.is_err()); // The original share was explicitly closed.
    let mut retirement = retired.lock().unwrap().take().unwrap();
    assert!(
        retirement
            .reap(&cx, Deadline::after(&cx, Duration::from_secs(1)).unwrap())
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(launches.load(Ordering::SeqCst), 1);
}

#[test]
fn sole_viewer_keeps_audio_source_routes_and_epoch_through_video_replacement() {
    let rt = runtime();
    rt.block_on(Box::pin(continuity(&rt, false)));
}

#[test]
fn recovering_viewer_audio_and_revocation_are_independent_of_healthy_sibling() {
    let rt = runtime();
    rt.block_on(Box::pin(continuity(&rt, true)));
}

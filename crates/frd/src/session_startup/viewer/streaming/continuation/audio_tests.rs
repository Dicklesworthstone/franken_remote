//! Recovery's production parent drive over authenticated QUIC and negotiated
//! audio routes. The output is a counted TEST DOUBLE, not Opus/device evidence.
//! No video packets or synthetic visibility are needed to make audio progress.
use super::*;
use crate::{
    media_quic::AudioLanes,
    session_startup::{
        HostSession,
        running::{
            controlled::tests::attach,
            tests::{pair_initialized, run},
        },
        tests::support,
        viewer::streaming::audio::{AudioEnd, AudioOutput, OutputRefused, ViewerAudio},
    },
};
use fr_core::audio::{AudioChannels, AudioDirection, AudioGeneration, AudioStopReason};
use fr_transport::quic;
use fr_wire::{audio as wire, negotiation::Capability};
use std::{cell::RefCell, rc::Rc};

const AUDIO_BINDING: u32 = 9;

#[derive(Default)]
struct Calls {
    configured: u32,
    serviced: u32,
    admitted: u32,
    received: u32,
    queued: u32,
    rendered: u32,
    resets: u32,
    dropped: u32,
    fail_once: bool,
    cancel: Option<Cx>,
}
struct Output {
    calls: Rc<RefCell<Calls>>,
    pending: Option<(u32, wire::AudioConfiguration)>,
}
impl AudioOutput for Output {
    fn configure(
        &mut self,
        binding: u32,
        offer: wire::AudioConfiguration,
    ) -> Result<(), OutputRefused> {
        self.calls.borrow_mut().configured += 1;
        self.pending = Some((binding, offer));
        Ok(())
    }
    fn service(
        &mut self,
        live: &mut dyn FnMut() -> bool,
        acknowledge: &mut dyn FnMut(&[u8]) -> Result<(), OutputRefused>,
    ) -> Result<(), OutputRefused> {
        if !live() {
            return Err(OutputRefused);
        }
        let mut calls = self.calls.borrow_mut();
        calls.serviced += 1;
        if let Some(cx) = calls.cancel.take() {
            // A native callback returning success cannot override cancellation.
            cx.cancel_fast(asupersync::types::CancelKind::User);
            return Ok(());
        }
        if std::mem::take(&mut calls.fail_once) {
            return Err(OutputRefused);
        }
        if let Some((binding, offer)) = self.pending.take() {
            let mut record = [0; wire::AUDIO_CONFIGURED_RECORD_BYTES];
            wire::encode_configured(
                &wire::AudioConfigured {
                    direction: offer.direction,
                    generation: offer.generation,
                    accepted: true,
                    actual_channels: offer.channels,
                    actual_sample_rate: offer.sample_rate,
                    actual_frame_duration_ms: offer.frame_duration_ms,
                },
                binding,
                &mut record,
            )
            .unwrap();
            acknowledge(&record)?;
        }
        // Deliberately require service AFTER receive, as a native decoder and
        // output with asynchronous completion do. Receipt alone is not playback.
        let queued = std::mem::take(&mut calls.queued);
        calls.rendered += queued;
        Ok(())
    }
    fn configuration_admitted(
        &mut self,
        _: u32,
        _: wire::AudioConfiguration,
    ) -> Result<(), OutputRefused> {
        self.calls.borrow_mut().admitted += 1;
        Ok(())
    }
    fn receive(
        &mut self,
        bytes: &[u8],
        live: &mut dyn FnMut() -> bool,
    ) -> Result<(), OutputRefused> {
        if !live() {
            return Err(OutputRefused);
        }
        if wire::record_kind(bytes) == Some(0x0062) {
            let mut calls = self.calls.borrow_mut();
            calls.received += 1;
            calls.queued += 1;
        }
        Ok(())
    }
    fn ended(&mut self, _: AudioEnd) {
        self.pending = None;
        self.calls.borrow_mut().queued = 0;
    }
    fn reset(&mut self) {
        self.pending = None;
        let mut calls = self.calls.borrow_mut();
        calls.queued = 0;
        calls.resets += 1;
    }
}
impl Drop for Output {
    fn drop(&mut self) {
        self.calls.borrow_mut().dropped += 1;
    }
}
struct Fixture {
    host: HostSession,
    viewer: ViewerSession,
    audio: ViewerAudio,
    host_audio: AudioLanes,
    // Keep the original negotiated owner, although video is never decoded here.
    _media: NegotiatedMedia,
    calls: Rc<RefCell<Calls>>,
    nonce: u128,
    acknowledgements: u32,
    last_ack: u64,
    stops: u32,
}
async fn fixture(c: &Cx, h: &Cx) -> Fixture {
    let mut caps: Vec<_> = [
        fr_wire::decoder::CAPABILITY,
        attachment::CAPABILITY,
        attachment::DELIVERY_CAPABILITY,
        wire::CAPABILITY,
        fr_wire::audio_control::CAPABILITY,
    ]
    .into_iter()
    .map(|name| Capability {
        name: name.into(),
        version: 1,
        required: true,
    })
    .collect();
    caps.sort_by(|a, b| a.name.cmp(&b.name));
    let (mut host, mut viewer) = pair_initialized(c, h, caps, |_| {}).await;
    let (_, config) = attach(&mut host, &mut viewer, c, h, MediaRole::Configuration, 18).await;
    let (_, recovery) = attach(&mut host, &mut viewer, c, h, MediaRole::Recovery, 19).await;
    let (_, video) = attach(&mut host, &mut viewer, c, h, MediaRole::Video, 20).await;
    let (ha, va) = attach(
        &mut host,
        &mut viewer,
        c,
        h,
        MediaRole::AudioDown,
        AUDIO_BINDING,
    )
    .await;
    let mut media = NegotiatedMedia::new(
        viewer.io().unwrap().0,
        host.selection(),
        &config,
        &recovery,
        &video,
    )
    .unwrap();
    media.attach_audio(viewer.io().unwrap().0, &va).unwrap();
    let channel = ha.completed_on(host.io().unwrap().0).unwrap();
    let host_audio = AudioLanes {
        control: channel.outbound,
        replies: channel.inbound,
        packets: channel.datagram.unwrap(),
        binding: AUDIO_BINDING,
        packet_maximum: usize::try_from(channel.byte_allowance).unwrap(),
    };
    let calls = Rc::new(RefCell::new(Calls::default()));
    let mut audio = ViewerAudio::attach(&media, viewer.io().unwrap().0)
        .unwrap()
        .unwrap();
    audio
        .configure(Box::new(Output {
            calls: calls.clone(),
            pending: None,
        }))
        .unwrap();
    Fixture {
        host,
        viewer,
        audio,
        host_audio,
        _media: media,
        calls,
        nonce: 10_000,
        acknowledgements: 0,
        last_ack: 0,
        stops: 0,
    }
}
async fn turn(f: &mut Fixture) {
    let Fixture {
        host,
        viewer,
        audio,
        host_audio,
        nonce,
        acknowledgements,
        last_ack,
        stops,
        ..
    } = f;
    let refused = Cell::new(None);
    let mut link = Link {
        parent: Parent::Observe(viewer),
        audio: Some(audio),
        refused: &refused,
    };
    let (a, b) = Box::pin(support::both(
        host.drive(
            Duration::from_millis(1),
            || {
                *nonce += 1;
                Ok(*nonce)
            },
            |route, bytes| {
                if route != Route::Stream(host_audio.replies) {
                    return Ok(Disposition::Blocked);
                }
                match wire::record_kind(bytes) {
                    Some(0x0061) => {
                        let ack = wire::decode_configured(bytes, AUDIO_BINDING).unwrap();
                        assert!(ack.accepted);
                        *acknowledgements += 1;
                        *last_ack = ack.generation.as_raw();
                    }
                    Some(0x0063) => {
                        let stop = wire::decode_stop(bytes, AUDIO_BINDING).unwrap();
                        assert_eq!(stop.reason, AudioStopReason::DeviceChanged);
                        *stops += 1;
                    }
                    other => panic!("unexpected audio reply {other:?}"),
                }
                Ok(Disposition::Consumed)
            },
        ),
        link.drive(Duration::from_millis(1), |_, _| Err(())),
    ))
    .await;
    a.unwrap();
    b.unwrap();
    assert_eq!(refused.get(), None);
}
async fn send(f: &mut Fixture, h: &Cx, route: Route, bytes: &[u8]) {
    let until = now(h).unwrap() + 500_000;
    loop {
        assert!(now(h).unwrap() < until, "original send deadline expired");
        match f.host.io().unwrap().0.send(h, route, bytes, until, || {
            h.checkpoint().is_ok() && now(h).is_ok_and(|t| t < until)
        }) {
            Ok(()) => return,
            Err(quic::Error::Backpressure) => turn(f).await,
            Err(error) => panic!("audio fixture send: {error:?}"),
        }
    }
}
async fn configure(f: &mut Fixture, h: &Cx, generation: u64) {
    let offer = wire::AudioConfiguration {
        direction: AudioDirection::Downlink,
        generation: AudioGeneration::from_raw(generation),
        channels: AudioChannels::Stereo,
        sample_rate: 48_000,
        frame_duration_ms: 20,
        max_packet_bytes: 200,
        max_decoded_samples: 960,
        jitter_target_ms: 20,
    };
    let mut bytes = [0; wire::AUDIO_CONFIGURATION_RECORD_BYTES];
    wire::encode_configuration(&offer, AUDIO_BINDING, &mut bytes).unwrap();
    let route = Route::Stream(f.host_audio.control);
    send(f, h, route, &bytes).await;
    let until = now(h).unwrap() + 500_000;
    while f.last_ack != generation {
        assert!(now(h).unwrap() < until, "audio ACK stalled during recovery");
        turn(f).await;
    }
}

#[test]
fn recovery_turns_acknowledge_audio_and_service_output_during_network_silence() {
    run(|c, h| async move {
        let mut f = Box::pin(fixture(&c, &h)).await;
        configure(&mut f, &h, 1).await;
        let services = f.calls.borrow().serviced;
        for _ in 0..4 {
            turn(&mut f).await;
        }
        let calls = f.calls.borrow();
        assert_eq!(calls.serviced, services + 4);
        assert_eq!(calls.configured, 1);
        assert_eq!(calls.admitted, 1);
        assert_eq!(calls.received, 0);
        assert_eq!(calls.resets, 0);
        assert_eq!(f.acknowledgements, 1);
        assert_eq!(f.audio.statistics().restarts, 0);
        assert!(!f.viewer.is_closed());
    });
}

#[test]
fn audio_received_in_recovery_is_pumped_without_waiting_for_another_packet() {
    run(|c, h| async move {
        let mut f = Box::pin(fixture(&c, &h)).await;
        configure(&mut f, &h, 1).await;
        let mut bytes = [0; wire::AUDIO_PACKET_OVERHEAD + 4];
        let n = wire::encode_packet(
            &wire::AudioPacket {
                direction: AudioDirection::Downlink,
                generation: AudioGeneration::from_raw(1),
                sequence: 0,
                timestamp_samples: 0,
                duration_samples: 960,
                payload: &[1, 2, 3, 4],
            },
            AUDIO_BINDING,
            &mut bytes,
        )
        .unwrap();
        let route = Route::Datagram(f.host_audio.packets);
        send(&mut f, &h, route, &bytes[..n]).await;
        let until = now(&h).unwrap() + 500_000;
        while f.calls.borrow().received == 0 {
            assert!(now(&h).unwrap() < until, "audio packet was not dispatched");
            turn(&mut f).await;
        }
        assert_eq!(f.calls.borrow().rendered, 0);
        turn(&mut f).await;
        assert_eq!(f.calls.borrow().rendered, 1);
        assert_eq!(f.calls.borrow().received, 1);
        assert_eq!(f.audio.statistics().packets, 1);
        assert_eq!(f.audio.statistics().restarts, 0);
    });
}

#[test]
fn cancelled_recovery_parent_cannot_poll_the_audio_output() {
    run(|c, h| async move {
        let mut f = Box::pin(fixture(&c, &h)).await;
        configure(&mut f, &h, 1).await;
        let services = f.calls.borrow().serviced;
        c.cancel_fast(asupersync::types::CancelKind::User);
        let refused = Cell::new(None);
        let mut link = Link {
            parent: Parent::Observe(&mut f.viewer),
            audio: Some(&mut f.audio),
            refused: &refused,
        };
        assert!(
            link.drive(Duration::from_millis(1), |_, _| Err(()))
                .await
                .is_err()
        );
        assert_eq!(f.calls.borrow().serviced, services);
        assert_eq!(f.calls.borrow().admitted, 1);
    });
}

#[test]
fn cancellation_inside_audio_service_fences_output_even_after_native_success() {
    run(|c, h| async move {
        let mut f = Box::pin(fixture(&c, &h)).await;
        configure(&mut f, &h, 1).await;
        f.calls.borrow_mut().cancel = Some(c.clone());
        let refused = Cell::new(None);
        let mut link = Link {
            parent: Parent::Observe(&mut f.viewer),
            audio: Some(&mut f.audio),
            refused: &refused,
        };
        assert_eq!(
            link.drive(Duration::from_millis(1), |_, _| Err(())).await,
            Err(Error::Transport(quic::Error::Unauthorized))
        );
        assert_eq!(f.calls.borrow().dropped, 1);
        assert_eq!(f.calls.borrow().admitted, 1);
        assert_eq!(f.calls.borrow().rendered, 0);
    });
}

#[test]
fn local_audio_failure_can_exchange_stop_and_new_epoch_without_ending_recovery() {
    run(|c, h| async move {
        let mut f = Box::pin(fixture(&c, &h)).await;
        configure(&mut f, &h, 1).await;
        let connection = f.viewer.io().unwrap().0.binding();
        f.calls.borrow_mut().fail_once = true;
        turn(&mut f).await;
        assert_eq!(f.calls.borrow().resets, 1);
        let until = now(&h).unwrap() + 500_000;
        while f.stops == 0 {
            assert!(
                now(&h).unwrap() < until,
                "audio stop stalled during recovery"
            );
            turn(&mut f).await;
        }
        configure(&mut f, &h, 2).await;
        assert_eq!(f.acknowledgements, 2);
        assert_eq!(f.calls.borrow().configured, 2);
        assert_eq!(f.calls.borrow().admitted, 2);
        assert_eq!(f.audio.statistics().restarts, 1);
        assert!(f.viewer.io().unwrap().0.is_bound_to(&connection));
        assert!(!c.is_cancel_requested());
    });
}

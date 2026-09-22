//! A real exhausted attachment namespace, not a fabricated capacity return.
use super::*;
use fr_media::delivery::{DeliveryError, ReceiveState};
use fr_wire::{Channel, Fragment, FrameDescriptor, RecoveryChunk, recovery_request::Reason};
use frd::{
    media_quic::recovery::{Error as ReportError, State},
    native_connection::reconnect::{Failure, retryable},
    session_startup::{ObserverError, StreamingViewerError},
};

async fn exhausted(cx: &Cx) -> (Link, NegotiatedMedia, ReceivePipeline) {
    let mut link = Link::new(cx, true).await;
    let (host, viewer, _) = link.media(cx).await;
    let (_, viewer) = replace(&mut link, cx, host, viewer).await;
    assert_eq!(link.c.remaining_channel_pairs(), 1);
    let config = viewer
        .receiver_config(&link.c, ReceivePolicy::default())
        .unwrap();
    let mut receiver =
        ReceivePipeline::new(config, MediaBudget::new(config.limits.protocol()).unwrap()).unwrap();
    let stamp = net::clock(cx);
    receiver.decoder_configured(stamp).unwrap();
    let mut bytes = [0; 1150];
    let n = fr_wire::encode_recovery(
        RecoveryChunk {
            frame: 0,
            total_bytes: 4,
            offset: 0,
            capture_micros: stamp,
            bytes: b"fake",
        },
        viewer.bindings().for_channel(Channel::Recovery),
        &viewer.limits(),
        &mut bytes,
    )
    .unwrap();
    receiver
        .receive(Channel::Recovery, &bytes[..n], stamp)
        .unwrap();
    let picture = receiver.take_decodable(stamp).unwrap().unwrap();
    receiver.complete_decode(&picture, stamp).unwrap();
    drop(picture);
    (link, viewer, receiver)
}
fn partial(receiver: &mut ReceivePipeline, media: &NegotiatedMedia, stamp: u64, complete: bool) {
    let mut bytes = [0; 1150];
    let n = fr_wire::encode_fragment(
        Fragment {
            descriptor: FrameDescriptor {
                frame: 1,
                reference: Some(0),
                capture_micros: stamp,
                total_bytes: if complete { 4 } else { 8 },
                stride: 4,
            },
            index: 0,
            bytes: b"next",
        },
        media.bindings().for_channel(Channel::Video),
        &media.limits(),
        &mut bytes,
    )
    .unwrap();
    receiver
        .receive(Channel::Video, &bytes[..n], stamp)
        .unwrap();
}
fn can_restart(error: ReportError) -> bool {
    retryable(Failure::Observation(ObserverError::Streaming(
        StreamingViewerError::Recovery(error),
    )))
}
#[test]
fn failed_reference_with_exhausted_roles_retains_cause_without_sending_a_doomed_request() {
    run_test!(cx, {
        let (mut link, media, mut receiver) = exhausted(&cx).await;
        let mut report = media
            .recovery_receiver(&link.c, link.cr, parent(), &receiver)
            .unwrap();
        let used = link.c.usage();
        // Namespace limits alone must not kill an otherwise healthy observation.
        assert_eq!(
            report.service(&cx, &mut link.c, &mut receiver, || true),
            Ok(State::Receiving)
        );
        assert_eq!(receiver.state(), ReceiveState::Streaming);
        assert_eq!(link.c.usage(), used);
        partial(&mut receiver, &media, net::clock(&cx), false);
        let deadline = receiver.reference_deadline().unwrap();
        asupersync::time::sleep_until(asupersync::types::Time::from_nanos(
            (deadline + 1_000) * 1_000,
        ))
        .await;
        let next = link.c.next_channel_binding().unwrap();
        let used = link.c.usage();
        let error = report
            .service(&cx, &mut link.c, &mut receiver, || true)
            .unwrap_err();
        assert_eq!(
            error,
            ReportError::NamespaceExhausted(Reason::ReferenceExpired)
        );
        assert!(can_restart(error));
        assert_eq!(report.state(), State::Closed);
        assert_eq!(receiver.state(), ReceiveState::NeedsRecovery);
        assert_eq!(link.c.usage(), used);
        assert_eq!(link.c.next_channel_binding().unwrap(), next);
        assert_eq!(link.c.remaining_channel_pairs(), 1);
        assert_eq!(
            report.service(&cx, &mut link.c, &mut receiver, || true),
            Err(ReportError::Closed)
        );
        // The caller, not this reporter, owns the original parent/worker cleanup.
        assert!(!link.c.is_closed());
    });
}
#[test]
fn exhausted_roles_do_not_turn_an_actual_decode_failure_into_reconnect_permission() {
    run_test!(cx, {
        let (mut link, media, mut receiver) = exhausted(&cx).await;
        let mut report = media
            .recovery_receiver(&link.c, link.cr, parent(), &receiver)
            .unwrap();
        let now = net::clock(&cx);
        partial(&mut receiver, &media, now, true);
        let picture = receiver.take_decodable(now).unwrap().unwrap();
        assert_eq!(
            receiver.acknowledge_decode(&picture, false, now),
            Err(DeliveryError::DecodeFailed)
        );
        drop(picture);
        let used = link.c.usage();
        let error = report
            .service(&cx, &mut link.c, &mut receiver, || true)
            .unwrap_err();
        assert_eq!(error, ReportError::NamespaceExhausted(Reason::DecodeFailed));
        assert!(!can_restart(error));
        assert_eq!(link.c.usage(), used);
    });
}
#[test]
fn permission_refusal_wins_over_namespace_exhaustion_and_never_grants_retry() {
    run_test!(cx, {
        let (mut link, media, mut receiver) = exhausted(&cx).await;
        let mut report = media
            .recovery_receiver(&link.c, link.cr, parent(), &receiver)
            .unwrap();
        let error = report
            .service(&cx, &mut link.c, &mut receiver, || false)
            .unwrap_err();
        assert_eq!(error, ReportError::Transport(quic::Error::Unauthorized));
        assert!(!can_restart(error));
    });
}

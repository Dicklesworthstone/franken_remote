//! Original UDP/TLS sessions and source IPC with the production metrics owners.
//! Codec bytes and decode completions are fixtures. A delayed completion retains
//! its actual receiver obligation/bytes; metrics measure that elapsed wait.
use super::*;
use crate::media::receiver_feedback::{self as metrics, ViewerFeedback};
use fr_media::pacing::{Reason, ReceiverEvidence};

impl Member {
    fn enable_feedback(&mut self, decode_delay_us: u64) {
        assert!(self.feedback.is_none());
        let host = self.shared.as_ref().unwrap();
        let setup = metrics::Setup::selected(
            host.session.selection(),
            host.session.binding(),
            self.media.binding(),
        )
        .unwrap()
        .unwrap();
        let routes = self.viewer.io().unwrap().1;
        self.feedback = Some(
            ViewerFeedback::new(
                &setup,
                Route::Stream(routes.inbound),
                Route::Stream(routes.outbound),
            )
            .unwrap(),
        );
        self.decode_delay_us = decode_delay_us;
    }

    pub(super) fn decode_pictures(&mut self) {
        loop {
            let current = now(&self.c).unwrap();
            if self
                .pending_decode
                .as_ref()
                .is_some_and(|(_, until)| current < *until)
            {
                return;
            }
            if let Some((picture, _)) = self.pending_decode.take() {
                let frame = self
                    .receiver
                    .complete_decode(&picture, current)
                    .unwrap()
                    .descriptor()
                    .frame;
                if let Some(feedback) = &mut self.feedback {
                    feedback.complete(current).unwrap();
                }
                self.first.get_or_insert(frame);
                self.frames.push(frame);
            }
            let Some(picture) = self.receiver.take_decodable(current).unwrap() else {
                return;
            };
            if let Some(feedback) = &mut self.feedback {
                feedback.begin(current).unwrap();
            }
            self.pending_decode =
                Some((picture, current.checked_add(self.decode_delay_us).unwrap()));
        }
    }

    pub(super) fn service_feedback(&mut self) {
        let Some(feedback) = &mut self.feedback else {
            return;
        };
        let q = self.viewer.io().unwrap().0;
        q.receive_ready(
            &self.c,
            || true,
            |_| true,
            |route, bytes| {
                if metrics::is_feedback(bytes) {
                    feedback
                        .receive(route, bytes, &self.receiver, now(&self.c).unwrap())
                        .unwrap();
                    Ok(Disposition::Consumed)
                } else {
                    Ok(Disposition::Blocked)
                }
            },
        )
        .unwrap();
        feedback.service(q, &self.c, now(&self.c).unwrap()).unwrap();
    }
}

#[test]
fn shared_decoder_load_reduces_capture_then_probes_after_the_original_decoder_catches_up() {
    let rt = support::runtime();
    let cleanup_cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(Box::pin(async {
        // The one-off source consent starts before worker/bootstrap readiness.
        // Cover that bounded startup plus the full slowdown/probe/renewal run;
        // each peer still uses its original three-second renewable authority.
        let mut group = Box::pin(group_with_source_lifetime(
            &rt,
            1,
            &[fr_wire::receiver_metrics::CAPABILITY],
            true,
            SendPolicy::default(),
            HostDuration::from_micros(8_000_000),
        ))
        .await;
        ready(&mut group).await;
        group.peers[0].enable_feedback(125_000);
        group
            .publisher
            .enable_adaptive_capture(Duration::from_millis(50), Duration::from_millis(200))
            .unwrap();
        let worker = group.publisher.worker_id();
        let started = now(&group.peers[0].h).unwrap();
        let mut captures = 0;
        let source = group.publisher.serve(Duration::from_millis(50), |report| {
            assert!(!report.unchanged);
            captures += 1;
        });
        let network = async {
            let peer = &mut group.peers[0];
            while now(&peer.h).unwrap() < started + 3_800_000 {
                if now(&peer.h).unwrap() >= started + 800_000 {
                    // No replacement, fabricated completion or new sample TTL.
                    // The already accepted slow obligation keeps its deadline.
                    peer.decode_delay_us = 0;
                }
                peer.turn().await.unwrap();
            }
            assert!(peer.frames.len() > 8);
            assert!(peer.feedback.as_ref().unwrap().sent > 4);
            let host = peer.shared.as_mut().unwrap();
            assert!(host.statistics().feedback_reports > 4);
            assert!(host.renewed_until().unwrap().as_micros() > started + 3_000_000);
            assert!(peer.control.check().is_ok());
            host.close();
        };
        let (result, ()) = Box::pin(support::both(source, network)).await;
        assert_eq!(result, Err(crate::media::shared_publisher::Error::Closed));
        assert!(captures > 8);
        let pacing = group.publisher.pacing().unwrap();
        assert!(
            pacing.decisions().any(|report| matches!(
                report.reason,
                Reason::DecoderWork | Reason::ReceiverBacklog
            ))
        );
        assert!(
            pacing
                .decisions()
                .any(|report| report.reason == Reason::HeadroomProbe)
        );
        assert!(pacing.interval_us() < 200_000);
        assert!(matches!(
            pacing.report().unwrap().receiver,
            ReceiverEvidence::Measured(_)
        ));
        assert_eq!(group.publisher.worker_id(), worker);
        cleanup(&mut group, &cleanup_cx).await;
    }));
}

#[test]
fn healthy_shared_decoder_keeps_progress_when_a_slow_sibling_leaves() {
    let rt = support::runtime();
    let cleanup_cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(Box::pin(async {
        let mut group = Box::pin(group_with_capabilities(
            &rt,
            2,
            &[fr_wire::receiver_metrics::CAPABILITY],
            true,
        ))
        .await;
        ready(&mut group).await;
        group.peers[0].enable_feedback(125_000);
        group.peers[1].enable_feedback(0);
        group
            .publisher
            .enable_adaptive_capture(Duration::from_millis(50), Duration::from_millis(200))
            .unwrap();
        let worker = group.publisher.worker_id();
        let started = now(&group.peers[0].h).unwrap();
        let source = group.publisher.serve(Duration::from_millis(50), |_| {});
        let network = async {
            let (slow, healthy) = group.peers.split_at_mut(1);
            let slow = &mut slow[0];
            let healthy = &mut healthy[0];
            while now(&healthy.h).unwrap() < started + 500_000 {
                let (a, b) = Box::pin(support::both(slow.turn(), healthy.turn())).await;
                a.unwrap();
                b.unwrap();
            }
            assert!(slow.shared.as_ref().unwrap().statistics().feedback_reports > 0);
            let before = healthy.frames.len();
            slow.shared.as_mut().unwrap().close();
            assert!(slow.control.check().is_err());
            while now(&healthy.h).unwrap() < started + 3_200_000 {
                healthy.turn().await.unwrap();
            }
            assert!(healthy.frames.len() > before + 8);
            let host = healthy.shared.as_mut().unwrap();
            assert!(host.statistics().feedback_reports > 4);
            assert!(host.renewed_until().unwrap().as_micros() > started + 3_000_000);
            assert!(healthy.control.check().is_ok());
            host.close();
        };
        let (result, ()) = Box::pin(support::both(source, network)).await;
        assert_eq!(result, Err(crate::media::shared_publisher::Error::Closed));
        let pacing = group.publisher.pacing().unwrap();
        assert!(
            !pacing.decisions().any(|report| matches!(
                report.reason,
                Reason::DecoderWork | Reason::ReceiverBacklog
            ))
        );
        assert!(matches!(
            pacing.report().unwrap().receiver,
            ReceiverEvidence::Measured(_)
        ));
        assert_eq!(group.publisher.worker_id(), worker);
        cleanup(&mut group, &cleanup_cx).await;
    }));
}

#[test]
fn shared_feedback_bridge_refuses_foreign_connection_and_stale_view_without_revoking_siblings() {
    let rt = support::runtime();
    let cleanup_cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(Box::pin(async {
        let mut group = Box::pin(group(&rt, 2)).await;
        ready(&mut group).await;
        let (first, second) = group.peers.split_at_mut(1);
        let first = &mut first[0];
        let second = &mut second[0];
        let view = first.media.binding();
        let host = first.shared.as_mut().unwrap();
        let foreign = second.shared.as_mut().unwrap().session.io().unwrap().0;
        let member = host.subscriber.as_mut().unwrap();
        assert_eq!(
            member.receiver_unnegotiated(foreign, view),
            Err(crate::media::shared_publisher::Error::ForeignConnection)
        );
        let q = host.session.io().unwrap().0;
        let mut stale = view;
        stale.recovery = stale.recovery.next().unwrap();
        assert_eq!(
            member.receiver_sample(q, stale, None),
            Err(crate::media::shared_publisher::Error::WrongSource)
        );
        let at_us = now(&first.h).unwrap();
        let sample = metrics::SharedSample {
            view,
            at_us,
            until_us: at_us + fr_media::receiver_feedback::SAMPLE_LIFETIME_US,
            load: fr_wire::receiver_metrics::Load {
                retained_bytes: 0,
                retained_pictures: 0,
                decoding: false,
                work_us: Some(0),
            },
        };
        member.receiver_sample(q, view, Some(sample)).unwrap();
        assert_eq!(
            member.receiver_sample(
                q,
                view,
                Some(metrics::SharedSample {
                    view: stale,
                    ..sample
                })
            ),
            Err(crate::media::shared_publisher::Error::WrongSource)
        );
        member.receiver_unnegotiated(q, view).unwrap();
        assert!(member.startup_complete(q).unwrap());
        assert!(first.control.check().is_ok());
        assert!(second.control.check().is_ok());
        assert!(group.owner.check().is_ok());
        cleanup(&mut group, &cleanup_cx).await;
    }));
}

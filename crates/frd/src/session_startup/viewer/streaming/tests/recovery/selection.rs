//! A frame can expire between canonical preflight and decoder dequeue. The
//! original receiver, not a synthesized exception, must admit that failure.
use super::*;
use crate::session_startup::viewer::streaming::recovery as recovery_flow;
use std::cell::Cell;

fn pending(peer: &mut Peer, receiver: &mut ReceivePipeline, cx: &Cx) -> ReceiveConfig {
    let (session, media) = peer.parts().unwrap();
    let cfg = media
        .receiver_config(&session.transport, ReceivePolicy::default())
        .unwrap();
    let stamp = now(cx).unwrap();
    let (_, bytes) = progress(cfg, stamp);
    receiver
        .receive(Channel::MediaConfig, &bytes, stamp)
        .unwrap();
    cfg
}

#[test]
fn reference_expiry_at_dequeue_reports_loss_without_starting_native_work() {
    run(|c, h| async move {
        let (mut host, mut peer, mut receiver, mut watcher, _) = Box::pin(pair(&c, &h)).await;
        pending(&mut peer, &mut receiver, &c);
        let called = Cell::new(false);
        let selected = recovery_flow::admit(
            &mut peer,
            Some(&mut watcher),
            &mut receiver,
            &mut Repair::default(),
            &c,
            |receiver| {
                called.set(true);
                std::thread::sleep(Duration::from_millis(130));
                receiver
                    .take_decodable(now(&c).unwrap())
                    .map_err(media::Error::Receiver)
            },
        )
        .unwrap();
        assert!(called.get());
        assert!(selected.is_none());
        assert_eq!(receiver.state(), ReceiveState::NeedsRecovery);
        assert_eq!(receiver.budget_usage(), BudgetUsage::default());
        assert!(matches!(
            watcher.state(),
            recovery_control::State::Pending | recovery_control::State::Requested
        ));
        assert!(watcher.next_deadline().is_some());
        assert!(peer.parts().is_ok());
        host.close();
        peer.close();
        receiver.close();
        watcher.close();
    });
}

#[test]
fn healthy_selection_keeps_the_original_picture_reservation_until_collection() {
    run(|c, h| async move {
        let (mut host, mut peer, mut receiver, mut watcher, _) = Box::pin(pair(&c, &h)).await;
        let cfg = pending(&mut peer, &mut receiver, &c);
        let descriptor = receiver.latest_progress().unwrap().descriptor;
        receiver
            .receive(
                Channel::Video,
                &fragments(cfg, descriptor),
                now(&c).unwrap(),
            )
            .unwrap();
        let selected = recovery_flow::admit(
            &mut peer,
            Some(&mut watcher),
            &mut receiver,
            &mut Repair::default(),
            &c,
            |receiver| {
                receiver
                    .take_decodable(now(&c).unwrap())
                    .map_err(media::Error::Receiver)
            },
        )
        .unwrap()
        .flatten()
        .unwrap();
        assert_eq!(selected.descriptor().frame, 1);
        assert_eq!(receiver.budget_usage().pictures, 1);
        assert_eq!(watcher.state(), recovery_control::State::Receiving);
        assert_eq!(watcher.next_deadline(), None);
        // This is explicit receiver-contract evidence, not a native decode.
        receiver
            .complete_decode(&selected, now(&c).unwrap())
            .unwrap();
        assert_eq!(receiver.budget_usage().pictures, 1);
        drop(selected);
        assert_eq!(receiver.budget_usage(), BudgetUsage::default());
        host.close();
        peer.close();
        receiver.close();
        watcher.close();
    });
}

#[test]
fn dequeue_failure_without_recovery_owner_remains_terminal() {
    run(|c, h| async move {
        let (mut host, mut peer, mut receiver, mut watcher, _) = Box::pin(pair(&c, &h)).await;
        pending(&mut peer, &mut receiver, &c);
        let selected = recovery_flow::admit(
            &mut peer,
            None,
            &mut receiver,
            &mut Repair::default(),
            &c,
            |receiver| {
                std::thread::sleep(Duration::from_millis(130));
                receiver
                    .take_decodable(now(&c).unwrap())
                    .map_err(media::Error::Receiver)
            },
        );
        assert!(matches!(
            selected,
            Err(Error::Media(media::Error::Receiver(
                DeliveryError::ReferenceExpired
            )))
        ));
        assert_eq!(watcher.state(), recovery_control::State::Receiving);
        assert_eq!(watcher.next_deadline(), None);
        host.close();
        peer.close();
        receiver.close();
        watcher.close();
    });
}

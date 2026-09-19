//! Receiver acceptance races use the exact canonical completion helper and
//! actual negotiated control route. No native/visible completion is invented.
use super::*;
use crate::session_startup::viewer::streaming::recovery as recovery_flow;
use std::{cell::Cell, rc::Rc};

struct Retired(Rc<Cell<bool>>);
impl Drop for Retired {
    fn drop(&mut self) {
        self.0.set(true);
    }
}

fn lose(peer: &mut Peer, receiver: &mut ReceivePipeline, cx: &Cx) {
    let (session, media) = peer.parts().unwrap();
    let cfg = media
        .receiver_config(&session.transport, ReceivePolicy::default())
        .unwrap();
    let stamp = now(cx).unwrap();
    let (_, bytes) = progress(cfg, stamp);
    receiver
        .receive(Channel::MediaConfig, &bytes, stamp)
        .unwrap();
}

#[test]
fn fenced_completion_is_retired_without_receipt_or_deadline_refresh() {
    run(|c, h| async move {
        let (mut host, mut peer, mut receiver, mut watcher, _) = Box::pin(pair(&c, &h)).await;
        lose(&mut peer, &mut receiver, &c);
        let mut repairs = Repair::default();
        asupersync::time::sleep(c.now(), Duration::from_millis(30)).await;
        repairs.prepare(&mut receiver, now(&c).unwrap()).unwrap();
        assert!(repairs.len > 0);
        asupersync::time::sleep(c.now(), Duration::from_millis(100)).await;
        let dropped = Rc::new(Cell::new(false));
        let held = Retired(dropped.clone());
        let receipt = recovery_flow::complete(
            &mut peer,
            Some(&mut watcher),
            &mut receiver,
            &mut repairs,
            &c,
            move |_| {
                let _held = held;
                panic!("a fenced native job must never produce a presentation receipt");
            },
        )
        .unwrap();
        assert!(receipt.is_none());
        assert!(dropped.get());
        assert!(matches!(
            watcher.state(),
            recovery_control::State::Pending | recovery_control::State::Requested
        ));
        assert_eq!(repairs.len, 0);
        assert_eq!(receiver.budget_usage(), BudgetUsage::default());
        let original = watcher.next_deadline().unwrap();
        asupersync::time::sleep(c.now(), Duration::from_millis(10)).await;
        assert!(
            recovery_flow::complete(
                &mut peer,
                Some(&mut watcher),
                &mut receiver,
                &mut repairs,
                &c,
                |_| panic!("duplicate completion cannot revive this chain"),
            )
            .unwrap()
            .is_none()
        );
        assert_eq!(watcher.next_deadline(), Some(original));
        assert!(peer.parts().is_ok());
        host.close();
        peer.close();
        receiver.close();
        watcher.close();
    });
}

#[test]
fn expiry_between_completion_preflight_and_receiver_acceptance_is_reported_once() {
    run(|c, h| async move {
        let (mut host, mut peer, mut receiver, mut watcher, _) = Box::pin(pair(&c, &h)).await;
        lose(&mut peer, &mut receiver, &c);
        let mut repairs = Repair::default();
        let called = Cell::new(false);
        let receipt = recovery_flow::complete(
            &mut peer,
            Some(&mut watcher),
            &mut receiver,
            &mut repairs,
            &c,
            |receiver| {
                called.set(true);
                // Force the otherwise tiny clock-read race, only in this test.
                std::thread::sleep(Duration::from_millis(130));
                Err(media::Error::Receiver(
                    receiver.tick(now(&c).unwrap()).unwrap_err(),
                ))
            },
        )
        .unwrap();
        assert!(called.get());
        assert!(receipt.is_none());
        assert!(matches!(
            watcher.state(),
            recovery_control::State::Pending | recovery_control::State::Requested
        ));
        assert!(watcher.next_deadline().is_some());
        assert_eq!(receiver.state(), ReceiveState::NeedsRecovery);
        assert!(peer.parts().is_ok());
        host.close();
        peer.close();
        receiver.close();
        watcher.close();
    });
}

#[test]
fn completion_without_recovery_negotiation_keeps_terminal_expiry() {
    run(|c, h| async move {
        let (mut host, mut peer, mut receiver, mut watcher, _) = Box::pin(pair(&c, &h)).await;
        lose(&mut peer, &mut receiver, &c);
        asupersync::time::sleep(c.now(), Duration::from_millis(130)).await;
        let result = recovery_flow::complete(
            &mut peer,
            None,
            &mut receiver,
            &mut Repair::default(),
            &c,
            |_| panic!("expired completion cannot bypass capability negotiation"),
        );
        assert!(matches!(
            result,
            Err(Error::Delivery(DeliveryError::ReferenceExpired))
        ));
        assert_eq!(watcher.state(), recovery_control::State::Receiving);
        assert_eq!(watcher.next_deadline(), None);
        host.close();
        peer.close();
        receiver.close();
        watcher.close();
    });
}

#[test]
fn unrelated_completion_errors_cannot_fabricate_recovery_or_suppress_failure() {
    run(|c, h| async move {
        let (mut host, mut peer, mut receiver, mut watcher, _) = Box::pin(pair(&c, &h)).await;
        for failure in [
            media::Error::InvalidFrame,
            media::Error::Receiver(DeliveryError::DecodeFailed),
        ] {
            let result = recovery_flow::complete(
                &mut peer,
                Some(&mut watcher),
                &mut receiver,
                &mut Repair::default(),
                &c,
                |_| Err(failure),
            );
            assert!(matches!(result, Err(Error::Media(actual)) if actual == failure));
            assert_eq!(receiver.state(), ReceiveState::Streaming);
            assert_eq!(watcher.state(), recovery_control::State::Receiving);
            assert_eq!(watcher.next_deadline(), None);
        }
        host.close();
        peer.close();
        receiver.close();
        watcher.close();
    });
}

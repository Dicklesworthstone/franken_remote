//! Real child IPC exercises split ownership, not HEVC or display qualification.
use super::*;
use std::{
    future::{Future, poll_fn},
    pin::pin,
    task::Poll,
};

#[test]
fn network_recovery_admission_does_not_wait_for_an_in_flight_native_capture() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (control, input) = control(&cx, 9);
        let mut source = source(&control, "delay-unchanged").await;
        let mut subscription = subscription(&control, &mut source, 2_000_000).await;
        let target = subscription.recovery_target(&source).unwrap();
        let original_worker = source.worker_id();
        let mut bytes = [0; 1150];
        let old = subscription.next_packet(&mut bytes).unwrap().unwrap();
        {
            // This mutable source borrow spans the actual pending IPC exchange.
            // Admission below cannot borrow, replace or wait for that worker.
            let mut capture = pin!(source.capture_if_changed(&control, false));
            poll_fn(|task| match capture.as_mut().poll(task) {
                Poll::Pending => Poll::Ready(()),
                Poll::Ready(_) => panic!("delayed native capture must be pending"),
            })
            .await;
            assert!(
                target
                    .request(&mut subscription, &request(binding()), binding())
                    .unwrap()
            );
            let until = subscription.next_deadline();
            assert!(
                !target
                    .request(&mut subscription, &request(binding()), binding())
                    .unwrap()
            );
            assert_eq!(subscription.next_deadline(), until);
            assert!(subscription.authorize_write(&old).is_err());
            assert!(!control.view_ready().unwrap());
            assert!(
                input
                    .monitor()
                    .authorize_ticket(InputTicketId::from_raw(1), host_now(&cx).unwrap())
                    .is_err()
            );
            // Already-issued work is not relabelled as an IDR or fresh recovery.
            assert!(capture.await.unwrap().is_unchanged());
        }
        assert!(source.next_recovery_deadline().is_some());
        let update = source.capture_if_changed(&control, false).await.unwrap();
        assert!(update.encoded().unwrap().is_idr());
        assert_eq!(source.worker_id(), original_worker);
        assert_eq!(source.next_recovery_deadline(), None);
        subscription
            .recover(
                MediaEpoch {
                    configuration: binding().configuration,
                    recovery: binding().recovery.next().unwrap(),
                },
                MediaBindings::new(11, 12, 13, 14).unwrap(),
            )
            .unwrap();
        subscription.enqueue_capture(update).unwrap();
        assert_eq!(
            subscription
                .next_packet(&mut bytes)
                .unwrap()
                .unwrap()
                .channel(),
            Channel::MediaConfig
        );
        assert_eq!(
            subscription
                .next_packet(&mut bytes)
                .unwrap()
                .unwrap()
                .channel(),
            Channel::Recovery
        );
        assert!(!control.view_ready().unwrap());
        stop(&mut source, &cx).await;
    });
}

#[test]
fn unrestricted_worker_access_retires_existing_recovery_handles() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (control, input) = control(&cx, 9);
        let mut source = source(&control, "healthy").await;
        let mut subscription = subscription(&control, &mut source, 2_000_000).await;
        let target = subscription.recovery_target(&source).unwrap();
        let worker = source.worker_id();
        let _ = source.worker_mut();
        assert_eq!(source.worker_id(), worker);
        assert_eq!(
            target.request(&mut subscription, &request(binding()), binding()),
            Err(Error::InvalidFrame)
        );
        assert!(matches!(
            subscription.recovery_target(&source),
            Err(Error::InvalidFrame)
        ));
        assert_eq!(source.next_recovery_deadline(), None);
        assert!(control.view_ready().unwrap());
        input
            .monitor()
            .authorize_ticket(InputTicketId::from_raw(1), host_now(&cx).unwrap())
            .unwrap();
        stop(&mut source, &cx).await;
    });
}

#[test]
fn equal_numeric_frames_on_another_worker_do_not_authorize_a_target() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (control, input) = control(&cx, 9);
        let mut source = source(&control, "healthy").await;
        let mut foreign = self::source(&control, "healthy").await;
        let subscription = subscription(&control, &mut source, 2_000_000).await;
        let mut other = self::subscription(&control, &mut foreign, 2_000_000).await;
        assert!(matches!(
            subscription.recovery_target(&foreign),
            Err(Error::InvalidFrame)
        ));
        let target = subscription.recovery_target(&source).unwrap();
        assert_eq!(
            target.request(&mut other, &request(binding()), binding()),
            Err(Error::InvalidFrame)
        );
        assert_eq!(source.next_recovery_deadline(), None);
        assert_eq!(foreign.next_recovery_deadline(), None);
        assert!(control.view_ready().unwrap());
        input
            .monitor()
            .authorize_ticket(InputTicketId::from_raw(1), host_now(&cx).unwrap())
            .unwrap();
        stop(&mut source, &cx).await;
        stop(&mut foreign, &cx).await;
    });
}

#[test]
fn a_handle_cannot_keep_a_dropped_capture_owner_alive() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (control, input) = control(&cx, 9);
        let mut source = source(&control, "healthy").await;
        let mut subscription = subscription(&control, &mut source, 2_000_000).await;
        let target = subscription.recovery_target(&source).unwrap();
        stop(&mut source, &cx).await;
        drop(source);
        assert_eq!(
            target.request(&mut subscription, &request(binding()), binding()),
            Err(Error::InvalidFrame)
        );
        assert!(control.view_ready().unwrap());
        input
            .monitor()
            .authorize_ticket(InputTicketId::from_raw(1), host_now(&cx).unwrap())
            .unwrap();
    });
}

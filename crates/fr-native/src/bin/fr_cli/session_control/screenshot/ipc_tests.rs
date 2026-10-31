//! Actual abstract sockets and the actual mailbox. Synthetic pixel fixtures
//! qualify framing and owner identity only, never a decoder or live desktop.
use super::*;
use crate::session_control::{Owner, hex};
use asupersync::{runtime::RuntimeBuilder, types::Budget};
use std::{
    future::{Future, poll_fn},
    pin::pin,
};

async fn beside<T>(
    owner: &mut Owner,
    cx: &Cx,
    channel: &fr_client::robot::snapshot::Channel,
    work: impl Future<Output = T>,
) -> T {
    let mut stop = pin!(owner.wait_for_stop(cx));
    let mut work = pin!(work);
    poll_fn(|task| {
        assert!(
            stop.as_mut().poll(task).is_pending(),
            "screenshot requested STOP"
        );
        if let Some(pending) = channel.take() {
            pending.complete(Ok(image()));
        }
        work.as_mut().poll(task)
    })
    .await
}
#[test]
fn original_owner_transfers_one_sealed_image_without_stop_or_new_session() {
    let runtime = RuntimeBuilder::current_thread()
        .enable_platform_reactor(true)
        .build()
        .unwrap();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let mut random = [0; 16];
        cx.random_bytes(&mut random);
        let target = format!("screenshot-{}", hex(&random));
        let publisher = observation::Publisher::default();
        let (channel, handle) = fr_client::robot::snapshot::Channel::new();
        channel.publish(Some(image().stamp()));
        publisher
            .update(|state| {
                state.begin(1)?;
                state.authenticated()?;
                Ok(())
            })
            .unwrap();
        let source = metadata(false).observation.frame.unwrap();
        publisher
            .update(|state| {
                state.ready(
                    Display {
                        handle: 17,
                        geometry: DisplayGeometryGeneration::INITIAL,
                        x: source.origin_x,
                        y: source.origin_y,
                        pixel_width: 800,
                        pixel_height: 600,
                        logical_width: 800,
                        logical_height: 600,
                        scale_numerator: 1,
                        scale_denominator: 1,
                        rotation: 0,
                    },
                    0,
                    Evidence::SubmittedToCompositor,
                    Instant::now(),
                )
            })
            .unwrap();
        publisher.bind_snapshot(handle).unwrap();
        let mut owner = Owner::bind(&cx, &target)
            .await
            .unwrap()
            .with_observations(publisher);
        let id = owner.info().instance;
        let observed = beside(&mut owner, &cx, &channel, read(&cx, &target, &id))
            .await
            .unwrap();
        assert_eq!(observed.info.instance, id);
        assert_eq!(observed.snapshot.pixels(), image().pixels());
        assert_eq!(observed.metadata.validate_image(&observed.snapshot), Ok(()));
        assert!(owner.requester.is_none());
        assert!(!cx.is_cancel_requested());
        assert!(matches!(
            beside(
                &mut owner,
                &cx,
                &channel,
                read(&cx, &target, &"ff".repeat(16))
            )
            .await,
            Err(ReadError::Local(Error::WrongOwner))
        ));
    });
}
#[test]
fn ordinary_owner_has_typed_absence_and_bad_pin_never_opens_a_request() {
    let runtime = RuntimeBuilder::current_thread()
        .enable_platform_reactor(true)
        .build()
        .unwrap();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let mut random = [0; 16];
        cx.random_bytes(&mut random);
        let target = format!("screenshot-{}", hex(&random));
        let (channel, _handle) = fr_client::robot::snapshot::Channel::new();
        let mut owner = Owner::bind(&cx, &target).await.unwrap();
        let id = owner.info().instance;
        assert!(matches!(
            beside(&mut owner, &cx, &channel, read(&cx, &target, &id)).await,
            Err(ReadError::Refused(Refusal::Unavailable))
        ));
        assert!(matches!(
            read(&cx, &target, "bad").await,
            Err(ReadError::Local(Error::InvalidInstance))
        ));
        assert!(owner.requester.is_none());
    });
}

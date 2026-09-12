use super::*;
use crate::session_startup::running::tests::{pair_initialized, run};
use fr_wire::negotiation::Capability;
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

fn assert_send<T: Send>(_: &T) {}

fn caps() -> Vec<Capability> {
    let mut caps: Vec<_> = [
        fr_wire::display::CAPABILITY,
        fr_wire::decoder::CAPABILITY,
        fr_wire::attachment::CAPABILITY,
        fr_wire::attachment::DELIVERY_CAPABILITY,
    ]
    .into_iter()
    .map(|name| Capability {
        name: name.into(),
        version: 1,
        required: true,
    })
    .collect();
    caps.sort_by(|a, b| a.name.cmp(&b.name));
    caps
}
fn launch() -> Launch {
    Launch::new(
        Path::new("/usr/bin/false"),
        ":0",
        None,
        fr_media::worker::Role::Capture,
        151,
    )
    .unwrap()
}
#[test]
fn invalid_policy_and_unpolled_publication_never_call_native_configuration() {
    for invalid in [false, true] {
        run(|c, h| async move {
            let (host, _viewer) = pair_initialized(&c, &h, caps(), |_| {}).await;
            let control = host.opened.control.clone();
            let called = Arc::new(AtomicBool::new(false));
            let check = called.clone();
            let policy = if invalid {
                Policy {
                    timeout: Duration::ZERO,
                    ..Policy::default()
                }
            } else {
                Policy::default()
            };
            let future = host.publish_display(
                launch(),
                policy,
                move |_| {
                    check.store(true, Ordering::Release);
                    Err(())
                },
                || Ok(33),
            );
            assert_send(&future);
            if invalid {
                assert!(matches!(future.await, Err(Error::InvalidConfiguration)));
            } else {
                drop(future);
            }
            assert!(!called.load(Ordering::Acquire));
            assert!(control.check().is_err());
            assert!(c.checkpoint().is_ok());
        });
    }
}
#[test]
fn time_before_first_poll_is_part_of_the_original_publication_budget() {
    run(|c, h| async move {
        let (host, _viewer) = pair_initialized(&c, &h, caps(), |_| {}).await;
        let control = host.opened.control.clone();
        let future = host.publish_display(
            launch(),
            Policy {
                timeout: Duration::from_millis(1),
                ..Policy::default()
            },
            |_| panic!("no codec policy before live native discovery"),
            || panic!("no entropy before expired admission"),
        );
        asupersync::time::sleep(c.now(), Duration::from_millis(5)).await;
        assert!(matches!(future.await, Err(Error::Expired)));
        assert!(control.check().is_err());
    });
}
#[test]
fn control_intent_refuses_before_spawning_the_source() {
    run(|c, h| async move {
        let (host, _viewer) = pair_initialized(&c, &h, caps(), |_| {}).await;
        assert!(matches!(
            host.publish_display(
                launch(),
                Policy::default(),
                |_| panic!("missing capability cannot configure"),
                || panic!("missing capability cannot mint tickets")
            )
            .await,
            Err(Error::InvalidConfiguration)
        ));
    });
}
#[test]
fn policy_bounds_do_not_enable_faster_capture_or_unbounded_adaptation() {
    let base = Policy::default();
    assert!(base.validate().is_ok());
    for policy in [
        Policy {
            timeout: Duration::from_secs(61),
            ..base
        },
        Policy {
            network_turn: Duration::ZERO,
            ..base
        },
        Policy {
            adaptive_capture: Some(Duration::from_millis(201)),
            ..base
        },
        Policy {
            adaptive_capture: Some(Duration::from_millis(1)),
            ..base
        },
        Policy {
            streaming: streaming::Policy {
                records_per_turn: 65,
                ..base.streaming
            },
            ..base
        },
    ] {
        assert_eq!(policy.validate(), Err(Error::InvalidConfiguration));
    }
}

#[test]
fn ready_native_result_does_not_cancel_the_original_pending_network_turn() {
    run(|c, h| async move {
        let (mut host, mut viewer) = pair_initialized(&c, &h, caps(), |_| {}).await;
        let control = host.opened.control.clone();
        let budget = Budget::new(control.clone(), Policy::default()).unwrap();
        let mut n = 200;
        let mut polled = false;
        let result = during(
            &mut host,
            &budget,
            &mut || {
                n += 1;
                Ok(n)
            },
            poll_fn(|task| {
                if polled {
                    Poll::Ready(Ok(37))
                } else {
                    polled = true;
                    task.waker().wake_by_ref();
                    Poll::Pending
                }
            }),
        )
        .await
        .unwrap();
        assert_eq!(result, 37);
        assert!(control.check().is_ok());
        let (a, b) = Box::pin(crate::session_startup::tests::support::both(
            host.drive(
                Duration::from_millis(1),
                || {
                    n += 1;
                    Ok(n)
                },
                block,
            ),
            viewer.drive(Duration::from_millis(1), block),
        ))
        .await;
        a.unwrap();
        b.unwrap();
    });
}
#[test]
fn original_budget_fences_before_abandoning_a_waiting_native_continuation() {
    struct Pending {
        control: ObservationControl,
        dropped: Arc<AtomicBool>,
    }
    impl Future for Pending {
        type Output = Result<(), media::Error>;
        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
            Poll::Pending
        }
    }
    impl Drop for Pending {
        fn drop(&mut self) {
            assert!(
                self.control.check().is_err(),
                "native work dropped before observation fence"
            );
            self.dropped.store(true, Ordering::Release);
        }
    }
    run(|c, h| async move {
        let (mut host, _viewer) = pair_initialized(&c, &h, caps(), |_| {}).await;
        let control = host.opened.control.clone();
        let dropped = Arc::new(AtomicBool::new(false));
        let budget = Budget::new(
            control.clone(),
            Policy {
                timeout: Duration::from_millis(15),
                ..Policy::default()
            },
        )
        .unwrap();
        let result = during(
            &mut host,
            &budget,
            &mut || Ok(44),
            Pending {
                control,
                dropped: dropped.clone(),
            },
        )
        .await;
        assert!(matches!(result, Err(Error::Expired)));
        assert!(dropped.load(Ordering::Acquire));
    });
}

//! Real localhost UDP/TLS and production startup/renewal/selection owners.
//! Host identity and OS catalog are explicit fixtures, not Tailscale or hardware.
use super::*;

async fn publish(host: Host, c: &Cx, catalog: &Catalog, delay: Duration) -> bool {
    let mut host = Box::pin(ready_host(host)).await;
    let control = host.observation().unwrap();
    let initial = control.deadline(Duration::from_secs(3)).unwrap().time();
    let h = control.context();
    let until = now(&h).unwrap() + u64::try_from(delay.as_micros()).unwrap();
    let mut nonce = 18000;
    while now(&h).unwrap() < until {
        host_turn(&mut host, &mut nonce).await.unwrap();
    }
    let renewed = control.deadline(Duration::from_secs(3)).unwrap().time() != initial;
    let mut selection = host
        .select_display(*catalog, Duration::from_secs(2))
        .unwrap();
    while c.checkpoint().is_ok() {
        selection.transmit(host.io().unwrap().0).unwrap();
        selection.dispatch(host.io().unwrap().0).unwrap();
        assert!(
            !selection.is_complete(),
            "inspection must not select a screen"
        );
        if host_turn(&mut host, &mut nonce).await.is_err() {
            break;
        }
    }
    assert!(!selection.is_complete());
    renewed
}

#[test]
fn inventory_preserves_empty_and_maximum_catalogs_without_selecting_or_starting_media() {
    for count in [0, 1, display::MAX_DISPLAYS] {
        run(|c, h| async move {
            let (host, viewer) = initial(&c, &h, false).await;
            let mut entries = Vec::new();
            for i in 0..count {
                let mut entry = catalog().displays()[0];
                entry.handle = u128::MAX - u128::try_from(i).unwrap();
                entries.push(entry);
            }
            let expected = Catalog::new(31, &entries, &ProtocolLimits::ABSOLUTE).unwrap();
            let (result, _) = Box::pin(support::both(
                viewer.inspect_displays(Policy::default(), |_| panic!("not requested")),
                publish(host, &c, &expected, Duration::ZERO),
            ))
            .await;
            let actual = result.unwrap();
            assert_eq!(actual, expected);
            assert!(
                c.checkpoint().is_err(),
                "successful inspection must close its session"
            );
        });
    }
}
#[test]
fn inventory_keeps_observation_renewal_alive_while_waiting_for_catalog() {
    run(|c, h| async move {
        let (host, viewer) = initial(&c, &h, false).await;
        let (result, renewed) = Box::pin(support::both(
            viewer.inspect_displays(Policy::default(), |_| Ok(())),
            publish(host, &c, &catalog(), Duration::from_millis(1100)),
        ))
        .await;
        assert_eq!(result.unwrap(), catalog());
        assert!(renewed);
    });
}
#[test]
fn inventory_approval_notification_cannot_authorize_catalog_disclosure() {
    run(|c, h| async move {
        let (mut host, viewer) = initial(&c, &h, true).await;
        let notices = AtomicUsize::new(0);
        let attempt = viewer.inspect_displays(
            Policy {
                timeout: Duration::from_millis(120),
                ..Policy::default()
            },
            |_| {
                notices.fetch_add(1, Ordering::Relaxed);
                Ok(())
            },
        );
        let serve = async {
            while c.checkpoint().is_ok() {
                if host.drive(Duration::from_millis(2)).await.is_err() {
                    break;
                }
                assert!(!host.is_complete());
                assert!(host.observation_until.is_none());
            }
        };
        let (result, ()) = Box::pin(support::both(attempt, serve)).await;
        assert!(result.is_err());
        assert_eq!(notices.load(Ordering::Relaxed), 1);
        assert!(c.checkpoint().is_err());
    });
}
#[test]
fn inventory_receives_catalog_only_after_the_host_approves_the_original_session() {
    run(|c, h| async move {
        let (mut host, viewer) = initial(&c, &h, true).await;
        let noticed = AtomicBool::new(false);
        let request = viewer.inspect_displays(Policy::default(), |_| {
            noticed.store(true, Ordering::Release);
            Ok(())
        });
        let serve = async {
            while !noticed.load(Ordering::Acquire) {
                host.drive(Duration::from_millis(2)).await.unwrap();
                assert!(!host.is_complete());
            }
            host.approval().unwrap().decide(true).unwrap();
            Box::pin(publish(host, &c, &catalog(), Duration::ZERO)).await;
        };
        let (result, ()) = Box::pin(support::both(request, serve)).await;
        assert_eq!(result.unwrap(), catalog());
    });
}
#[test]
fn inventory_invalid_unpolled_and_expired_attempts_stop_only_their_original_viewer() {
    for mode in 0..3 {
        run(|c, h| async move {
            let (_host, viewer) = initial(&c, &h, false).await;
            let future = viewer.inspect_displays(
                Policy {
                    timeout: if mode == 0 {
                        Duration::ZERO
                    } else {
                        Duration::from_millis(10)
                    },
                    ..Policy::default()
                },
                |_| panic!("no approval callback"),
            );
            match mode {
                0 => assert_eq!(future.await, Err(Error::InvalidConfiguration)),
                1 => drop(future),
                _ => {
                    asupersync::time::sleep(c.now(), Duration::from_millis(20)).await;
                    assert_eq!(future.await, Err(Error::Expired));
                }
            }
            assert!(c.checkpoint().is_err());
            assert!(h.checkpoint().is_ok());
        });
    }
}
#[test]
fn inventory_callback_failure_closes_without_minting_an_approval() {
    run(|c, h| async move {
        let (mut host, viewer) = initial(&c, &h, true).await;
        let request = viewer.inspect_displays(Policy::default(), |_| Err(()));
        let serve = async {
            while c.checkpoint().is_ok() {
                if host.drive(Duration::from_millis(2)).await.is_err() {
                    break;
                }
                assert!(!host.is_complete());
            }
        };
        let (result, ()) = Box::pin(support::both(request, serve)).await;
        assert_eq!(result, Err(Error::Application));
        assert!(c.checkpoint().is_err());
    });
}

#[test]
fn inventory_callback_panic_fences_before_a_retained_failed_future_is_dropped() {
    run(|c, h| async move {
        let (mut host, viewer) = initial(&c, &h, true).await;
        let mut request = Box::pin(viewer.inspect_displays(Policy::default(), |_| {
            panic!("explicit local approval notification panic")
        }));
        let failing = poll_fn(|task| {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                request.as_mut().poll(task)
            })) {
                Ok(Poll::Pending) => Poll::Pending,
                Ok(Poll::Ready(_)) => panic!("notification must panic"),
                Err(_) => {
                    assert!(c.checkpoint().is_err(), "failed future is still retained");
                    Poll::Ready(())
                }
            }
        });
        let serve = async {
            while c.checkpoint().is_ok() {
                if host.drive(Duration::from_millis(2)).await.is_err() {
                    break;
                }
                assert!(!host.is_complete());
            }
        };
        Box::pin(support::both(failing, serve)).await;
        assert!(c.checkpoint().is_err());
        drop(request);
    });
}
#[test]
fn inventory_rejects_a_catalog_from_another_remote_session() {
    run(|c, h| async move {
        let (host, viewer) = initial(&c, &h, false).await;
        let request = viewer.inspect_displays(Policy::default(), |_| Ok(()));
        let serve = async {
            let mut host = Box::pin(ready_host(host)).await;
            let foreign = ControlBinding {
                id: 7,
                host_boot: HostBootId::from_raw(11),
                os_session: OsSessionId::from_raw(12),
                remote_session: RemoteSessionId::from_raw(14),
            };
            let mut bytes = [0; display::MAX_CATALOG_BYTES];
            let n = display::encode(
                &display::Message::Catalog(catalog()),
                foreign,
                &ProtocolLimits::ABSOLUTE,
                &mut bytes,
                InputDirection::HostToViewer,
                InputDelivery::Reliable,
            )
            .unwrap();
            let (q, routes) = host.io().unwrap();
            q.send(
                &h,
                Route::Stream(routes.outbound),
                &bytes[..n],
                now(&h).unwrap() + 500_000,
                || true,
            )
            .unwrap();
            let mut nonce = 23000;
            while c.checkpoint().is_ok() {
                if host_turn(&mut host, &mut nonce).await.is_err() {
                    break;
                }
            }
        };
        let (result, ()) = Box::pin(support::both(request, serve)).await;
        assert!(
            matches!(
                result,
                Err(Error::Display(crate::display_selection::Error::Wire(_)))
            ),
            "{result:?}"
        );
        assert!(c.checkpoint().is_err());
    });
}

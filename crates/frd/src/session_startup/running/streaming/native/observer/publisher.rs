//! Both PUBLIC application bootstraps. Real discovery, selected capture, HEVC,
//! process supervision, TLS/QUIC, and X11 pixels; only tailnet/approval are fixtures.
use super::*;
use crate::session_startup::{NativePublisher, PublisherError, PublisherPolicy};

async fn publish(
    mut host: StartupHost,
    source: &Display,
    image: &Path,
    notices: &AtomicUsize,
    policy: PublisherPolicy,
    selected: &AtomicUsize,
) -> Result<NativePublisher, PublisherError> {
    while !host.is_complete() {
        host.drive(Duration::from_millis(1))
            .await
            .map_err(PublisherError::Session)?;
        if notices.load(Ordering::Acquire) != 0
            && let Some(approval) = host.approval()
        {
            approval.decide(true).unwrap();
        }
    }
    let host = host.finish().unwrap().into_running().unwrap();
    let mut nonce_value = 9000;
    host.publish_display(
        source.launch(image, Role::Capture, 161),
        policy,
        |display| {
            selected.fetch_add(1, Ordering::AcqRel);
            let cfg = configuration();
            assert_eq!(
                (display.pixel_width, display.pixel_height),
                (cfg.width, cfg.height)
            );
            Ok(cfg)
        },
        || nonce(&mut nonce_value),
    )
    .await
}
fn image() -> PathBuf {
    PathBuf::from(std::env::var_os("FR_NATIVE_TEST_WORKER").expect("build real worker"))
        .canonicalize()
        .unwrap()
}

#[test]
#[ignore = "explicit native lane requires FR_NATIVE_TEST_WORKER and Xvfb"]
fn public_publisher_and_observer_discover_select_decode_and_stream_the_same_native_workers() {
    let image = image();
    run(|c, h| async move {
        let cleanup = Cx::current().unwrap();
        let mut source = Display::start();
        let mut target = Display::start();
        source.paint(0x0050_3060);
        let (host, viewer) = startup(&c, &h).await;
        let notices = AtomicUsize::new(0);
        let configurations = AtomicUsize::new(0);
        let choices = AtomicUsize::new(0);
        let (host, viewer) = Box::pin(support::both(
            publish(
                host,
                &source,
                &image,
                &notices,
                PublisherPolicy {
                    adaptive_capture: Some(Duration::from_millis(200)),
                    ..PublisherPolicy::default()
                },
                &configurations,
            ),
            viewer.observe(
                target.launch(&image, Role::Present, 162),
                ObserverPolicy::default(),
                |catalog| {
                    assert_eq!(catalog.displays().len(), 1);
                    choices.fetch_add(1, Ordering::AcqRel);
                    Ok(Some(catalog.displays()[0].handle))
                },
                |_| {
                    notices.fetch_add(1, Ordering::AcqRel);
                    Ok(())
                },
            ),
        ))
        .await;
        let mut host = host.unwrap();
        let mut viewer = viewer.unwrap();
        assert_eq!(notices.load(Ordering::Acquire), 1);
        assert_eq!(choices.load(Ordering::Acquire), 1);
        assert_eq!(configurations.load(Ordering::Acquire), 1);
        assert_eq!(host.display(), viewer.display());
        target.assert_pixel(0x0050_3060);
        let source_id = host.worker_id();
        let decoder_id = viewer.worker_id();
        assert!(source_id.is_some() && decoder_id.is_some());
        let started = now(&c).unwrap();
        let viewer_stop = viewer.control();
        let stop = host.control();
        let mut seen = 0;
        let mut n = 20000;
        source.paint(0x0020_5060);
        let (a, b) = Box::pin(support::both(
            host.serve(|| nonce(&mut n)),
            viewer.serve(|frame| {
                if let Some(frame) = frame {
                    assert_eq!(frame.stage, media::PresentationStage::SubmittedToCompositor);
                    target.assert_pixel(0x0020_5060);
                    seen += 1;
                }
                if now(&c).unwrap() - started > 3_200_000 {
                    viewer_stop.stop();
                    stop.revoke();
                }
                Ok(())
            }),
        ))
        .await;
        assert!(a.is_err() && b.is_err());
        assert!(
            support::clock(&c) >= started + 3_200_000,
            "early closure: {a:?} {b:?}"
        );
        assert!(seen >= 1);
        assert_eq!(host.worker_id(), source_id);
        assert_eq!(viewer.worker_id(), decoder_id);
        assert_eq!(host.statistics().encoded_updates, 1);
        assert!(host.statistics().unchanged_observations > 4);
        assert!(
            host.pacing()
                .unwrap()
                .decisions()
                .any(|r| r.reason == fr_media::pacing::Reason::VerifiedIdle)
        );
        host.reap_media(
            &cleanup,
            Deadline::after(&cleanup, Duration::from_secs(2)).unwrap(),
        )
        .await
        .unwrap();
        viewer
            .reap_media(
                &cleanup,
                Deadline::after(&cleanup, Duration::from_secs(2)).unwrap(),
            )
            .await
            .unwrap();
    });
}

#[test]
#[ignore = "explicit native lane requires FR_NATIVE_TEST_WORKER and Xvfb"]
fn live_discovery_with_no_remote_choice_expires_without_configuring_or_sending_pixels() {
    let image = image();
    run(|c, h| async move {
        let source = Display::start();
        let target = Display::start();
        let (host, viewer) = startup(&c, &h).await;
        let notices = AtomicUsize::new(0);
        let configured = AtomicUsize::new(0);
        let catalogs = AtomicUsize::new(0);
        let (a, b) = Box::pin(support::both(
            publish(
                host,
                &source,
                &image,
                &notices,
                PublisherPolicy {
                    timeout: Duration::from_millis(250),
                    ..PublisherPolicy::default()
                },
                &configured,
            ),
            viewer.observe(
                target.launch(&image, Role::Present, 163),
                ObserverPolicy {
                    timeout: Duration::from_secs(1),
                    ..ObserverPolicy::default()
                },
                |catalog| {
                    assert_eq!(catalog.displays().len(), 1);
                    catalogs.fetch_add(1, Ordering::AcqRel);
                    Ok(None)
                },
                |_| {
                    notices.fetch_add(1, Ordering::AcqRel);
                    Ok(())
                },
            ),
        ))
        .await;
        assert!(a.is_err() && b.is_err());
        assert_eq!(configured.load(Ordering::Acquire), 0);
        assert!(catalogs.load(Ordering::Acquire) > 0);
    });
}

#[test]
#[ignore = "explicit native lane requires FR_NATIVE_TEST_WORKER and Xvfb"]
fn delayed_display_choice_keeps_discovered_source_and_both_observation_lifetimes_alive() {
    let image = image();
    run(|c, h| async move {
        let cleanup = Cx::current().unwrap();
        let mut source = Display::start();
        let mut target = Display::start();
        source.paint(0x0040_6070);
        let (host, viewer) = startup(&c, &h).await;
        let notices = AtomicUsize::new(0);
        let configured = AtomicUsize::new(0);
        let mut choose_at = None;
        let (a, b) = Box::pin(support::both(
            publish(
                host,
                &source,
                &image,
                &notices,
                PublisherPolicy::default(),
                &configured,
            ),
            viewer.observe(
                target.launch(&image, Role::Present, 171),
                ObserverPolicy::default(),
                |catalog| {
                    let current = now(&c).unwrap();
                    let until = *choose_at.get_or_insert(current + 3_250_000);
                    Ok((current >= until).then_some(catalog.displays()[0].handle))
                },
                |_| {
                    notices.fetch_add(1, Ordering::AcqRel);
                    Ok(())
                },
            ),
        ))
        .await;
        let mut host = a.unwrap();
        let mut viewer = b.unwrap();
        assert!(now(&c).unwrap() >= choose_at.unwrap());
        assert_eq!(configured.load(Ordering::Acquire), 1);
        assert!(host.control().check().is_ok());
        assert_eq!(host.display(), viewer.display());
        target.assert_pixel(0x0040_6070);
        host.close();
        viewer.close();
        host.reap_media(
            &cleanup,
            Deadline::after(&cleanup, Duration::from_secs(2)).unwrap(),
        )
        .await
        .unwrap();
        viewer
            .reap_media(
                &cleanup,
                Deadline::after(&cleanup, Duration::from_secs(2)).unwrap(),
            )
            .await
            .unwrap();
    });
}
#[test]
#[ignore = "explicit native lane requires FR_NATIVE_TEST_WORKER and Xvfb"]
fn selected_geometry_mismatch_refuses_before_native_capture_or_decoder_configuration() {
    let image = image();
    run(|c, h| async move {
        let source = Display::start();
        let target = Display::start();
        let (mut host, viewer) = startup(&c, &h).await;
        let notices = AtomicUsize::new(0);
        let attempted = AtomicUsize::new(0);
        let mut n = 31000;
        let (a, b) = Box::pin(support::both(
            async {
                while !host.is_complete() {
                    host.drive(Duration::from_millis(1)).await.unwrap();
                    if notices.load(Ordering::Acquire) != 0
                        && let Some(approval) = host.approval()
                    {
                        approval.decide(true).unwrap();
                    }
                }
                host.finish()
                    .unwrap()
                    .into_running()
                    .unwrap()
                    .publish_display(
                        source.launch(&image, Role::Capture, 172),
                        PublisherPolicy::default(),
                        |_| {
                            attempted.fetch_add(1, Ordering::AcqRel);
                            let mut cfg = configuration();
                            cfg.width += 2;
                            Ok(cfg)
                        },
                        || nonce(&mut n),
                    )
                    .await
            },
            viewer.observe(
                target.launch(&image, Role::Present, 173),
                ObserverPolicy::default(),
                |catalog| Ok(Some(catalog.displays()[0].handle)),
                |_| {
                    notices.fetch_add(1, Ordering::AcqRel);
                    Ok(())
                },
            ),
        ))
        .await;
        assert!(
            matches!(a, Err(PublisherError::Media(media::Error::InvalidFrame))),
            "{a:?}"
        );
        assert!(b.is_err());
        assert_eq!(attempted.load(Ordering::Acquire), 1);
    });
}

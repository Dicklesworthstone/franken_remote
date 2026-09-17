//! Native surface state is a fixture; the public bootstrap, children and UDP/TLS
//! are real. The independent fr-native target supplies actual X11 evidence.
use super::*;
use crate::local_sharing::{AttachError, State, Surface};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize};

#[derive(Default)]
struct Probe {
    state: AtomicU8,
    stopped_after_revoke: AtomicBool,
    cleanup: AtomicBool,
    dropped: AtomicUsize,
}
struct LocalSurface {
    original: crate::media::ObservationControl,
    probe: Arc<Probe>,
}
impl Surface for LocalSurface {
    fn original(&self) -> &crate::media::ObservationControl {
        &self.original
    }
    fn state(&self) -> State {
        match self.probe.state.load(Ordering::Acquire) {
            0 => State::Opening,
            1 => State::Ready,
            _ => State::Stopped,
        }
    }
    fn stop(&self) {
        self.probe
            .stopped_after_revoke
            .store(self.original.check().is_err(), Ordering::Release);
        self.original.revoke();
        self.probe.state.store(2, Ordering::Release);
    }
    fn finish(&mut self) -> bool {
        self.probe.cleanup.load(Ordering::Acquire)
    }
}
impl Drop for LocalSurface {
    fn drop(&mut self) {
        self.stop();
        self.probe.dropped.fetch_add(1, Ordering::AcqRel);
    }
}
fn surface(original: &crate::media::ObservationControl, probe: &Arc<Probe>) -> Box<dyn Surface> {
    Box::new(LocalSurface {
        original: original.clone(),
        probe: probe.clone(),
    })
}

#[test]
fn original_surface_is_one_use_and_foreign_equal_id_surface_is_returned_intact() {
    run3(|c, h, other| async move {
        let (mut host, _viewer) = pair_initialized(&c, &h, capabilities(), |_| {}).await;
        let (mut foreign, _foreign_viewer) =
            pair_initialized(&c, &other, capabilities(), |_| {}).await;
        let original = host.observation().unwrap();
        let foreign_original = foreign.observation().unwrap();
        let wrong = Arc::new(Probe::default());
        let rejected = host
            .attach_sharing_surface(surface(&foreign_original, &wrong))
            .unwrap_err();
        assert_eq!(rejected.reason, AttachError::WrongOwner);
        assert!(original.check().is_ok() && foreign_original.check().is_ok());
        assert_eq!(wrong.dropped.load(Ordering::Acquire), 0);
        let right = Arc::new(Probe::default());
        host.attach_sharing_surface(surface(&original, &right))
            .unwrap();
        let second = host.attach_sharing_surface(rejected.surface).unwrap_err();
        assert_eq!(second.reason, AttachError::AlreadyAttached);
        assert!(foreign_original.check().is_ok());
        host.close();
        assert!(original.check().is_err());
        assert!(right.stopped_after_revoke.load(Ordering::Acquire));
        assert!(foreign_original.check().is_ok());
        drop(second); // Explicitly drop the returned foreign owner, not a side effect of refusal.
        assert_eq!(wrong.dropped.load(Ordering::Acquire), 1);
    });
}

#[test]
fn pending_surface_blocks_native_spawn_until_the_original_bootstrap_deadline() {
    run3(|c, h, _cleanup| async move {
        let (mut host, _viewer) = pair_initialized(&c, &h, capabilities(), |_| {}).await;
        let original = host.observation().unwrap();
        let probe = Arc::new(Probe::default());
        host.attach_sharing_surface(surface(&original, &probe))
            .unwrap();
        // An attempted spawn would produce SpawnFailed, not the expected deadline.
        let launch = Launch::new(
            std::path::Path::new("/fr-native-fixture-must-never-start"),
            ":0",
            None,
            WorkerRole::Capture,
            155,
        )
        .unwrap();
        let mut id = 0u128;
        let result = host
            .publish_controlled_display(
                launch,
                PublisherPolicy {
                    timeout: Duration::from_millis(40),
                    ..PublisherPolicy::default()
                },
                |_| panic!("unmapped surface must not configure capture"),
                || {
                    id += 1;
                    Ok(id)
                },
            )
            .await;
        assert!(
            matches!(result, Err(crate::session_startup::PublisherError::Expired)),
            "{result:?}"
        );
        assert!(original.check().is_err());
        assert!(probe.stopped_after_revoke.load(Ordering::Acquire));
        assert_eq!(probe.dropped.load(Ordering::Acquire), 1);
    });
}

#[test]
fn dropping_unpolled_surface_bootstrap_revokes_before_discarding_native_owner() {
    run3(|c, h, _cleanup| async move {
        let (mut host, _viewer) = pair_initialized(&c, &h, capabilities(), |_| {}).await;
        let original = host.observation().unwrap();
        let probe = Arc::new(Probe::default());
        host.attach_sharing_surface(surface(&original, &probe))
            .unwrap();
        let attempt = host.publish_controlled_display(
            launch(WorkerRole::Capture),
            PublisherPolicy::default(),
            |_| panic!("never polled"),
            || panic!("never polled"),
        );
        drop(attempt);
        assert!(original.check().is_err());
        assert!(probe.stopped_after_revoke.load(Ordering::Acquire));
        assert_eq!(probe.dropped.load(Ordering::Acquire), 1);
    });
}

#[test]
fn native_publisher_retains_surface_and_reap_timeout_keeps_its_original_owner() {
    run3(|c, h, cleanup| async move {
        let (mut host, viewer) = pair_initialized(&c, &h, capabilities(), |_| {}).await;
        let original = host.observation().unwrap();
        let probe = Arc::new(Probe::default());
        host.attach_sharing_surface(surface(&original, &probe))
            .unwrap();
        let configured = AtomicBool::new(false);
        let start = now(&h).unwrap();
        let mut nonce = 400_000u128;
        let (pair, ()) = Box::pin(support::both(
            Box::pin(support::both(
                host.publish_controlled_display(
                    launch(WorkerRole::Capture),
                    PublisherPolicy::default(),
                    |display| {
                        assert_eq!(probe.state.load(Ordering::Acquire), 1);
                        configured.store(true, Ordering::Release);
                        config(display)
                    },
                    || {
                        nonce += 1;
                        Ok(nonce)
                    },
                ),
                viewer.observe_for_control(
                    launch(WorkerRole::Present),
                    ObserverPolicy::default(),
                    ClockPolicy::default(),
                    |cat| Ok(Some(cat.displays()[0].handle)),
                ),
            )),
            async {
                while now(&h).unwrap() < start + 100_000 {
                    assert!(!configured.load(Ordering::Acquire));
                    asupersync::time::sleep(h.now(), Duration::from_millis(1)).await;
                }
                probe.state.store(1, Ordering::Release);
            },
        ))
        .await;
        let (mut host, mut viewer) = (pair.0.unwrap(), pair.1.unwrap());
        assert!(configured.load(Ordering::Acquire));
        assert_eq!(probe.dropped.load(Ordering::Acquire), 0);
        assert!(original.check().is_ok());
        host.close();
        assert!(probe.stopped_after_revoke.load(Ordering::Acquire));
        // Media can finish but native UI remains pending. Its ownership must not
        // disappear merely because a cleanup budget expired or control stopped.
        assert_eq!(
            host.reap_media(
                &cleanup,
                Deadline::after(&cleanup, Duration::from_secs(1)).unwrap()
            )
            .await,
            Err(crate::worker::Error::ReapPending)
        );
        assert_eq!(probe.dropped.load(Ordering::Acquire), 0);
        probe.cleanup.store(true, Ordering::Release);
        host.reap_media(
            &cleanup,
            Deadline::after(&cleanup, Duration::from_secs(1)).unwrap(),
        )
        .await
        .unwrap();
        viewer
            .reap_media(
                &cleanup,
                Deadline::after(&cleanup, Duration::from_secs(1)).unwrap(),
            )
            .await
            .unwrap();
        drop(host);
        assert_eq!(probe.dropped.load(Ordering::Acquire), 1);
    });
}

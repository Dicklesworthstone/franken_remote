//! Real QUIC/startup/attachment and process supervision with an explicitly
//! synthetic codec. Native HEVC/pixel qualification is a separate executed lane.
use super::*;
pub(super) use crate::{
    media::{
        self,
        streaming::{Policy, Stream},
    },
    media_quic::NegotiatedMedia,
    session_startup::{
        ViewerSession,
        running::{RefreshTurn, pump_refresh, tests::pair_initialized},
        tests::support,
    },
    worker::{Deadline, Launch},
};
use fr_core::{ids::*, limits::ProtocolLimits};
pub(super) use fr_media::{
    delivery::{MediaBudget, ReceivePipeline, ReceivePolicy, SendPolicy},
    worker::{Backend, Configuration, Role},
};
pub(super) use fr_wire::{
    attachment::{self, MediaRole},
    negotiation::Capability,
};
use std::{
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

pub(super) fn configuration() -> Configuration {
    Configuration {
        width: 320,
        height: 240,
        fps: 30,
        backend: Backend::SoftwareExplicit,
        bitrate: 2_000_000,
        max_access_unit_bytes: ProtocolLimits::ABSOLUTE.max_encoded_access_unit_bytes(),
        generation: CodecConfigurationGeneration::INITIAL,
    }
}
fn fixture_path(mode: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "fr-stream-fixture-{}-{}-{mode}.py",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(
        &path,
        include_str!("capture_fixture.py").replace("@MODE@", mode),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    path
}
#[allow(clippy::unnecessary_wraps)]
pub(super) fn block(_: Route, _: &[u8]) -> Result<Disposition, ()> {
    Ok(Disposition::Blocked)
}
pub(super) fn nonce(n: &mut u128) -> Result<u128, ()> {
    *n = n.checked_add(1).ok_or(())?;
    Ok(*n)
}
pub(super) fn capabilities() -> Vec<Capability> {
    [
        fr_wire::decoder::CAPABILITY,
        attachment::CAPABILITY,
        attachment::DELIVERY_CAPABILITY,
    ]
    .into_iter()
    .map(|name| Capability {
        name: name.into(),
        version: 1,
        required: true,
    })
    .collect()
}
struct Media {
    stream: Stream,
    channels: NegotiatedMedia,
    receiver: ReceivePipeline,
}
// Called by the controlled-host regression too; no production admission bypass.
pub(in crate::session_startup::running) async fn source_for_controlled(
    host: &mut HostSession,
    viewer: &mut ViewerSession,
    c: &Cx,
    h: &Cx,
) -> Stream {
    Box::pin(media(host, viewer, c, h, "stall", SendPolicy::default()))
        .await
        .stream
}
async fn media(
    host: &mut HostSession,
    viewer: &mut ViewerSession,
    c: &Cx,
    h: &Cx,
    mode: &str,
    policy: SendPolicy,
) -> Media {
    use crate::session_startup::running::controlled::tests::attach;
    let (hc, vc) = attach(host, viewer, c, h, MediaRole::Configuration, 18).await;
    let (hr, vr) = attach(host, viewer, c, h, MediaRole::Recovery, 19).await;
    let (hv, vv) = attach(host, viewer, c, h, MediaRole::Video, 20).await;
    let selected = host.selection().clone();
    let channels = NegotiatedMedia::new(viewer.io().unwrap().0, &selected, &vc, &vr, &vv).unwrap();
    let host_channels =
        NegotiatedMedia::new(host.io().unwrap().0, &selected, &hc, &hr, &hv).unwrap();
    let control = host.observation().unwrap();
    let mut source = CaptureSource::start(
        &control,
        Launch::new(&fixture_path(mode), ":0", None, Role::Capture, 1).unwrap(),
        configuration(),
    )
    .await
    .unwrap();
    let initial = source.capture_if_changed(&control, true).await.unwrap();
    let mut sender = host_channels
        .sender(host.io().unwrap().0, control.clone(), policy)
        .unwrap();
    sender.enqueue_capture(initial).unwrap();
    let cfg = channels
        .receiver_config(viewer.io().unwrap().0, ReceivePolicy::default())
        .unwrap();
    let mut receiver =
        ReceivePipeline::new(cfg, MediaBudget::new(cfg.limits.protocol()).unwrap()).unwrap();
    receiver.decoder_configured(now(c).unwrap()).unwrap();
    let mut n = 1000;
    loop {
        sender
            .transmit(h, host.io().unwrap().0, Lane::Original)
            .unwrap();
        let (host_result, viewer_result) = Box::pin(support::both(
            host.drive(Duration::from_millis(1), || nonce(&mut n), block),
            viewer.drive(Duration::from_millis(1), block),
        ))
        .await;
        host_result.unwrap();
        viewer_result.unwrap();
        channels
            .receive_ready(
                c,
                viewer.io().unwrap().0,
                || true,
                |channel, bytes| {
                    receiver.receive(channel, bytes, now(c).unwrap()).unwrap();
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
        if let Some(frame) = receiver.take_decodable(now(c).unwrap()).unwrap() {
            assert_eq!(frame.descriptor().frame, 0);
            receiver
                .acknowledge_decode(&frame, true, now(c).unwrap())
                .unwrap();
            break;
        }
    }
    // Synthetic decoder fixture, deliberately not the public native startup gate.
    let stream = Stream {
        source,
        sender,
        control,
        policy: Policy::default(),
        capacity: usize::try_from(configuration().max_access_unit_bytes).unwrap()
            + fr_media::worker::UNIT_PREFIX_BYTES,
        statistics: Statistics::default(),
        served: false,
    };
    Media {
        stream,
        channels,
        receiver,
    }
}
async fn fixture(
    c: &Cx,
    h: &Cx,
    mode: &str,
    policy: SendPolicy,
) -> (
    StreamingHost,
    ViewerSession,
    NegotiatedMedia,
    ReceivePipeline,
) {
    let (mut host, mut viewer) = pair_initialized(c, h, capabilities(), |_| {}).await;
    let Media {
        stream,
        channels,
        receiver,
    } = Box::pin(media(&mut host, &mut viewer, c, h, mode, policy)).await;
    (
        host.into_streaming(stream).unwrap(),
        viewer,
        channels,
        receiver,
    )
}
async fn receive(
    viewer: &mut ViewerSession,
    channels: &NegotiatedMedia,
    receiver: &mut ReceivePipeline,
    c: &Cx,
) -> usize {
    viewer.drive(Duration::from_millis(1), block).await.unwrap();
    channels
        .receive_ready(
            c,
            viewer.io().unwrap().0,
            || true,
            |channel, bytes| {
                receiver.receive(channel, bytes, now(c).unwrap()).unwrap();
                Ok(Disposition::Consumed)
            },
        )
        .unwrap();
    let mut count = 0;
    while let Some(frame) = receiver.take_decodable(now(c).unwrap()).unwrap() {
        assert!(frame.descriptor().frame > 0);
        receiver
            .acknowledge_decode(&frame, true, now(c).unwrap())
            .unwrap();
        count += 1;
    }
    count
}
#[test]
fn continuous_frames_keep_the_original_session_alive_past_initial_authority() {
    run(|c, h| async move {
        let (mut host, mut viewer, channels, mut receiver) =
            Box::pin(fixture(&c, &h, "slow", SendPolicy::default())).await;
        let stop = host.stream.control.clone();
        let initial = stop.check().unwrap().as_micros() + 3_200_000;
        let mut n = 2000;
        let (result, frames) = Box::pin(support::both(
            host.serve(|| nonce(&mut n), || None, block),
            async {
                let mut frames = 0;
                while now(&c).unwrap() < initial {
                    frames += receive(&mut viewer, &channels, &mut receiver, &c).await;
                }
                stop.revoke();
                frames
            },
        ))
        .await;
        assert!(result.is_err());
        assert!(frames >= 20, "slow native work blocked the network");
        assert!(host.stream.statistics.encoded_updates >= u64::try_from(frames).unwrap());
        assert!(host.host.session().opened.transport.is_closed());
        host.reap_media(&c, Deadline::after(&c, Duration::from_secs(1)).unwrap())
            .await
            .unwrap();
    });
}
#[test]
fn static_source_sends_qualified_observations_without_dummy_encoded_frames() {
    run(|c, h| async move {
        let (mut host, mut viewer, channels, mut receiver) =
            Box::pin(fixture(&c, &h, "unchanged", SendPolicy::default())).await;
        let stop = host.stream.control.clone();
        let until = now(&c).unwrap() + 220_000;
        let mut n = 2000;
        let (result, frames) = Box::pin(support::both(
            host.serve(|| nonce(&mut n), || None, block),
            async {
                let mut frames = 0;
                while now(&c).unwrap() < until {
                    frames += receive(&mut viewer, &channels, &mut receiver, &c).await;
                }
                stop.revoke();
                frames
            },
        ))
        .await;
        assert!(result.is_err());
        assert_eq!(frames, 0);
        assert_eq!(host.statistics().encoded_updates, 0);
        assert!(host.statistics().unchanged_observations >= 3);
        host.reap_media(&c, Deadline::after(&c, Duration::from_secs(1)).unwrap())
            .await
            .unwrap();
    });
}
#[test]
fn cache_credit_stops_capture_before_another_compressed_picture_exists() {
    run(|c, h| async move {
        let (mut host, mut viewer, channels, mut receiver) = Box::pin(fixture(
            &c,
            &h,
            "healthy",
            SendPolicy {
                max_cached_pictures: 1,
                ..SendPolicy::default()
            },
        ))
        .await;
        let stop = host.stream.control.clone();
        let until = now(&c).unwrap() + 120_000;
        let mut n = 2000;
        let (result, frames) = Box::pin(support::both(
            host.serve(|| nonce(&mut n), || None, block),
            async {
                let mut frames = 0;
                while now(&c).unwrap() < until {
                    frames += receive(&mut viewer, &channels, &mut receiver, &c).await;
                }
                stop.revoke();
                frames
            },
        ))
        .await;
        assert!(result.is_err());
        assert_eq!(frames, 0);
        assert_eq!(host.statistics().encoded_updates, 0);
        assert_eq!(host.statistics().unchanged_observations, 0);
        host.reap_media(&c, Deadline::after(&c, Duration::from_secs(1)).unwrap())
            .await
            .unwrap();
    });
}
#[test]
fn stalled_capture_does_not_block_local_revoke_and_unpolled_serve_is_terminal() {
    run(|c, h| async move {
        let (mut host, mut viewer, channels, mut receiver) =
            Box::pin(fixture(&c, &h, "stall", SendPolicy::default())).await;
        let stop = host.stream.control.clone();
        let until = now(&c).unwrap() + 80_000;
        let mut n = 2000;
        let (result, ()) = Box::pin(support::both(
            host.serve(|| nonce(&mut n), || None, block),
            async {
                while now(&c).unwrap() < until {
                    assert_eq!(receive(&mut viewer, &channels, &mut receiver, &c).await, 0);
                }
                stop.revoke();
            },
        ))
        .await;
        assert!(result.is_err());
        assert!(host.host.session().opened.transport.is_closed());
        assert!(
            !host
                .reap_media(&c, Deadline::after(&c, Duration::from_secs(1)).unwrap())
                .await
                .unwrap()
                .success()
        );
        assert!(host.serve(|| Ok(99), || None, block).await.is_err());
    });
    run(|c, h| async move {
        let (mut host, _viewer, _channels, _receiver) =
            Box::pin(fixture(&c, &h, "healthy", SendPolicy::default())).await;
        let control = host.stream.control.clone();
        drop(host.serve(|| Ok(99), || None, block));
        assert!(control.check().is_err());
        assert!(host.host.session().opened.transport.is_closed());
        host.reap_media(&c, Deadline::after(&c, Duration::from_secs(1)).unwrap())
            .await
            .unwrap();
    });
}
#[test]
fn pending_admission_refresh_services_native_results_before_the_lookup_finishes() {
    run(|c, h| async move {
        let (mut host, mut viewer) = pair_initialized(&c, &h, capabilities(), |_| {}).await;
        let Media {
            mut stream,
            channels,
            mut receiver,
        } = Box::pin(media(
            &mut host,
            &mut viewer,
            &c,
            &h,
            "healthy",
            SendPolicy::default(),
        ))
        .await;
        let (credit, requests) = mpsc::channel(1);
        let (completed, results) = mpsc::channel(1);
        let done = Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        let mut n = 2000;
        let control = stream.control.clone();
        let mut other = block;
        let mut services = VideoServices {
            control: &control,
            sender: &mut stream.sender,
            statistics: &mut stream.statistics,
            policy: stream.policy,
            capacity: stream.capacity,
            credit,
            results,
            in_flight: false,
            next_capture: 0,
            repair_turn: false,
            other: &mut other,
        };
        let mut producer = pin!(produce(&mut stream.source, &control, requests, completed));
        let mut fresh_nonce = || nonce(&mut n);
        let mut refresh = pin!(pump_refresh(
            &mut host.renewal,
            &mut host.opened.transport,
            async {
                asupersync::time::sleep(h.now(), Duration::from_millis(180)).await;
                flag.store(true, Ordering::Release);
                Ok(())
            },
            RefreshTurn {
                cx: &h,
                control: &control,
                until: now(&h).unwrap() + 400_000,
                wait: Duration::from_millis(2)
            },
            &mut fresh_nonce,
            &mut services
        ));
        let mut remote = pin!(async {
            let mut frames = 0;
            while !done.load(Ordering::Acquire) {
                frames += receive(&mut viewer, &channels, &mut receiver, &c).await;
            }
            assert!(frames >= 2, "a ready native result waited behind LocalAPI");
        });
        let mut refreshed = false;
        let mut received = false;
        poll_fn(|task| {
            assert!(producer.as_mut().poll(task).is_pending());
            if !refreshed && let Poll::Ready(r) = refresh.as_mut().poll(task) {
                r.unwrap();
                refreshed = true;
            }
            if !received && remote.as_mut().poll(task).is_ready() {
                received = true;
            }
            if refreshed && received {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
        control.revoke();
    });
}
#[test]
fn stream_policy_and_same_id_foreign_observation_are_rejected() {
    for policy in [
        Policy {
            capture_interval: Duration::ZERO,
            ..Policy::default()
        },
        Policy {
            network_turn: Duration::from_secs(1),
            ..Policy::default()
        },
        Policy {
            records_per_turn: 0,
            ..Policy::default()
        },
    ] {
        assert!(policy.validate(30).is_err());
    }
    assert!(Policy::default().validate(0).is_err());
    run(|c, h| async move {
        let (mut host, mut viewer) = pair_initialized(&c, &h, capabilities(), |_| {}).await;
        let Media { mut stream, .. } = Box::pin(media(
            &mut host,
            &mut viewer,
            &c,
            &h,
            "healthy",
            SendPolicy::default(),
        ))
        .await;
        let mut authority = fr_core::authority::SessionAuthority::new(
            host.binding().remote_session,
            fr_core::authority::AuthorityPolicy::plan_defaults(),
        );
        authority.mark_capabilities_checked().unwrap();
        authority
            .authorize_observation(media::host_now(&h).unwrap())
            .unwrap();
        stream.control = ObservationControl::new(h.clone(), authority).unwrap();
        assert!(matches!(host.into_streaming(stream), Err(Error::Order)));
    });
}

pub(in crate::session_startup::running) fn run<F, Fut>(f: F)
where
    F: FnOnce(Cx, Cx) -> Fut,
    Fut: Future<Output = ()>,
{
    let runtime = asupersync::runtime::RuntimeBuilder::new()
        .worker_threads(2)
        .blocking_threads(2, 4)
        .enable_platform_reactor(true)
        .build()
        .unwrap();
    let c = runtime.request_cx_with_budget(asupersync::types::Budget::INFINITE);
    let h = runtime.request_cx_with_budget(asupersync::types::Budget::INFINITE);
    runtime.block_on(async {
        asupersync::time::timeout(c.now(), Duration::from_secs(12), f(c, h))
            .await
            .unwrap();
    });
}

#[test]
fn unpolled_native_future_is_dropped_only_after_observation_is_fenced() {
    struct Witness {
        control: ObservationControl,
        witnessed: Arc<AtomicBool>,
    }
    impl Future for Witness {
        type Output = ();
        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
            Poll::Pending
        }
    }
    impl Drop for Witness {
        fn drop(&mut self) {
            self.witnessed
                .store(self.control.check().is_err(), Ordering::SeqCst);
        }
    }
    run(|c, h| async move {
        let (mut host, _) = pair_initialized(&c, &h, capabilities(), |_| {}).await;
        let control = host.observation().unwrap();
        let witnessed = Arc::new(AtomicBool::new(false));
        let future = Guarded {
            fence: Fence {
                control: control.clone(),
                native: None,
            },
            inner: Box::pin(Witness {
                control: control.clone(),
                witnessed: witnessed.clone(),
            }),
        };
        assert!(control.check().is_ok());
        drop(future);
        assert!(
            witnessed.load(Ordering::SeqCst),
            "native future dropped before authority fencing"
        );
    });
}

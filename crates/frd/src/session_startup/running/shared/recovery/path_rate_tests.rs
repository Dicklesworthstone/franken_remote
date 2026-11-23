//! Real shared sessions, replacement attachments and source IPC. Only the
//! acknowledged path snapshots, encoded bytes and decoder replies are fixtures;
//! these tests do not measure a WAN or qualify a native encoder.
use super::*;
use fr_media::bitrate::Reason;
use fr_transport::quic::PathLoad;

fn enable(g: &mut Group, adaptive: bool) {
    if adaptive {
        g.publisher
            .enable_adaptive_capture(Duration::from_millis(50), Duration::from_millis(200))
            .unwrap();
        assert!(g.publisher.enable_adaptive_bitrate().unwrap());
    }
    assert!(g.publisher.enable_path_bitrate().unwrap());
}

fn sample(g: &mut Group, queued: &[bool]) {
    let at = now(&g.peers[0].h).unwrap();
    let baseline = PathLoad {
        latest_rtt_us: 1_000,
        smoothed_rtt_us: 1_000,
        rtt_variation_us: 500,
        bytes_in_flight: 64 * 1024,
        packets_acked: 0,
        busy_packets_acked: 0,
        busy_acked_at_us: None,
    };
    let loads: Vec<_> = queued
        .iter()
        .map(|queued| {
            let rtt = if *queued { 200_000 } else { 1_001 };
            (
                baseline,
                PathLoad {
                    latest_rtt_us: rtt,
                    smoothed_rtt_us: rtt,
                    rtt_variation_us: rtt / 2,
                    bytes_in_flight: if *queued { 64 * 1024 } else { 0 },
                    packets_acked: 1,
                    busy_packets_acked: 1,
                    busy_acked_at_us: Some(at),
                },
            )
        })
        .collect();
    g.publisher.sample_paths_for_test(&loads).unwrap();
}

async fn hold(g: &mut Group, queued: &[bool]) {
    let started = now(&g.peers[0].h).unwrap();
    loop {
        for p in &mut g.peers {
            if p.shared.as_ref().unwrap().statistics().recovery_requests == 0 {
                p.turn().await.unwrap();
            } else {
                network(p).await.unwrap();
            }
        }
        sample(g, queued);
        if queued.iter().any(|queued| !queued) {
            // The healthy sibling continues receiving real static-source
            // observations while the failed viewer waits for replacement.
            assert!(g.publisher.capture_next().await.unwrap().unchanged);
        }
        if now(&g.peers[0].h).unwrap() - started >= 220_000 {
            break;
        }
        asupersync::time::sleep(g.peers[0].h.now(), Duration::from_millis(10)).await;
    }
}

#[test]
fn a_subscribers_newer_clock_carries_through_a_lagged_publisher_turn() {
    let rt = support::runtime();
    let cleanup_cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut g = Box::pin(group_with_capabilities(&rt, 1, &[wire::CAPABILITY], false)).await;
        ready(&mut g).await;
        enable(&mut g, true);
        let turn = now(&g.peers[0].h).unwrap();
        // Actual connection service advances Members.last after the publisher
        // took its turn timestamp. No fixture clock or deadline is advanced.
        g.peers[0].turn().await.unwrap();
        assert!(
            g.publisher
                .capture_interval_for_test(turn)
                .unwrap()
                .is_some()
        );
        assert!(
            g.publisher
                .pacing()
                .unwrap()
                .report()
                .unwrap()
                .sample
                .now_us
                > turn
        );
        assert!(g.publisher.capture_next().await.unwrap().unchanged);
        assert_eq!(g.publisher.bitrate().unwrap().pending(), None);
        assert!(g.owner.check().is_ok());
        cleanup(&mut g, &cleanup_cx).await;
    });
}

#[test]
fn recovering_shared_owner_keeps_sampling_and_applies_its_path_cut_on_the_recovery_idr() {
    for adaptive in [false, true] {
        let rt = support::runtime();
        let cleanup_cx = rt.request_cx_with_budget(Budget::INFINITE);
        rt.block_on(async {
            let mut g = Box::pin(group_with_capabilities(&rt, 1, &[wire::CAPABILITY], false)).await;
            ready(&mut g).await;
            enable(&mut g, adaptive);
            let worker = g.publisher.worker_id();
            let original = g.peers[0].media.binding();
            let connection = g.peers[0]
                .shared
                .as_ref()
                .unwrap()
                .session
                .opened
                .transport
                .binding();
            admit(&mut g.peers[0]).await;
            let until = deadline(&mut g.peers[0]);

            // No decoder is eligible now. Its original path still measures a
            // persistent queue and must lower the eventual recovery IDR rate.
            hold(&mut g, &[true]).await;
            let cut = g.publisher.bitrate().unwrap().pending().unwrap();
            assert_eq!(cut.reason(), Reason::PathQueue);
            assert_eq!(cut.bitrate(), 1_200_000);
            assert_eq!(g.publisher.bitrate().unwrap().applied_bitrate(), 2_000_000);
            assert_eq!(deadline(&mut g.peers[0]), until);

            let failed = g.peers.remove(0);
            let recovered = Box::pin(replace(failed)).await;
            g.peers.push(recovered);
            assert_eq!(deadline(&mut g.peers[0]), until);
            sample(&mut g, &[false]);
            assert_eq!(g.publisher.bitrate().unwrap().pending(), Some(cut));
            let idr = g.publisher.capture_next().await.unwrap();
            assert_eq!((idr.delivered, idr.refused), (1, 0));
            assert!(!idr.unchanged);
            assert_eq!(
                g.publisher.bitrate().unwrap().applied_bitrate(),
                cut.bitrate()
            );
            assert_eq!(g.publisher.bitrate().unwrap().pending(), None);
            ready(&mut g).await;
            assert!(g.peers[0].frames.contains(&idr.frame));
            assert_eq!(
                g.peers[0].media.binding().recovery,
                original.recovery.next().unwrap()
            );
            assert!(
                g.peers[0]
                    .shared
                    .as_ref()
                    .unwrap()
                    .session
                    .opened
                    .transport
                    .is_bound_to(&connection)
            );
            assert_eq!(g.publisher.worker_id(), worker);
            assert!(g.owner.check().is_ok());
            assert!(!g.peers[0].control.view_ready().unwrap());
            cleanup(&mut g, &cleanup_cx).await;
        });
    }
}

#[test]
fn a_static_native_deferral_survives_shared_decoder_replacement() {
    for adaptive in [false, true] {
        let rt = support::runtime();
        let cleanup_cx = rt.request_cx_with_budget(Budget::INFINITE);
        rt.block_on(async {
            let mut g = Box::pin(group_with_capabilities(&rt, 1, &[wire::CAPABILITY], false)).await;
            ready(&mut g).await;
            enable(&mut g, adaptive);
            assert!(g.publisher.capture_next().await.unwrap().unchanged);
            hold(&mut g, &[true]).await;
            let cut = g.publisher.bitrate().unwrap().pending().unwrap();
            sample(&mut g, &[false]);
            let unchanged = g.publisher.capture_next().await.unwrap();
            assert!(unchanged.unchanged);
            assert_eq!(g.publisher.bitrate().unwrap().applied_bitrate(), 2_000_000);
            assert_eq!(g.publisher.bitrate().unwrap().pending(), Some(cut));

            admit(&mut g.peers[0]).await;
            let until = deadline(&mut g.peers[0]);
            let failed = g.peers.remove(0);
            let recovered = Box::pin(replace(failed)).await;
            g.peers.push(recovered);
            assert_eq!(deadline(&mut g.peers[0]), until);
            sample(&mut g, &[false]);
            let idr = g.publisher.capture_next().await.unwrap();
            assert!(!idr.unchanged);
            assert_eq!(
                g.publisher.bitrate().unwrap().applied_bitrate(),
                cut.bitrate()
            );
            assert_eq!(g.publisher.bitrate().unwrap().pending(), None);
            ready(&mut g).await;
            assert!(g.peers[0].frames.contains(&idr.frame));
            cleanup(&mut g, &cleanup_cx).await;
        });
    }
}

#[test]
fn a_healthy_sibling_vetoes_the_recovering_paths_shared_rate_cut() {
    for adaptive in [false, true] {
        let rt = support::runtime();
        let cleanup_cx = rt.request_cx_with_budget(Budget::INFINITE);
        rt.block_on(async {
            let mut g = Box::pin(group_with_capabilities(&rt, 2, &[wire::CAPABILITY], false)).await;
            ready(&mut g).await;
            enable(&mut g, adaptive);
            let healthy_view = g.peers[1].media.binding();
            let worker = g.publisher.worker_id();
            admit(&mut g.peers[0]).await;
            let until = deadline(&mut g.peers[0]);

            hold(&mut g, &[true, false]).await;
            assert_eq!(g.publisher.bitrate().unwrap().pending(), None);
            hold(&mut g, &[true, true]).await;
            assert_eq!(
                g.publisher.bitrate().unwrap().pending().unwrap().reason(),
                Reason::PathQueue
            );
            // The same original healthy sibling drains: withdraw the whole
            // source cut even while the other decoder is still recovering.
            sample(&mut g, &[true, false]);
            assert_eq!(g.publisher.bitrate().unwrap().pending(), None);
            let capture = g.publisher.capture_next().await.unwrap();
            assert!(capture.unchanged);
            assert_eq!(g.publisher.bitrate().unwrap().applied_bitrate(), 2_000_000);
            assert_eq!(deadline(&mut g.peers[0]), until);
            assert_eq!(g.peers[1].media.binding(), healthy_view);
            assert_eq!(
                g.peers[1]
                    .shared
                    .as_ref()
                    .unwrap()
                    .statistics()
                    .recovery_requests,
                0
            );
            assert!(g.peers[1].control.check().is_ok());
            assert_eq!(g.publisher.worker_id(), worker);
            cleanup(&mut g, &cleanup_cx).await;
        });
    }
}

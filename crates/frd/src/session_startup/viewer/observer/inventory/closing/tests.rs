//! Production inspection/host drivers, real local TLS/UDP, fixture identity.
use super::*;
use crate::{
    media::renewal,
    session_startup::{
        Error as StartupError,
        running::tests::{pair_before_finish, run},
        test_network as network,
        viewer::observer::Policy,
    },
};
use fr_core::limits::ProtocolLimits;
use fr_wire::{
    display::{self, Catalog},
    negotiation::Capability,
};
use std::time::Instant;

#[test]
fn completed_display_inspection_requests_host_close_without_selecting_a_display() {
    run(|c, h| async move {
        let capabilities = vec![Capability {
            name: display::CAPABILITY.into(),
            version: display::VERSION,
            required: true,
        }];
        let (host, viewer) = pair_before_finish(&c, &h, capabilities).await;
        let mut host = host.finish().unwrap().into_running().unwrap();
        let observation = host.observation().unwrap();
        // An empty catalog is still a valid completed metadata lookup. No
        // invented display or native worker is needed to exercise this path.
        let catalog = Catalog::new(7, &[], &ProtocolLimits::ABSOLUTE).unwrap();
        let mut selection = host
            .select_display(catalog, Duration::from_secs(2))
            .unwrap();
        let host_run = async {
            let mut nonce = 0_u128;
            for _ in 0..500 {
                selection.transmit(host.io().unwrap().0).unwrap();
                let result = host
                    .drive(
                        Duration::from_millis(2),
                        || {
                            nonce += 1;
                            Ok(nonce)
                        },
                        |_, _| panic!("inspection must not select a display or request input"),
                    )
                    .await;
                if let Err(error) = result {
                    return error;
                }
            }
            panic!("inspection left host waiting for observation expiry");
        };
        let (host_result, catalog_result) = Box::pin(network::both(
            host_run,
            viewer.inspect_displays(Policy::default(), |_| Ok(())),
        ))
        .await;
        assert_eq!(
            host_result,
            StartupError::Renewal(renewal::Error::PeerClosed)
        );
        assert_eq!(catalog_result.unwrap().revision(), 7);
        assert!(observation.check().is_err());
    });
}

#[test]
fn inspection_close_does_not_wait_for_a_silent_peer_or_extend_the_old_budget() {
    run(|c, h| async move {
        let (host, viewer) = pair_before_finish(&c, &h, vec![]).await;
        let _host = host.finish().unwrap().into_running().unwrap();
        let viewer = viewer.finish().unwrap();
        let budget = Budget::new(
            c.clone(),
            Policy {
                timeout: Duration::from_millis(20),
                ..Policy::default()
            },
        )
        .unwrap();
        let start = Instant::now();
        finish(viewer, &budget).await;
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(c.is_cancel_requested());
    });
}

#[test]
fn prepare_fences_service_and_dropping_an_unpolled_close_cancels_the_session() {
    run(|c, h| async move {
        let (host, viewer) = pair_before_finish(&c, &h, vec![]).await;
        let _host = host.finish().unwrap().into_running().unwrap();
        let mut viewer = viewer.finish().unwrap();
        let budget = Budget::new(c.clone(), Policy::default()).unwrap();
        let (bytes, started, until) = prepare(&mut viewer, &budget).unwrap();
        assert!(viewer.is_closed());
        assert!(until <= budget.until && until - started <= DRAIN_US);
        let request = closure::decode_request(
            &bytes,
            Binding {
                channel: viewer.opened.binding.id,
                session: viewer.opened.binding.remote_session,
            },
            &ProtocolLimits::ABSOLUTE,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
        )
        .unwrap();
        assert_eq!(request.reason, Reason::InspectionComplete);
        // The terminal future owns this same session, even before first poll.
        let pending = finish(viewer, &budget);
        drop(pending);
        assert!(c.is_cancel_requested());
    });
}

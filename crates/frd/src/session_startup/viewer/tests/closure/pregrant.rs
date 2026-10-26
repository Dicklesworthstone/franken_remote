//! Negotiated host/viewer TLS and production closure dispatch. Source failure
//! is signalled by the original owner; no native capture or input is simulated
//! as hardware evidence, and no control request or lease is constructed.
use super::*;
use crate::session_startup::closure_reporting::Reporting;

#[test]
fn control_capable_viewer_without_a_request_receives_the_original_hosts_failure() {
    with_cleanup(|c, h, cleanup| async move {
        let (reporting, configuration) = Reporting::new(&cleanup);
        let (mut host, mut viewer) = Box::pin(opened_reporting(
            &c,
            &h,
            Role::RequestControl,
            Some(configuration),
        ))
        .await;
        let observation = host.observation().unwrap();
        assert_eq!(host.selection().role, Role::RequestControl);
        assert!(!observation.view_ready().unwrap());
        assert!(observation.check_control().is_err(), "no implicit lease");
        host.mark_host_failure();
        host.close();
        assert!(observation.check().is_err());
        let expected = expected(ClosedReason::HostFailure);
        let (sent, received) = Box::pin(network::both(
            reporting.finish(ClosedReason::HostStopping),
            receive_end(&mut viewer),
        ))
        .await;
        assert_eq!(received, Error::RemoteClosed(expected));
        assert_eq!(viewer.closed_report(), Some(expected));
        let sent = sent.expect("the original pre-grant closure consumer");
        assert_eq!(sent.report, expected);
        assert!(matches!(sent.delivery, Ok(()) | Err(quic::Error::Expired)));
        assert!(h.is_cancel_requested());
        assert!(!cleanup.is_cancel_requested());
    });
}

#[test]
fn cancelled_or_unpolled_pregrant_service_still_reports_without_minting_a_lease() {
    for cancelled in [false, true] {
        with_cleanup(|c, h, cleanup| async move {
            let (reporting, configuration) = Reporting::new(&cleanup);
            let (mut host, mut viewer) = Box::pin(opened_reporting(
                &c,
                &h,
                Role::RequestControl,
                Some(configuration),
            ))
            .await;
            let observation = host.observation().unwrap();
            assert!(observation.check_control().is_err());
            if cancelled {
                h.cancel_fast(asupersync::types::CancelKind::User);
                assert!(host.io().is_err());
            } else {
                drop(host.drive(
                    Duration::from_millis(1),
                    || panic!("unpolled renewal"),
                    |_, _| panic!("unpolled application dispatch"),
                ));
            }
            assert!(observation.check().is_err());
            let expected = expected(ClosedReason::HostStopping);
            let (sent, received) = Box::pin(network::both(
                reporting.finish(expected.reason),
                receive_end(&mut viewer),
            ))
            .await;
            assert_eq!(received, Error::RemoteClosed(expected));
            assert_eq!(sent.unwrap().report, expected);
            assert_eq!(viewer.closed_report(), Some(expected));
            assert!(!cleanup.is_cancel_requested());
        });
    }
}

#[test]
fn closure_reporting_before_session_opened_is_unarmed_and_never_sends_a_session_report() {
    with_cleanup(|c, h, cleanup| async move {
        let (reporting, configuration) = Reporting::new(&cleanup);
        let (mut host, mut viewer) = pair(&c, &h, false).await;
        host.retain_observation_reporting(configuration).unwrap();
        host.close();
        assert_eq!(reporting.finish(ClosedReason::HostFailure).await, None);
        assert!(!host.is_complete());
        assert!(!viewer.is_complete());
        viewer.close();
        assert!(!cleanup.is_cancel_requested());
    });
}

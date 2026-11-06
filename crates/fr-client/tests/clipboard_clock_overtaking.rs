//! Original controller projection under independently ordered native/network
//! consumers. Recording sinks prove lane behavior, not a live desktop session.
#[allow(dead_code)]
#[path = "clipboard_controller/mod.rs"]
mod support;

use fr_client::clipboard::Error;
use fr_core::clipboard::Publication;
use fr_wire::clipboard::session::Pump;
use support::*;

#[test]
fn overtaken_clipboard_samples_count_owner_progress_only_once() {
    for maintenance in [false, true] {
        let (mut input, lane) = attached();
        let transport = lane.transport();
        input.tick(client(100)).unwrap();
        for elapsed in 0..=100 {
            let actual = if maintenance {
                transport.sample_maintenance(client(elapsed))
            } else {
                transport.sample(client(elapsed))
            };
            assert_eq!(actual, Ok(host_at(100)), "sample {elapsed}");
        }
        assert_eq!(transport.sample(client(101)), Ok(host_at(101)));
        // The consumer's own ordered samples still cannot move backwards,
        // even when every sample precedes a newer input-owner observation.
        input.tick(client(200)).unwrap();
        assert_eq!(transport.sample(client(102)), Ok(host_at(200)));
        assert_eq!(transport.sample(client(101)), Err(Error::Clock));
        assert_eq!(transport.sample(client(201)), Err(Error::Stopped));
    }
}

#[test]
fn overtaken_file_permission_keeps_its_clock_and_regression_fence() {
    let (mut input, clipboard) = attached();
    let grant = grant();
    let authority = input
        .file_receive_authority(grant.request.parent, 78, grant.lease, client(0))
        .unwrap();
    input.tick(client(100)).unwrap();
    for elapsed in 0..=100 {
        let at = authority.sample(client(elapsed)).unwrap();
        assert_eq!(at, host_at(100));
        assert_eq!(
            authority.deadline(at).unwrap().as_micros(),
            grant.lease_until_us
        );
    }
    input.tick(client(200)).unwrap();
    assert_eq!(authority.sample(client(101)).unwrap(), host_at(200));
    assert!(authority.sample(client(100)).is_err());
    assert!(authority.sample(client(201)).is_err());
    // A real file-clock fault still retires only that auxiliary authority.
    input.tick(client(201)).unwrap();
    assert_eq!(clipboard.transport().sample(client(201)), Ok(host_at(201)));
    assert_eq!(input.stopped(), None);
}

#[test]
fn overtaken_transport_does_not_retire_a_live_clipboard_transfer() {
    let (mut input, mut viewer) = attached();
    let transport = viewer.transport();
    let host_input = host_owner();
    let mut host = host_channel(&host_input);
    let mut platform = Platform::default();
    let mut gate = Gate::default();
    viewer.offer(1, "original copy", None, client(0)).unwrap();
    assert_eq!(
        viewer.pump(&mut scratch(), &mut gate, || client(0)),
        Ok(Pump::RecordAccepted)
    );
    host.receive(&gate.last, &mut platform, || host_at(0))
        .unwrap();

    ready(&mut input, 1, 1_000_000);
    ready(&mut input, 2, 2_000_000);
    // Both callbacks were sampled before the owner's latest progress. The
    // second must not count that already observed duration a second time and
    // falsely exhaust the original three-second authorization.
    assert_eq!(transport.sample(client(0)), Ok(host_at(2_000_000)));
    assert_eq!(transport.sample(client(999_900)), Ok(host_at(2_000_000)));
    let mut publication = None;
    for _ in 0..4 {
        let result = viewer
            .pump(&mut scratch(), &mut gate, || client(2_000_000))
            .unwrap();
        publication = host
            .receive(&gate.last, &mut platform, || host_at(2_000_000))
            .unwrap();
        if matches!(result, Pump::ItemAccepted(_)) {
            break;
        }
    }
    assert_eq!(publication.unwrap().publication, Publication::SubmittedToOs);
    assert_eq!(platform.text, ["original copy"]);
    assert!(!viewer.is_closed());

    // Correcting over-counting does not renew the grant or the original item.
    ready(&mut input, 3, 2_999_899);
    assert_eq!(transport.sample(client(2_999_899)), Ok(host_at(2_999_899)));
    assert_eq!(transport.sample(client(2_999_900)), Err(Error::Expired));
    assert!(
        viewer
            .offer(2, "too late", None, client(2_999_900))
            .is_err()
    );
    assert_eq!(viewer.retained_bytes(), 0);
}

use super::*;
use fr_wire::clipboard::session::{Admission, RecordSink, TransportFailure, egress::Egress};
#[derive(Default)]
struct Handoff(Option<Egress>);
impl RecordSink for Handoff {
    fn try_send(&mut self, _: &[u8]) -> Result<Admission, TransportFailure> {
        panic!("permit required")
    }
    fn try_send_checked(
        &mut self,
        _: &[u8],
        permit: Egress,
    ) -> Result<Admission, TransportFailure> {
        self.0 = Some(permit);
        Ok(Admission::Accepted)
    }
}
#[test]
fn concurrent_network_and_input_checks_do_not_replace_the_native_sample() {
    let (mut input, mut lane) = attached();
    let network = lane.transport();
    lane.offer(1, "native", None, client(0)).unwrap();
    let mut sink = Handoff::default();
    lane.pump(&mut scratch(), &mut sink, || client(0)).unwrap();
    let native_at = network.sample(client(0)).unwrap();
    network.sample(client(20)).unwrap();
    input.tick(client(10)).unwrap();
    // Independent policy consumers may overtake the native sample. It remains
    // exact for its own cursor; original lifecycle/readiness is checked anew.
    sink.0.as_ref().unwrap().check(native_at).unwrap();
    lane.pump(&mut scratch(), &mut sink, || client(5)).unwrap();
    network.sample(client(21)).unwrap();
    assert!(!lane.is_closed());
}
#[test]
fn regression_of_one_network_cursor_still_stops_original_authority() {
    let (_input, mut lane) = attached();
    let network = lane.transport();
    network.sample(client(20)).unwrap();
    assert_eq!(network.sample(client(19)), Err(Error::Clock));
    assert!(lane.offer(1, "cannot revive", None, client(21)).is_err());
}
#[test]
fn original_owner_drop_stops_every_independent_network_cursor() {
    let (input, lane) = attached();
    let first = lane.transport();
    let second = lane.transport();
    first.sample(client(10)).unwrap();
    second.sample(client(1)).unwrap();
    drop(input);
    assert!(first.sample(client(11)).is_err());
    assert!(second.sample(client(2)).is_err());
}
#[test]
fn wire_close_stops_controller_transport_without_stopping_unrelated_input() {
    let (mut input, mut lane) = attached();
    let network = lane.transport();
    network.transport().close();
    assert!(network.sample(client(1)).is_err());
    assert!(lane.offer(1, "not queued", None, client(1)).is_err());
    input.tick(client(2)).unwrap();
}

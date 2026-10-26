//! Real public bootstrap, `StreamingViewer` report admission, native mailbox,
//! QUIC feedback and post-renewal input. Source, visibility and native effects
//! retain the parent fixture's explicit scope; this is not hardware evidence.
use super::*;

pub(super) fn capabilities(profile: Profile) -> Vec<WireCapability> {
    if profile == Profile::Legacy {
        return super::capabilities();
    }
    let host = host_offer(true);
    let mut client = fr_client::native::control_offer();
    if profile == Profile::Authority {
        client
            .capabilities
            .retain(|cap| cap.name != fr_wire::input_ticket::READINESS_CAPABILITY);
    }
    let intersection = host.intersect(&client).unwrap();
    let selected = intersection.select().unwrap();
    assert_eq!(
        fr_wire::input_ticket::selected_profile(&selected),
        Ok(profile)
    );
    client.check_host(&intersection).unwrap();
    intersection.capabilities
}

#[test]
fn readiness_feedback_keeps_the_original_stream_and_native_owner_live_past_grant_renewal() {
    // Four actual native receipts: press/release under the initial grant,
    // then another press/release after 3.1 s. A merely queued grant, unbound
    // report cursor, wrong feedback codec or old ticket cannot satisfy this.
    run3(|c, h, cleanup| exercise(c, h, cleanup, true, true, Profile::Readiness));
}

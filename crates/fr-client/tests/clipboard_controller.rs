//! The viewer is constructed by the real grant decoder, never a synthetic host
//! `InputSession`. Native/transport sinks here are explicit recording fixtures.
#[path = "clipboard_controller/mod.rs"]
mod support;
use fr_client::{
    clipboard::Error,
    input::{Action, InputClient, StopReason},
};
use fr_core::{
    clipboard::{Endpoint, Publication, Stamp},
    ids::InputTicketId,
    input::{KeyTransition, PhysicalKey},
};
use fr_wire::clipboard::{
    self, Body, Message, Role,
    session::{Offer, Pump},
};
use support::*;

#[test]
fn wire_ids_alone_never_manufacture_viewer_clipboard_authority() {
    let g = grant();
    let mut input = InputClient::new(
        g.credentials(),
        9,
        g.request.target.bounds,
        g.request.target.capabilities,
        L,
        policy(),
        client(0),
    )
    .unwrap();
    ready(&mut input, 0, 0);
    assert_eq!(
        input.attach_clipboard(77, true, client(0)).unwrap_err(),
        Error::NotGranted
    );
}

#[test]
fn accepted_grant_requires_separate_permission_mapping_presentation_and_lane() {
    let mut input = accepted();
    assert_eq!(
        input.attach_clipboard(77, false, client(0)).unwrap_err(),
        Error::Permission
    );
    assert_eq!(
        input.attach_clipboard(77, true, client(0)).unwrap_err(),
        Error::NotReady
    );
    input
        .confirm_mapping(
            grant().credentials().session,
            grant().credentials().view,
            client(0),
        )
        .unwrap();
    assert_eq!(
        input.attach_clipboard(77, true, client(0)).unwrap_err(),
        Error::NotReady
    );
    ready(&mut input, 0, 0);
    for wrong in [0, 7, 8, 9] {
        assert_eq!(
            input.attach_clipboard(wrong, true, client(0)).unwrap_err(),
            Error::WrongChannel
        );
    }
    let lane = input.attach_clipboard(77, true, client(0)).unwrap();
    drop(lane);
    assert_eq!(
        input.attach_clipboard(78, true, client(0)).unwrap_err(),
        Error::AlreadyAttached
    );
}

#[test]
fn all_terminal_input_states_fence_the_original_clipboard_even_without_input_ticks() {
    for reason in [
        StopReason::FocusLost,
        StopReason::Suspended,
        StopReason::Disconnected,
        StopReason::ViewChanged,
        StopReason::ViewStale,
        StopReason::ClockRegression,
        StopReason::CounterExhausted,
        StopReason::ReceiptTimeout,
        StopReason::ActionFailed,
        StopReason::InvalidReceipt,
        StopReason::InvalidTicket,
        StopReason::InvalidControl,
    ] {
        let (mut input, mut lane) = attached();
        lane.offer(1, "secret", None, client(0)).unwrap();
        input.stop(reason);
        let mut gate = Gate::default();
        assert!(lane.pump(&mut scratch(), &mut gate, || client(1)).is_err());
        assert!(lane.is_closed());
        assert_eq!(lane.retained_bytes(), 0);
        assert_eq!(gate.calls, 0);
    }
}

#[test]
fn dropping_original_owner_is_terminal_even_when_replacement_reuses_all_ids() {
    let (input, mut old) = attached();
    old.offer(1, "old", None, client(0)).unwrap();
    drop(input);
    let (_new_input, mut new_lane) = attached();
    new_lane.offer(2, "new", None, client(0)).unwrap();
    assert_eq!(old.offer(3, "stale", None, client(0)), Err(Error::Stopped));
    assert_eq!(old.retained_bytes(), 0);
    assert!(matches!(
        new_lane.pump(&mut scratch(), &mut Gate::default(), || client(0)),
        Ok(Pump::RecordAccepted)
    ));
}

#[test]
fn controller_and_host_exchange_empty_unicode_and_one_mib_in_both_directions() {
    for text in [
        String::new(),
        "private 🦀 café\n\0tail".to_owned(),
        "🦀".repeat(262_144),
    ] {
        for from_viewer in [true, false] {
            let (_input, mut viewer) = attached();
            let host_input = host_owner();
            let mut host = host_channel(&host_input);
            let mut platform = Platform::default();
            let mut gate = Gate::default();
            let offered = if from_viewer {
                viewer.offer(123, &text, None, client(0)).unwrap()
            } else {
                host.offer(123, &text, None, host_at(0)).unwrap()
            };
            let Offer::Queued(stamp) = offered else {
                panic!("expected new item")
            };
            let mut committed = false;
            for _ in 0..1030 {
                let mut scratch = scratch();
                let result = if from_viewer {
                    viewer.pump(&mut scratch, &mut gate, || client(0)).unwrap()
                } else {
                    host.pump(&mut scratch, &mut gate, || host_at(0)).unwrap()
                };
                assert!(scratch.iter().all(|b| *b == 0));
                let receipt = if from_viewer {
                    host.receive(&gate.last, &mut platform, || host_at(0))
                        .unwrap()
                } else {
                    viewer
                        .receive(&gate.last, &mut platform, || client(0))
                        .unwrap()
                };
                if matches!(result, Pump::ItemAccepted(_)) {
                    assert_eq!(receipt.unwrap().publication, Publication::SubmittedToOs);
                    committed = true;
                    break;
                }
            }
            assert!(committed);
            assert_eq!(platform.text.as_slice(), std::slice::from_ref(&text));
            assert_eq!(viewer.retained_bytes(), 0);
            assert_eq!(host.retained_bytes(), 0);
            let echo = if from_viewer {
                host.offer(124, &text, Some(stamp), host_at(0)).unwrap()
            } else {
                viewer.offer(124, &text, Some(stamp), client(0)).unwrap()
            };
            assert_eq!(echo, Offer::EchoSuppressed);
        }
    }
}

#[test]
fn clipboard_lease_is_not_shortened_to_input_ticket_but_expiry_is_terminal() {
    let (mut input, mut lane) = attached();
    ready(&mut input, 1, 1_000_000);
    assert!(input.ticket_deadline().unwrap() <= client(1_000_000));
    lane.offer(1, "clipboard remains authorized", None, client(1_000_000))
        .unwrap();
    ready(&mut input, 2, 2_000_000);
    assert!(lane.offer(2, "still live", None, client(2_000_000)).is_ok());
    let mut gate = Gate::default();
    assert_eq!(
        lane.pump(&mut scratch(), &mut gate, || client(3_000_000)),
        Err(Error::Expired)
    );
    assert!(lane.is_closed());
    assert_eq!(gate.calls, 0);
    assert!(
        input
            .accept_ticket(
                &ticket(2, H + 3_000_000, H + 4_000_000),
                correlation(3_000_000),
                client(3_000_000)
            )
            .is_err()
    );
    assert!(input.stopped().is_some());
}

#[test]
fn genuine_new_host_ticket_extends_only_the_still_live_original_owner() {
    let (mut input, mut lane) = attached();
    ready(&mut input, 1, 1_000_000);
    ready(&mut input, 2, 2_000_000);
    input
        .accept_ticket(
            &ticket(1, H + 2_000_000, H + 3_500_000),
            correlation(2_000_000),
            client(2_000_000),
        )
        .unwrap();
    ready(&mut input, 3, 3_000_000);
    assert!(lane.offer(1, "renewed", None, client(3_000_000)).is_ok());
    assert_eq!(
        lane.offer(2, "too late", None, client(3_500_000)),
        Err(Error::Expired)
    );
}

#[test]
fn view_staleness_and_clock_regression_are_checked_by_clipboard_during_silence() {
    for elapsed in [1_500_000, 0] {
        let (_input, mut lane) = attached();
        lane.offer(1, "pending", None, client(1)).unwrap();
        let mut gate = Gate::default();
        assert!(
            lane.pump(&mut scratch(), &mut gate, || client(elapsed))
                .is_err()
        );
        assert!(lane.is_closed());
        assert_eq!(gate.calls, 0);
        assert_eq!(lane.retained_bytes(), 0);
    }
}

#[test]
fn missed_input_receipt_deadline_also_fences_clipboard_without_owner_tick() {
    let (mut input, mut lane) = attached();
    let encoded = input
        .action(
            Action::Key {
                key: PhysicalKey::new(4).unwrap(),
                transition: KeyTransition::Press,
            },
            &mut [0; 256],
            client(0),
        )
        .unwrap();
    assert_eq!(encoded.sequence, 0);
    lane.offer(1, "pending", None, client(0)).unwrap();
    assert_eq!(
        lane.offer(2, "too late", None, client(500_000)),
        Err(Error::Expired)
    );
    assert!(input.tick(client(500_000)).is_err());
}

#[test]
fn native_preparation_is_followed_by_final_original_owner_and_clock_check() {
    for expire in [true, false] {
        let (mut input, mut lane) = attached();
        let records = incoming("incoming");
        let mut platform = Platform::default();
        for record in &records[..2] {
            lane.receive(record, &mut platform, || client(0)).unwrap();
        }
        let mut calls = 0;
        let result = lane.receive(&records[2], &mut platform, || {
            calls += 1;
            if calls > 1 && !expire {
                input.stop(StopReason::FocusLost);
            }
            client(if calls > 1 && expire { 1_500_000 } else { 0 })
        });
        assert!(result.is_err());
        assert_eq!(platform.prepared, 1);
        assert_eq!(platform.cancelled, 1);
        assert_eq!(platform.text, [] as [String; 0]);
        assert_eq!(lane.retained_bytes(), 0);
    }
}

#[test]
fn uncertain_native_publication_receipt_survives_and_is_never_replayed() {
    let (_input, mut lane) = attached();
    let records = incoming("uncertain");
    let mut platform = Platform {
        outcome: Some(Publication::UnknownEffect),
        ..Platform::default()
    };
    for record in &records[..2] {
        lane.receive(record, &mut platform, || client(0)).unwrap();
    }
    let receipt = lane
        .receive(&records[2], &mut platform, || client(0))
        .unwrap()
        .unwrap();
    assert_eq!(receipt.publication, Publication::UnknownEffect);
    assert_eq!(
        lane.receive(&records[2], &mut platform, || client(0))
            .unwrap()
            .unwrap(),
        receipt
    );
    assert_eq!(platform.text, ["uncertain"]);
}

#[test]
fn both_switches_retire_payloads_without_revoking_otherwise_live_input() {
    for local in [true, false] {
        let (mut input, mut lane) = attached();
        lane.offer(1, "stale", None, client(0)).unwrap();
        let switch = if local {
            lane.local_switch()
        } else {
            lane.peer_switch()
        };
        switch.set_enabled(false);
        switch.set_enabled(true);
        assert_eq!(
            lane.pump(&mut scratch(), &mut Gate::default(), || client(0)),
            Ok(Pump::Suspended)
        );
        assert_eq!(lane.retained_bytes(), 0);
        input.tick(client(0)).unwrap();
        assert!(matches!(
            lane.offer(2, "genuine new copy", None, client(0)),
            Ok(Offer::Queued(_))
        ));
    }
}

#[test]
fn malformed_or_foreign_lane_records_close_only_the_clipboard() {
    for malformed in [true, false] {
        let (mut input, mut lane) = attached();
        let mut record = vec![0; 128];
        let n = clipboard::encode(
            Message {
                stamp: Stamp {
                    id: 1,
                    sequence: 1,
                    source: Endpoint::Host,
                },
                body: Body::Begin {
                    total_bytes: 0,
                    chunks: 0,
                },
            },
            fr_wire::clipboard::Context {
                channel: 78,
                ..context(Role::Host)
            },
            &L,
            &mut record,
        )
        .unwrap();
        let record = if malformed {
            &[0u8; 24][..]
        } else {
            &record[..n]
        };
        assert!(
            lane.receive(record, &mut Platform::default(), || client(0))
                .is_err()
        );
        assert!(lane.is_closed());
        assert_eq!(lane.retained_bytes(), 0);
        input.tick(client(0)).unwrap();
    }
}

#[test]
fn plain_ticket_ids_cannot_replace_accepted_ticket_authority() {
    let (mut input, mut lane) = attached();
    assert!(
        input
            .ticket(InputTicketId::from_raw(50), client(0))
            .is_err()
    );
    assert!(lane.offer(1, "denied", None, client(0)).is_err());
}

#[test]
fn panic_at_every_clock_or_transport_callback_consumes_lane_without_replay() {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    struct PanicGate;
    impl fr_wire::clipboard::session::RecordSink for PanicGate {
        fn try_send(
            &mut self,
            _: &[u8],
        ) -> Result<
            fr_wire::clipboard::session::Admission,
            fr_wire::clipboard::session::TransportFailure,
        > {
            panic!("uncertain transport")
        }
    }
    for panic_at in 1..=2 {
        let (mut input, mut lane) = attached();
        lane.offer(1, "secret", None, client(0)).unwrap();
        let mut calls = 0;
        let mut gate = Gate::default();
        let mut scratch = scratch();
        assert!(
            catch_unwind(AssertUnwindSafe(|| lane.pump(
                &mut scratch,
                &mut gate,
                || {
                    calls += 1;
                    assert_ne!(calls, panic_at, "clock fixture panic");
                    client(0)
                }
            )))
            .is_err()
        );
        assert!(lane.is_closed());
        assert_eq!(lane.retained_bytes(), 0);
        assert_eq!(gate.calls, 0);
        input.tick(client(0)).unwrap();
        assert!(lane.offer(2, "not replayed", None, client(0)).is_err());
    }
    let (_input, mut lane) = attached();
    lane.offer(1, "secret", None, client(0)).unwrap();
    let mut scratch = scratch();
    assert!(
        catch_unwind(AssertUnwindSafe(|| lane.pump(
            &mut scratch,
            &mut PanicGate,
            || client(0)
        )))
        .is_err()
    );
    assert!(lane.is_closed());
    assert_eq!(lane.retained_bytes(), 0);
    assert!(scratch.iter().all(|b| *b == 0));
    for panic_at in 1..=2 {
        let (_input, mut lane) = attached();
        let records = incoming("secret");
        let mut platform = Platform::default();
        for record in &records[..2] {
            lane.receive(record, &mut platform, || client(0)).unwrap();
        }
        let mut calls = 0;
        assert!(
            catch_unwind(AssertUnwindSafe(|| lane.receive(
                &records[2],
                &mut platform,
                || {
                    calls += 1;
                    assert_ne!(calls, panic_at, "clock fixture panic");
                    client(0)
                }
            )))
            .is_err()
        );
        assert!(lane.is_closed());
        assert_eq!(lane.retained_bytes(), 0);
        assert_eq!(platform.text, [] as [String; 0]);
        assert_eq!(platform.cancelled, usize::from(panic_at == 2));
    }
}

#[test]
fn better_clock_correlation_and_renewal_do_not_extend_started_item_deadline() {
    // Initial exchange has 100 us uncertainty. The later exchange has none.
    let (mut input, mut lane) = attached();
    lane.offer(1, "original", None, client(0)).unwrap();
    let mut gate = Gate::default();
    assert_eq!(
        lane.pump(&mut scratch(), &mut gate, || client(0)),
        Ok(Pump::RecordAccepted)
    );
    ready(&mut input, 1, 1_000_000);
    ready(&mut input, 2, 2_000_000);
    let tighter = fr_media::freshness::ClockCorrelation::new(
        fr_media::freshness::ClockSample {
            host_boot: grant().request.parent.host_boot,
            client_sent_us: client(2_000_000).0,
            client_received_us: client(2_000_000).0,
            host_sample_us: H + 2_000_000,
        },
        fr_media::freshness::ClockPolicy {
            drift_ppm: 0,
            ..fr_media::freshness::ClockPolicy::default()
        },
    )
    .unwrap();
    input
        .accept_ticket(
            &ticket(1, H + 2_000_000, H + 3_500_000),
            tighter,
            client(2_000_000),
        )
        .unwrap();
    // Original host lease boundary is H+3M. A tighter estimate must not stop
    // the accounting clock for 100us, extending the old transfer's budget.
    let p = lane
        .pump(&mut scratch(), &mut gate, || client(2_999_900))
        .unwrap();
    assert!(matches!(p, Pump::CancelAccepted(_)));
    assert!(matches!(
        clipboard::decode(&gate.last, context(Role::Controller), &L)
            .unwrap()
            .body,
        Body::Cancel(clipboard::CancelReason::Expired)
    ));
    assert_eq!(lane.retained_bytes(), 0);
    assert!(!lane.is_closed());
    input.tick(client(2_999_900)).unwrap();
}

#[test]
fn queued_or_sent_control_response_never_proves_host_lease_extension() {
    use fr_wire::{
        authority::{self, Binding, Message, Scope},
        input::{InputDelivery, InputDirection},
    };
    for sent in [false, true] {
        let (mut input, mut lane) = attached();
        input.enable_control_renewal(7, client(0)).unwrap();
        ready(&mut input, 1, 1_000_000);
        ready(&mut input, 2, 2_000_000);
        let challenge = Message::Challenge {
            scope: Scope::Control(grant().lease),
            nonce: 99,
            deadline_micros: H + 3_000_000,
        };
        let mut b = [0; 256];
        let n = authority::encode(
            challenge,
            Binding {
                channel: 7,
                session: grant().request.parent.remote_session,
            },
            &L,
            &mut b,
            InputDirection::HostToViewer,
            InputDelivery::Reliable,
        )
        .unwrap();
        input
            .accept_control_challenge(&b[..n], client(2_000_000))
            .unwrap();
        if sent {
            input.control_response_sent(client(2_000_000)).unwrap();
        }
        assert!(
            lane.offer(1, "not a host acknowledgment", None, client(3_000_000))
                .is_err()
        );
    }
}

#[path = "clipboard_controller/lane.rs"]
mod lane;

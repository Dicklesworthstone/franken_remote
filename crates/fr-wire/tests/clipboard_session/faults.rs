use super::*;

#[test]
fn either_off_on_switch_fences_every_payload_stage_at_final_check() {
    for peer in [false, true] {
        for stage in 0..3 {
            let input = owner(10_000_000);
            let mut source = session(&input, Role::Host);
            let old = stamp(source.offer(1, "secret", None, at(0)).unwrap());
            let switch = if peer {
                source.peer_switch()
            } else {
                source.local_switch()
            };
            let mut gate = Gate::default();
            for _ in 0..stage {
                assert_eq!(pump(&mut source, &mut gate, 0), Pump::RecordAccepted);
            }
            let calls = gate.calls;
            let mut clock_calls = 0;
            let mut scratch = [0xa5; 1024];
            assert_eq!(
                source.pump(&mut scratch, &mut gate, || {
                    clock_calls += 1;
                    if clock_calls == 2 {
                        switch.set_enabled(false);
                        switch.set_enabled(true);
                    }
                    at(0)
                }),
                Ok(Pump::Deferred)
            );
            assert_eq!(gate.calls, calls);
            assert_eq!(source.retained_bytes(), 0);
            assert!(scratch.iter().all(|b| *b == 0));
            if stage != 0 {
                assert_eq!(pump(&mut source, &mut gate, 0), Pump::CancelAccepted(old));
                assert_eq!(
                    decode(&gate.last, context(Role::Host), &ProtocolLimits::ABSOLUTE)
                        .unwrap()
                        .body,
                    Body::Cancel(CancelReason::Disabled)
                );
            }
            assert_eq!(pump(&mut source, &mut gate, 0), Pump::Idle);
            assert!(input.monitor().deadline(at(0)).is_ok());
            assert_eq!(
                stamp(source.offer(2, "fresh", None, at(0)).unwrap()).sequence,
                2
            );
        }
    }
}
#[test]
fn disabled_switch_still_allows_release_only_cancel_but_no_new_item() {
    let input = owner(10_000_000);
    let peer_input = owner(10_000_000);
    let mut source = session(&input, Role::Host);
    let mut target = session(&peer_input, Role::Controller);
    let old = stamp(source.offer(1, "secret", None, at(0)).unwrap());
    let mut gate = Gate::default();
    let mut platform = Platform::default();
    assert_eq!(pump(&mut source, &mut gate, 0), Pump::RecordAccepted);
    target.receive(&gate.last, &mut platform, || at(0)).unwrap();
    source.set_enabled(false, true);
    assert_eq!(source.retained_bytes(), 0);
    assert_eq!(
        source.offer(2, "not allowed", None, at(0)),
        Err(SessionError::Clipboard(Error::Disabled))
    );
    assert_eq!(pump(&mut source, &mut gate, 0), Pump::CancelAccepted(old));
    target.receive(&gate.last, &mut platform, || at(0)).unwrap();
    assert_eq!(target.retained_bytes(), 0);
    assert_eq!(pump(&mut source, &mut gate, 0), Pump::Suspended);
    assert_eq!(platform.text, [] as [String; 0]);
}
#[test]
fn renewal_does_not_extend_an_items_original_authorization_cap() {
    let mut input = owner(3_000_000);
    let mut source = session(&input, Role::Host);
    source.offer(1, "secret", None, at(1_000_000)).unwrap();
    assert_eq!(source.outgoing_deadline(), Some(at(3_000_000)));
    input
        .issue_observation_challenge(10, at(2_000_000))
        .unwrap();
    input.renew_observation(10, at(2_000_000)).unwrap();
    input.issue_control_challenge(20, at(2_000_000)).unwrap();
    input.renew_control(20, at(2_000_000)).unwrap();
    assert_eq!(input.monitor().deadline(at(3_000_000)), Ok(at(5_000_000)));
    assert!(source.maintain(at(3_000_000)).unwrap().outgoing_expired);
    assert_eq!(source.retained_bytes(), 0);
    assert!(!source.is_closed());
}
#[test]
fn stalled_incoming_item_expires_without_cancelling_a_newer_outgoing_item() {
    let input = owner(10_000_000);
    let peer_input = owner(10_000_000);
    let mut local = session(&input, Role::Host);
    let mut remote = session(&peer_input, Role::Controller);
    remote.offer(1, "incoming", None, at(0)).unwrap();
    let mut gate = Gate::default();
    let mut platform = Platform::default();
    assert_eq!(pump(&mut remote, &mut gate, 0), Pump::RecordAccepted);
    local.receive(&gate.last, &mut platform, || at(0)).unwrap();
    local.offer(2, "outgoing", None, at(2_000_000)).unwrap();
    assert_eq!(local.retained_bytes(), 16);
    let report = local.maintain(at(3_000_000)).unwrap();
    assert!(report.incoming_expired);
    assert!(!report.outgoing_expired);
    assert_eq!(local.retained_bytes(), 8);
    assert_eq!(local.outgoing_deadline(), Some(at(5_000_000)));
}
#[test]
fn expiry_after_encoding_discards_payload_and_preserves_only_cancel_metadata() {
    let input = owner(10_000_000);
    let mut source = session(&input, Role::Host);
    let old = stamp(source.offer(1, "secret", None, at(0)).unwrap());
    let mut gate = Gate::default();
    assert_eq!(pump(&mut source, &mut gate, 0), Pump::RecordAccepted);
    let mut scratch = [0xa5; 1024];
    let mut calls = 0;
    assert_eq!(
        source.pump(&mut scratch, &mut gate, || {
            calls += 1;
            at(if calls == 1 { 2_999_999 } else { 3_000_000 })
        }),
        Ok(Pump::Deferred)
    );
    assert_eq!(gate.calls, 1);
    assert_eq!(source.retained_bytes(), 0);
    assert!(scratch.iter().all(|b| *b == 0));
    assert_eq!(
        pump(&mut source, &mut gate, 3_000_000),
        Pump::CancelAccepted(old)
    );
    assert_eq!(
        decode(&gate.last, context(Role::Host), &ProtocolLimits::ABSOLUTE)
            .unwrap()
            .body,
        Body::Cancel(CancelReason::Expired)
    );
}
#[test]
fn oversize_replacement_retires_obsolete_text_without_resetting_sequence_floor() {
    let input = owner(10_000_000);
    let mut source = session(&input, Role::Host);
    let old = stamp(source.offer(1, "old", None, at(0)).unwrap());
    let mut gate = Gate::default();
    assert_eq!(pump(&mut source, &mut gate, 0), Pump::RecordAccepted);
    assert_eq!(
        source.offer(2, &"x".repeat(1_048_577), None, at(0)),
        Err(SessionError::Wire(fr_wire::WireError::ResourceLimit))
    );
    assert_eq!(source.retained_bytes(), 0);
    let new = stamp(source.offer(3, "new", None, at(0)).unwrap());
    assert_eq!(new.sequence, 3);
    assert_eq!(pump(&mut source, &mut gate, 0), Pump::CancelAccepted(old));
    assert_eq!(pump(&mut source, &mut gate, 0), Pump::RecordAccepted);
    assert_eq!(
        decode(&gate.last, context(Role::Host), &ProtocolLimits::ABSOLUTE)
            .unwrap()
            .stamp,
        new
    );
}
#[test]
fn malformed_inbound_framing_closes_both_directions_without_revoking_input() {
    for size in 0..24 {
        let input = owner(10_000_000);
        let mut local = session(&input, Role::Host);
        local.offer(1, "secret", None, at(0)).unwrap();
        let mut platform = Platform::default();
        assert!(matches!(
            local.receive(&vec![0; size], &mut platform, || at(0)),
            Err(SessionError::Wire(_))
        ));
        assert!(local.is_closed());
        assert_eq!(local.retained_bytes(), 0);
        assert_eq!(platform.text, [] as [String; 0]);
        assert!(input.monitor().deadline(at(0)).is_ok());
    }
}
#[test]
fn receive_observing_a_switch_generation_also_discards_outgoing_work() {
    let input = owner(10_000_000);
    let peer_input = owner(10_000_000);
    let mut local = session(&input, Role::Host);
    let mut remote = session(&peer_input, Role::Controller);
    local.offer(1, "old local", None, at(0)).unwrap();
    remote.offer(2, "incoming", None, at(0)).unwrap();
    let mut gate = Gate::default();
    assert_eq!(pump(&mut remote, &mut gate, 0), Pump::RecordAccepted);
    let switch = local.peer_switch();
    switch.set_enabled(false);
    switch.set_enabled(true);
    assert_eq!(
        local.receive(&gate.last, &mut Platform::default(), || at(0)),
        Err(SessionError::Clipboard(Error::Disabled))
    );
    assert_eq!(local.retained_bytes(), 0);
    assert_eq!(pump(&mut local, &mut gate, 0), Pump::Idle);
}
#[test]
fn clock_regression_and_original_owner_drop_permanently_fence_send() {
    for regress in [false, true] {
        let input = owner(10_000_000);
        let mut local = session(&input, Role::Host);
        local.offer(1, "secret", None, at(10)).unwrap();
        if regress {
            assert_eq!(
                local.maintain(at(9)),
                Err(SessionError::Clipboard(Error::Clock))
            );
            assert!(input.monitor().deadline(at(10)).is_err());
        } else {
            drop(input);
            let replacement = owner(10_000_000);
            assert!(replacement.monitor().deadline(at(10)).is_ok());
            assert!(matches!(
                local.maintain(at(10)),
                Err(SessionError::Clipboard(Error::Authority(_)))
            ));
        }
        assert!(local.is_closed());
        assert_eq!(local.retained_bytes(), 0);
        assert_eq!(
            local.offer(2, "no replay", None, at(11)),
            Err(SessionError::Clipboard(Error::Closed))
        );
    }
}
#[test]
fn uncertain_transport_effect_and_caught_panic_are_terminal_not_retryable() {
    struct Failing {
        panic: bool,
        entered: usize,
    }
    impl RecordSink for Failing {
        fn try_send(&mut self, _: &[u8]) -> Result<Admission, TransportFailure> {
            self.entered += 1;
            assert!(!self.panic, "injected admission panic");
            Err(TransportFailure)
        }
    }
    for panic in [false, true] {
        let input = owner(10_000_000);
        let mut local = session(&input, Role::Host);
        local.offer(1, "secret", None, at(0)).unwrap();
        let mut scratch = [0xa5; 1024];
        let mut failing = Failing { panic, entered: 0 };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            local.pump(&mut scratch, &mut failing, || at(0))
        }));
        if panic {
            assert!(result.is_err());
        } else {
            assert_eq!(result.unwrap(), Err(SessionError::Transport));
        }
        assert!(local.is_closed());
        assert_eq!(local.retained_bytes(), 0);
        assert!(scratch.iter().all(|b| *b == 0));
        assert_eq!(
            local.pump(&mut scratch, &mut failing, || at(0)),
            Err(SessionError::Clipboard(Error::Closed))
        );
        assert_eq!(failing.entered, 1);
        assert!(input.monitor().deadline(at(0)).is_ok());
    }
}
#[test]
fn panic_during_native_publication_clears_both_directions_and_prepared_state() {
    struct Failing {
        cleanup: usize,
    }
    impl ClipboardSink for Failing {
        fn prepare(&mut self, _: &str, _: Stamp) -> Result<(), PlatformError> {
            Ok(())
        }
        fn publish(&mut self, _: &str, _: Stamp) -> Publication {
            panic!("injected native publication panic");
        }
        fn cancel_prepared(&mut self) {
            self.cleanup += 1;
        }
    }
    let input = owner(10_000_000);
    let peer_input = owner(10_000_000);
    let mut local = session(&input, Role::Host);
    let mut remote = session(&peer_input, Role::Controller);
    local.offer(1, "old local", None, at(0)).unwrap();
    remote.offer(2, "incoming", None, at(0)).unwrap();
    let mut gate = Gate::default();
    let mut failing = Failing { cleanup: 0 };
    for _ in 0..2 {
        assert_eq!(pump(&mut remote, &mut gate, 0), Pump::RecordAccepted);
        local.receive(&gate.last, &mut failing, || at(0)).unwrap();
    }
    assert!(matches!(
        pump(&mut remote, &mut gate, 0),
        Pump::ItemAccepted(_)
    ));
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            local.receive(&gate.last, &mut failing, || at(0))
        }))
        .is_err()
    );
    assert_eq!(failing.cleanup, 1);
    assert!(local.is_closed());
    assert_eq!(local.retained_bytes(), 0);
    assert!(input.monitor().deadline(at(0)).is_ok());
}
#[test]
fn publication_uncertainty_is_not_retried_and_refusal_does_not_erase_local_work() {
    for outcome in [
        Publication::UnknownEffect,
        Publication::NotSubmitted(PlatformError::Unavailable),
    ] {
        let input = owner(10_000_000);
        let peer_input = owner(10_000_000);
        let mut local = session(&input, Role::Host);
        let mut remote = session(&peer_input, Role::Controller);
        local.offer(1, "local", None, at(0)).unwrap();
        let remote_stamp = stamp(remote.offer(2, "incoming", None, at(0)).unwrap());
        let mut platform = Platform {
            outcome: Some(outcome),
            ..Platform::default()
        };
        let commit = deliver(&mut remote, &mut local, &mut platform);
        assert_eq!(
            local
                .receive(&commit, &mut platform, || at(0))
                .unwrap()
                .unwrap()
                .publication,
            outcome
        );
        assert_eq!(platform.text.len(), 1);
        if outcome == Publication::UnknownEffect {
            assert_eq!(local.retained_bytes(), 0);
            assert_eq!(
                local.offer(3, "incoming", Some(remote_stamp), at(0)),
                Ok(Offer::EchoSuppressed)
            );
        } else {
            assert_eq!(local.retained_bytes(), "local".len());
            assert!(matches!(
                local.offer(3, "incoming", Some(remote_stamp), at(0)),
                Ok(Offer::Queued(_))
            ));
        }
    }
}
#[test]
fn small_scratch_refuses_without_consuming_or_exposing_a_record() {
    let input = owner(10_000_000);
    let mut local = session(&input, Role::Host);
    local.offer(1, "data", None, at(0)).unwrap();
    let mut gate = Gate::default();
    for size in 0..89 {
        let mut scratch = vec![0xa5; size];
        assert_eq!(
            local.pump(&mut scratch, &mut gate, || at(0)),
            Err(SessionError::Wire(fr_wire::WireError::BufferTooSmall))
        );
        assert!(scratch.iter().all(|b| *b == 0));
    }
    assert_eq!(gate.calls, 0);
    assert_eq!(pump(&mut local, &mut gate, 0), Pump::RecordAccepted);
    for size in 0..97 {
        let mut scratch = vec![0xa5; size];
        assert_eq!(
            local.pump(&mut scratch, &mut gate, || at(0)),
            Err(SessionError::Wire(fr_wire::WireError::BufferTooSmall))
        );
        assert!(scratch.iter().all(|b| *b == 0));
    }
    assert_eq!(gate.calls, 1);
    assert_eq!(pump(&mut local, &mut gate, 0), Pump::RecordAccepted);
}
#[test]
fn unapproved_observer_foreign_binding_and_wrong_lane_are_refused() {
    let input = owner(10_000_000);
    let cases = [
        (
            context(Role::Host),
            false,
            SessionError::Clipboard(Error::Permission),
        ),
        (
            context(Role::Observer),
            true,
            SessionError::Wire(fr_wire::WireError::WrongRole),
        ),
        (
            Context {
                lane: Lane::Other,
                ..context(Role::Host)
            },
            true,
            SessionError::Wire(fr_wire::WireError::WrongChannel),
        ),
        (
            Context {
                channel: 0,
                ..context(Role::Host)
            },
            true,
            SessionError::Wire(fr_wire::WireError::InvalidBinding),
        ),
        (
            Context {
                scope: Binding {
                    lease: InputLeaseId::from_raw(99),
                    ..context(Role::Host).scope
                },
                ..context(Role::Host)
            },
            true,
            SessionError::Wire(fr_wire::WireError::InvalidBinding),
        ),
    ];
    for (context, granted, expected) in cases {
        assert_eq!(
            ClipboardChannel::new(&input, context, ProtocolLimits::ABSOLUTE, granted, at(0))
                .unwrap_err(),
            expected
        );
    }
}

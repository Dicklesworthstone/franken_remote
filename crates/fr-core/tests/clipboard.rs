use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    clipboard::*,
    ids::*,
    input::*,
    input_submission::{Capabilities, InputSession},
    limits::{LimitOverrides, ProtocolLimits},
    time::HostInstant,
};
fn at(us: u64) -> HostInstant {
    HostInstant::from_micros(us)
}
fn owner() -> InputSession {
    let c = InputCredentials {
        session: RemoteSessionId::from_raw(1),
        lease: InputLeaseId::from_raw(2),
        ticket: InputTicketId::from_raw(3),
        view: InputView {
            geometry: DisplayGeometryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
    };
    let mut a = SessionAuthority::new(c.session, AuthorityPolicy::plan_defaults());
    a.mark_capabilities_checked().unwrap();
    a.authorize_observation(at(0)).unwrap();
    a.mark_view_ready(at(0)).unwrap();
    a.grant_lease(c.lease, at(0)).unwrap();
    a.issue_input_ticket(c.lease, c.ticket, at(0)).unwrap();
    InputSession::new(
        a,
        c,
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
        Capabilities::default(),
        at(0),
    )
    .unwrap()
}
fn receiver(input: &InputSession) -> ClipboardSession {
    ClipboardSession::new(input, Endpoint::Host, ProtocolLimits::ABSOLUTE, true, at(0)).unwrap()
}
fn begin(c: &ClipboardSession, seq: u64, total: u32, chunks: u32) -> Begin {
    Begin {
        binding: c.binding(),
        stamp: Stamp {
            id: u128::from(seq),
            source: Endpoint::Controller,
            sequence: seq,
        },
        total_bytes: total,
        chunks,
    }
}
#[derive(Default)]
struct Sink {
    effects: Vec<String>,
    cleanup: usize,
    outcome: Option<Publication>,
}
impl ClipboardSink for Sink {
    fn prepare(&mut self, _: &str, _: Stamp) -> Result<(), PlatformError> {
        Ok(())
    }
    fn publish(&mut self, text: &str, _: Stamp) -> Publication {
        self.effects.push(text.to_owned());
        self.outcome.unwrap_or(Publication::SubmittedToOs)
    }
    fn cancel_prepared(&mut self) {
        self.cleanup += 1;
    }
}
fn receive(c: &mut ClipboardSession, seq: u64, text: &[u8]) -> Begin {
    let b = begin(
        c,
        seq,
        u32::try_from(text.len()).unwrap(),
        u32::from(!text.is_empty()),
    );
    c.begin(b, at(0)).unwrap();
    if !text.is_empty() {
        c.chunk(b.stamp, 0, 0, text, at(0)).unwrap();
    }
    b
}
#[test]
fn complete_unicode_and_empty_items_publish_once_not_as_paste() {
    for text in ["", "private 🦀 café\n\0tail"] {
        let input = owner();
        let mut c = receiver(&input);
        let mut sink = Sink::default();
        let b = receive(&mut c, 1, text.as_bytes());
        let first = c
            .commit(b.stamp, b.total_bytes, &mut sink, || at(1))
            .unwrap();
        assert_eq!(first.publication, Publication::SubmittedToOs);
        assert_eq!(sink.effects, [text]);
        assert_eq!(sink.cleanup, 1);
        assert_eq!(c.buffered_bytes(), 0);
        assert_eq!(c.reserved_bytes(), 0);
        assert_eq!(
            c.commit(b.stamp, b.total_bytes, &mut sink, || at(2)),
            Ok(first)
        );
        assert_eq!(sink.effects.len(), 1);
        assert_eq!(
            c.commit(b.stamp, b.total_bytes + 1, &mut sink, || at(3)),
            Err(Error::Incomplete)
        );
    }
}
#[test]
fn split_utf8_is_validated_only_after_complete_reassembly() {
    let input = owner();
    let mut c = receiver(&input);
    let mut sink = Sink::default();
    let b = begin(&c, 1, 4, 2);
    c.begin(b, at(0)).unwrap();
    c.chunk(b.stamp, 0, 0, &[0xf0, 0x9f], at(0)).unwrap();
    c.chunk(b.stamp, 1, 2, &[0xa6, 0x80], at(0)).unwrap();
    c.commit(b.stamp, 4, &mut sink, || at(1)).unwrap();
    assert_eq!(sink.effects, ["🦀"]);
}
#[test]
fn full_one_mib_is_bounded_and_never_one_large_chunk() {
    let input = owner();
    let mut c = receiver(&input);
    let mut sink = Sink::default();
    let b = begin(&c, 1, 1_048_576, 64);
    c.begin(b, at(0)).unwrap();
    assert_eq!(c.reserved_bytes(), 1_048_576);
    for i in 0..64 {
        c.chunk(
            b.stamp,
            i,
            i * 16_384,
            &[b'x'; MAX_CHUNK_BYTES],
            at(u64::from(i)),
        )
        .unwrap();
    }
    c.commit(b.stamp, b.total_bytes, &mut sink, || at(100))
        .unwrap();
    assert_eq!(sink.effects[0].len(), 1_048_576);
    assert_eq!(c.reserved_bytes(), 0);
}
#[test]
fn permission_is_separate_and_revoked_input_owner_cannot_enable_clipboard() {
    let mut input = owner();
    assert!(matches!(
        ClipboardSession::new(
            &input,
            Endpoint::Host,
            ProtocolLimits::ABSOLUTE,
            false,
            at(0)
        ),
        Err(Error::Permission)
    ));
    input.revoke();
    assert!(matches!(
        ClipboardSession::new(
            &input,
            Endpoint::Host,
            ProtocolLimits::ABSOLUTE,
            true,
            at(0)
        ),
        Err(Error::Authority(_))
    ));
}
#[test]
fn bounds_and_metadata_are_checked_before_allocation() {
    let input = owner();
    let mut c = receiver(&input);
    for (size, chunks) in [
        (1_048_577, 65),
        (1, 2),
        (0, 1),
        (1, 0),
        (1025, 1025),
        (16_385, 1),
    ] {
        let b = begin(&c, 1, size, chunks);
        assert_eq!(c.begin(b, at(0)), Err(Error::Limit));
        assert_eq!(c.reserved_bytes(), 0);
    }
    let limits = ProtocolLimits::with_overrides(LimitOverrides {
        max_clipboard_item_bytes: Some(64),
        ..LimitOverrides::default()
    })
    .unwrap();
    let mut c = ClipboardSession::new(&input, Endpoint::Host, limits, true, at(0)).unwrap();
    assert_eq!(c.begin(begin(&c, 1, 65, 1), at(0)), Err(Error::Limit));
}
#[test]
fn incorrect_binding_and_source_never_allocate() {
    let input = owner();
    let mut c = receiver(&input);
    let mut b = begin(&c, 1, 4, 1);
    b.binding.lease = InputLeaseId::from_raw(99);
    assert_eq!(c.begin(b, at(0)), Err(Error::Binding));
    b.binding = c.binding();
    b.stamp.source = Endpoint::Host;
    assert_eq!(c.begin(b, at(0)), Err(Error::Source));
    assert_eq!(c.reserved_bytes(), 0);
}
#[test]
fn wrong_order_offset_empty_and_oversized_chunks_retire_the_transfer() {
    for case in 0..4 {
        let input = owner();
        let mut c = receiver(&input);
        let b = begin(&c, 1, 4, 1);
        c.begin(b, at(0)).unwrap();
        let result = match case {
            0 => c.chunk(b.stamp, 1, 0, b"data", at(0)),
            1 => c.chunk(b.stamp, 0, 1, b"data", at(0)),
            2 => c.chunk(b.stamp, 0, 0, b"", at(0)),
            _ => c.chunk(b.stamp, 0, 0, b"extra", at(0)),
        };
        assert_eq!(result, Err(Error::ChunkOrder));
        assert_eq!(c.reserved_bytes(), 0);
        assert_eq!(c.begin(b, at(0)), Err(Error::Replay));
    }
}
#[test]
fn invalid_utf8_and_incomplete_commit_never_reach_platform() {
    let input = owner();
    let mut c = receiver(&input);
    let mut sink = Sink::default();
    let b = receive(&mut c, 1, &[0xff]);
    assert_eq!(
        c.commit(b.stamp, 1, &mut sink, || at(0)),
        Err(Error::InvalidUtf8)
    );
    let b = begin(&c, 2, 4, 2);
    c.begin(b, at(0)).unwrap();
    c.chunk(b.stamp, 0, 0, b"ab", at(0)).unwrap();
    assert_eq!(
        c.commit(b.stamp, 4, &mut sink, || at(0)),
        Err(Error::Incomplete)
    );
    assert_eq!(sink.effects.len(), 0);
    assert_eq!(sink.cleanup, 0);
    assert_eq!(c.reserved_bytes(), 0);
}
#[test]
fn cancel_does_not_touch_newer_transfer_or_reset_replay_floor() {
    let input = owner();
    let mut c = receiver(&input);
    let a = receive(&mut c, 1, b"old");
    c.cancel(a.stamp).unwrap();
    assert_eq!(c.begin(a, at(0)), Err(Error::Replay));
    let b = receive(&mut c, 2, b"new");
    assert_eq!(c.cancel(a.stamp), Err(Error::UnknownTransfer));
    assert_eq!(c.buffered_bytes(), 3);
    c.cancel(b.stamp).unwrap();
    assert_eq!(c.reserved_bytes(), 0);
}
#[test]
fn disabled_switches_clear_buffers_and_never_resurrect_old_records() {
    let input = owner();
    let mut c = receiver(&input);
    let b = receive(&mut c, 1, b"private");
    let switch = c.local_switch();
    switch.set_enabled(false);
    switch.set_enabled(true);
    assert_eq!(c.maintain(at(1)), Err(Error::Disabled));
    assert_eq!(c.reserved_bytes(), 0);
    assert_eq!(c.begin(b, at(2)), Err(Error::Replay));
    c.peer_switch().set_enabled(false);
    assert_eq!(c.begin(begin(&c, 2, 1, 1), at(3)), Err(Error::Disabled));
}
#[test]
fn owner_drop_same_numeric_replacement_and_clock_regression_are_terminal() {
    let input = owner();
    let mut c = receiver(&input);
    receive(&mut c, 1, b"secret");
    drop(input);
    let replacement = owner();
    assert!(matches!(c.maintain(at(1)), Err(Error::Authority(_))));
    assert!(c.is_closed());
    assert_eq!(c.reserved_bytes(), 0);
    assert!(!replacement.monitor().is_revoked());
    let mut c = receiver(&replacement);
    c.maintain(at(100)).unwrap();
    assert_eq!(c.maintain(at(99)), Err(Error::Clock));
    assert!(replacement.monitor().is_revoked());
}
#[test]
fn newer_local_value_wins_over_incomplete_remote_transfer() {
    let input = owner();
    let mut c = receiver(&input);
    let mut sink = Sink::default();
    let b = receive(&mut c, 1, b"old remote");
    assert_eq!(c.local_change(None, at(0)), Ok(true));
    assert_eq!(
        c.commit(b.stamp, b.total_bytes, &mut sink, || at(0)),
        Err(Error::LocalChanged)
    );
    assert_eq!(sink.effects.len(), 0);
}
#[test]
fn echo_suppression_uses_exact_source_provenance_including_uncertain_effect() {
    for outcome in [Publication::SubmittedToOs, Publication::UnknownEffect] {
        let input = owner();
        let mut c = receiver(&input);
        let mut sink = Sink {
            outcome: Some(outcome),
            ..Sink::default()
        };
        let b = receive(&mut c, 1, b"hello");
        c.commit(b.stamp, 5, &mut sink, || at(1)).unwrap();
        assert_eq!(c.local_change(Some(b.stamp), at(2)), Ok(false));
        let mut other = b.stamp;
        other.id += 1;
        assert_eq!(c.local_change(Some(other), at(3)), Ok(true));
        assert_eq!(c.local_change(None, at(4)), Ok(true));
    }
}
#[test]
fn unknown_effect_receipt_is_not_retried_or_promoted_to_success() {
    let input = owner();
    let mut c = receiver(&input);
    let mut sink = Sink {
        outcome: Some(Publication::UnknownEffect),
        ..Sink::default()
    };
    let b = receive(&mut c, 1, b"hello");
    let first = c.commit(b.stamp, 5, &mut sink, || at(0)).unwrap();
    assert_eq!(first.publication, Publication::UnknownEffect);
    assert_eq!(c.commit(b.stamp, 5, &mut sink, || at(0)), Ok(first));
    assert_eq!(sink.effects.len(), 1);
}
#[test]
fn final_clock_sample_after_prepare_prevents_expired_publication() {
    let input = owner();
    let mut c = receiver(&input);
    let mut sink = Sink::default();
    let b = receive(&mut c, 1, b"secret");
    let mut now = 0;
    assert!(
        c.commit(b.stamp, b.total_bytes, &mut sink, || {
            now += 1;
            if now == 1 { at(0) } else { at(3_000_000) }
        })
        .is_err()
    );
    assert_eq!(sink.effects.len(), 0);
    assert_eq!(sink.cleanup, 1);
    assert_eq!(c.reserved_bytes(), 0);
}
#[test]
fn renewed_input_authority_cannot_extend_an_old_transfer_deadline() {
    let mut input = owner();
    let mut c = receiver(&input);
    let mut sink = Sink::default();
    let b = receive(&mut c, 1, b"secret");
    input
        .issue_observation_challenge(11, at(1_000_000))
        .unwrap();
    input.renew_observation(11, at(1_000_001)).unwrap();
    input.issue_control_challenge(12, at(1_000_002)).unwrap();
    input.renew_control(12, at(1_000_003)).unwrap();
    assert_eq!(
        c.commit(b.stamp, b.total_bytes, &mut sink, || at(3_000_000)),
        Err(Error::Expired)
    );
    assert!(!input.monitor().is_revoked());
    assert_eq!(sink.effects.len(), 0);
}
#[test]
fn disabling_during_preparation_fences_even_an_off_on_cycle() {
    struct SwitchSink {
        switch: ClipboardSwitch,
        submitted: bool,
        cleaned: bool,
    }
    impl ClipboardSink for SwitchSink {
        fn prepare(&mut self, _: &str, _: Stamp) -> Result<(), PlatformError> {
            self.switch.set_enabled(false);
            self.switch.set_enabled(true);
            Ok(())
        }
        fn publish(&mut self, _: &str, _: Stamp) -> Publication {
            self.submitted = true;
            Publication::SubmittedToOs
        }
        fn cancel_prepared(&mut self) {
            self.cleaned = true;
        }
    }
    let input = owner();
    let mut c = receiver(&input);
    let b = receive(&mut c, 1, b"secret");
    let mut sink = SwitchSink {
        switch: c.local_switch(),
        submitted: false,
        cleaned: false,
    };
    assert_eq!(
        c.commit(b.stamp, b.total_bytes, &mut sink, || at(0)),
        Err(Error::Disabled)
    );
    assert!(!sink.submitted);
    assert!(sink.cleaned);
    assert!(!input.monitor().is_revoked());
}
#[test]
fn debug_and_errors_do_not_include_text_or_opaque_transfer_id() {
    let input = owner();
    let mut c = receiver(&input);
    let b = receive(&mut c, 1, b"TOP SECRET SENTINEL");
    let debug = format!("{c:?} {b:?} {}", Error::InvalidUtf8);
    assert!(!debug.contains("TOP SECRET"));
    assert!(!debug.contains("00000000000000000000000000000001"));
}

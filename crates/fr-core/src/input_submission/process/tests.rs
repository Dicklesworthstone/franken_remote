use super::*;
use crate::input_submission::Capability;

fn bounds() -> InputBounds {
    InputBounds::new(DesktopPoint { x: -1920, y: 0 }, 1920, 1080).unwrap()
}
fn caps() -> Capabilities {
    Capabilities::default()
        .with(Capability::Keys)
        .with(Capability::Absolute)
        .with(Capability::Buttons)
}
fn key(transition: KeyTransition) -> Operation {
    Operation::Key {
        key: PhysicalKey::new(4).unwrap(),
        transition,
    }
}
fn operations() -> [Operation; 12] {
    [
        key(KeyTransition::Press),
        key(KeyTransition::Release),
        key(KeyTransition::Repeat),
        Operation::Absolute(DesktopPoint { x: -5, y: i32::MAX }),
        Operation::Button {
            button: PointerButton::Forward,
            pressed: true,
        },
        Operation::Button {
            button: PointerButton::Primary,
            pressed: false,
        },
        Operation::Relative { x: i32::MIN, y: 7 },
        Operation::Scroll {
            x: -65_536,
            y: 3,
            unit: ScrollUnit::Lines,
        },
        Operation::Wheel {
            direction: WheelDirection::Down,
            pressed: true,
        },
        Operation::Wheel {
            direction: WheelDirection::Left,
            pressed: false,
        },
        Operation::Text('\u{10ffff}'),
        Operation::Text('a'),
    ]
}

#[test]
fn every_request_and_reply_round_trips_with_its_sequence() {
    let mut requests = vec![
        Request::Hello {
            epoch: u128::MAX,
            bounds: bounds(),
            required: caps(),
        },
        Request::Cancel,
        Request::Cleanup,
        Request::Stop,
        Request::Probe { epoch: 1 },
        Request::Probe { epoch: u128::MAX },
    ];
    for op in operations() {
        requests.push(Request::Prepare(op));
        requests.push(Request::Submit {
            operation: op,
            not_after_ns: u64::MAX,
        });
        if op.is_release() {
            requests.push(Request::Release(op));
        }
    }
    for (index, request) in requests.into_iter().enumerate() {
        let sequence = u64::try_from(index).unwrap() + 1;
        let bytes = encode_request(sequence, request).unwrap();
        assert_eq!(decode_request(&bytes), Ok((sequence, request)));
        // A request frame is never accepted as a reply, and vice versa.
        assert_eq!(decode_reply(&bytes), Err(CodecError::Kind));
    }
    let replies = [
        Reply::Ready {
            epoch: 1,
            capabilities: caps(),
            repeat_requires_pair: true,
            line_scroll_requires_pairs: false,
        },
        Reply::Ready {
            epoch: u128::MAX,
            capabilities: Capabilities::default(),
            repeat_requires_pair: false,
            line_scroll_requires_pairs: true,
        },
        Reply::Refused(PlatformError::GeometryChanged),
        Reply::Prepared,
        Reply::PrepareFailed(PlatformError::Permission),
        Reply::Submitted,
        Reply::NotSubmitted(PlatformError::Unavailable),
        Reply::Unknown,
        Reply::Expired,
        Reply::Fenced,
        Reply::Cancelled,
        Reply::Cleaned(true),
        Reply::Cleaned(false),
        Reply::Stopped,
    ];
    for reply in replies {
        let bytes = encode_reply(u64::MAX, reply).unwrap();
        assert_eq!(decode_reply(&bytes), Ok((u64::MAX, reply)));
        assert_eq!(decode_request(&bytes), Err(CodecError::Kind));
    }
}

#[test]
fn malformed_frames_are_refused_before_any_field_is_used() {
    let good = encode_request(
        9,
        Request::Submit {
            operation: key(KeyTransition::Press),
            not_after_ns: 5,
        },
    )
    .unwrap();
    let with = |at: usize, value: u8| {
        let mut bytes = good;
        bytes[at] = value;
        bytes
    };
    assert_eq!(decode_request(&with(0, b'X')), Err(CodecError::Magic));
    assert_eq!(decode_request(&with(4, 3)), Err(CodecError::Version));
    assert_eq!(decode_request(&with(4, 0)), Err(CodecError::Version));
    assert_eq!(decode_request(&with(5, 0)), Err(CodecError::Kind));
    assert_eq!(decode_request(&with(5, 9)), Err(CodecError::Kind));
    assert_eq!(decode_request(&with(6, 1)), Err(CodecError::Reserved));
    assert_eq!(decode_request(&with(7, 1)), Err(CodecError::Reserved));
    // Unused operation bytes, unused body bytes and the final byte.
    for at in [16 + 4, 16 + 15, 16 + 24, 63] {
        assert_eq!(
            decode_request(&with(at, 1)),
            Err(CodecError::Padding),
            "{at}"
        );
    }
    let mut zero_sequence = good;
    zero_sequence[8..16].fill(0);
    assert_eq!(decode_request(&zero_sequence), Err(CodecError::Sequence));
    assert_eq!(decode_request(&good[..63]), Err(CodecError::Length));
    let mut long = good.to_vec();
    long.push(0);
    assert_eq!(decode_request(&long), Err(CodecError::Length));
    // Zero deadline is never a valid "no deadline" encoding.
    let mut forever = good;
    forever[32..40].fill(0);
    assert_eq!(decode_request(&forever), Err(CodecError::Value));
    // Reply kinds with bodies reject every out-of-range value and padding.
    let refused = encode_reply(1, Reply::Refused(PlatformError::Unsupported)).unwrap();
    let mut code = refused;
    code[16] = 5;
    assert_eq!(decode_reply(&code), Err(CodecError::Value));
    let mut padded = refused;
    padded[17] = 1;
    assert_eq!(decode_reply(&padded), Err(CodecError::Padding));
    let mut cleaned = encode_reply(1, Reply::Cleaned(true)).unwrap();
    cleaned[16] = 2;
    assert_eq!(decode_reply(&cleaned), Err(CodecError::Value));
    let mut stopped = encode_reply(1, Reply::Stopped).unwrap();
    stopped[20] = 1;
    assert_eq!(decode_reply(&stopped), Err(CodecError::Padding));
    let ready = encode_reply(
        1,
        Reply::Ready {
            epoch: 3,
            capabilities: caps(),
            repeat_requires_pair: true,
            line_scroll_requires_pairs: true,
        },
    )
    .unwrap();
    let mut flags = ready;
    flags[16 + 18] = 4;
    assert_eq!(decode_reply(&flags), Err(CodecError::Value));
    let mut unknown_capability = ready;
    unknown_capability[16 + 16] = 1;
    assert_eq!(decode_reply(&unknown_capability), Err(CodecError::Value));
    let mut no_epoch = ready;
    no_epoch[16..32].fill(0);
    assert_eq!(decode_reply(&no_epoch), Err(CodecError::Value));
}

#[test]
fn invalid_operation_fields_and_hello_values_are_refused() {
    let prepare = encode_request(1, Request::Prepare(key(KeyTransition::Press))).unwrap();
    let patched = |edits: &[(usize, u8)]| {
        let mut bytes = prepare;
        for (at, value) in edits {
            bytes[16 + at] = *value;
        }
        decode_request(&bytes)
    };
    // Reserved USB usage, invalid transition, unknown operation kind.
    assert_eq!(patched(&[(2, 0xa5)]), Err(CodecError::Value));
    assert_eq!(patched(&[(3, 3)]), Err(CodecError::Value));
    assert_eq!(patched(&[(0, 0)]), Err(CodecError::Value));
    assert_eq!(patched(&[(0, 8)]), Err(CodecError::Value));
    // Button: out-of-range button, non-boolean pressed.
    assert_eq!(
        patched(&[(0, 3), (1, 6), (2, 0), (3, 0)]),
        Err(CodecError::Value)
    );
    assert_eq!(
        patched(&[(0, 3), (1, 1), (2, 2), (3, 0)]),
        Err(CodecError::Value)
    );
    // Wheel direction 4; scroll unit 2; surrogate and out-of-range scalars.
    assert_eq!(
        patched(&[(0, 6), (1, 4), (2, 1), (3, 0)]),
        Err(CodecError::Value)
    );
    assert_eq!(
        patched(&[(0, 5), (1, 0), (2, 0), (3, 0), (9, 2)]),
        Err(CodecError::Value)
    );
    assert_eq!(
        patched(&[(0, 7), (1, 0), (2, 0), (3, 0xd8), (4, 0)]),
        Err(CodecError::Value)
    );
    assert_eq!(
        patched(&[(0, 7), (1, 0), (2, 0x11), (3, 0), (4, 0)]),
        Err(CodecError::Value)
    );
    let hello = encode_request(
        1,
        Request::Hello {
            epoch: 7,
            bounds: bounds(),
            required: caps(),
        },
    )
    .unwrap();
    let mut no_epoch = hello;
    no_epoch[16..32].fill(0);
    assert_eq!(decode_request(&no_epoch), Err(CodecError::Value));
    let mut empty = hello;
    empty[16 + 24..16 + 28].fill(0);
    assert_eq!(decode_request(&empty), Err(CodecError::Value));
    let mut unknown_capability = hello;
    unknown_capability[16 + 32] = 1;
    assert_eq!(decode_request(&unknown_capability), Err(CodecError::Value));
    assert_eq!(
        encode_request(
            1,
            Request::Hello {
                epoch: 0,
                bounds: bounds(),
                required: caps(),
            }
        ),
        Err(CodecError::Value)
    );
    assert_eq!(encode_request(0, Request::Stop), Err(CodecError::Sequence));
    // A probe carries only its launch identity: no bounds, no capabilities.
    let probe = encode_request(1, Request::Probe { epoch: 7 }).unwrap();
    let mut no_epoch = probe;
    no_epoch[16..32].fill(0);
    assert_eq!(decode_request(&no_epoch), Err(CodecError::Value));
    for at in [32, 48, 63] {
        let mut padded = probe;
        padded[at] = 1;
        assert_eq!(decode_request(&padded), Err(CodecError::Padding), "{at}");
    }
    assert_eq!(
        encode_request(1, Request::Probe { epoch: 0 }),
        Err(CodecError::Value)
    );
}

#[test]
fn release_frames_carry_only_release_transitions() {
    let releases: Vec<_> = operations()
        .into_iter()
        .filter(|o| o.is_release())
        .collect();
    assert_eq!(
        releases,
        [
            key(KeyTransition::Release),
            Operation::Button {
                button: PointerButton::Primary,
                pressed: false,
            },
            Operation::Wheel {
                direction: WheelDirection::Left,
                pressed: false,
            },
        ]
    );
    for op in operations() {
        let encoded = encode_request(3, Request::Release(op));
        if op.is_release() {
            assert_eq!(
                decode_request(&encoded.unwrap()),
                Ok((3, Request::Release(op)))
            );
        } else {
            assert_eq!(encoded, Err(CodecError::Value), "{op:?}");
            // A forged release frame naming a press/position is refused too.
            let mut forged = encode_request(3, Request::Prepare(op)).unwrap();
            forged[5] = 4;
            assert_eq!(decode_request(&forged), Err(CodecError::Value), "{op:?}");
        }
    }
}

#[test]
fn signals_are_fixed_datagrams_with_direction_named_kinds() {
    for signal in [Signal::Fence, Signal::LocalRevoke, Signal::IndicatorFailed] {
        let bytes = encode_signal(signal);
        assert_eq!(decode_signal(&bytes), Ok(signal));
        assert_eq!(decode_signal(&bytes[..15]), Err(CodecError::Length));
        let mut padded = bytes;
        padded[15] = 1;
        assert_eq!(decode_signal(&padded), Err(CodecError::Padding));
        for unknown in [0, 4] {
            let mut kind = bytes;
            kind[5] = unknown;
            assert_eq!(decode_signal(&kind), Err(CodecError::Kind));
        }
        let mut magic = bytes;
        magic[0] = b'X';
        assert_eq!(decode_signal(&magic), Err(CodecError::Magic));
    }
    // A command frame is never a signal.
    assert_eq!(
        decode_signal(&encode_request(1, Request::Stop).unwrap()),
        Err(CodecError::Length)
    );
}

#[test]
fn executor_deadline_errs_early_and_never_wraps() {
    let now = HostInstant::from_micros(1_000);
    assert_eq!(
        not_after_ns(50, now, HostInstant::from_micros(1_750)),
        Some(750_050)
    );
    assert_eq!(not_after_ns(50, now, now), None);
    assert_eq!(not_after_ns(50, now, HostInstant::from_micros(999)), None);
    assert_eq!(
        not_after_ns(u64::MAX - 10, now, HostInstant::from_micros(1_001)),
        None
    );
    assert_eq!(
        not_after_ns(0, HostInstant::ORIGIN, HostInstant::from_micros(u64::MAX)),
        None
    );
}

#[test]
fn suspend_clock_profile_refuses_legacy_commands_replies_and_signals() {
    // Independent version-1 frames, not generated by the current encoder.
    // A mixed installed process family must fail at Hello/Ready, before input.
    let mut legacy = [0; FRAME_BYTES];
    legacy[..8].copy_from_slice(b"FRIA\x01\x01\0\0");
    legacy[15] = 1;
    assert_eq!(decode_request(&legacy), Err(CodecError::Version));
    legacy[5] = 0x81;
    assert_eq!(decode_reply(&legacy), Err(CodecError::Version));
    let mut signal = [0; SIGNAL_BYTES];
    signal[..6].copy_from_slice(b"FRIF\x01\x01");
    assert_eq!(decode_signal(&signal), Err(CodecError::Version));

    let current = encode_request(1, Request::Stop).unwrap();
    assert_eq!(&current[..16], b"FRIA\x02\x07\0\0\0\0\0\0\0\0\0\x01");
    assert_eq!(decode_request(&current), Ok((1, Request::Stop)));
}

fn input_clock() -> DeadlineClock {
    DeadlineClock::new(10_000_000_000, HostInstant::from_micros(100_000))
}

#[test]
fn awake_executor_keeps_its_original_exclusive_deadline() {
    let clock = input_clock();
    let until = HostInstant::from_micros(400_000);
    assert_eq!(
        clock.not_after_ns(10_050_000_000, HostInstant::from_micros(150_000), until),
        Some(10_299_999_000)
    );
    assert_eq!(
        clock.not_after_ns(10_299_998_999, HostInstant::from_micros(399_998), until),
        Some(10_299_999_000)
    );
    assert_eq!(
        clock.not_after_ns(10_299_999_000, HostInstant::from_micros(399_999), until),
        None,
        "the native deadline is exclusive even before the next runtime tick"
    );
    assert_eq!(
        clock.not_after_ns(10_299_999_999, until, until),
        None,
        "the runtime deadline is independently exclusive"
    );
}

#[test]
fn suspend_before_translation_cannot_rebase_an_original_input_ticket() {
    let clock = input_clock();
    let runtime = HostInstant::from_micros(150_000);
    let until = HostInstant::from_micros(400_000);
    // Fifty milliseconds awake, then two seconds asleep. The runtime still
    // reports 250 ms left, but the original native deadline is already gone.
    let resumed_ns = 12_050_000_000;
    assert_eq!(clock.not_after_ns(resumed_ns, runtime, until), None);
    assert_eq!(
        not_after_ns(resumed_ns, runtime, until),
        Some(12_300_000_000),
        "the previous per-submit offset would incorrectly renew this lifetime"
    );
}

#[test]
fn short_suspend_consumes_time_without_rejecting_a_still_live_original_ticket() {
    let clock = input_clock();
    let runtime = HostInstant::from_micros(150_000);
    let until = HostInstant::from_micros(400_000);
    // Fifty milliseconds awake and 100 ms suspended consumes half the 300 ms
    // lifetime; only the original 150 ms remains, not the runtime's 250 ms.
    assert_eq!(
        clock.not_after_ns(10_150_000_000, runtime, until),
        Some(10_299_999_000)
    );
    // A later runtime sample must not rebuild the offset either.
    assert_eq!(
        clock.not_after_ns(10_300_000_000, HostInstant::from_micros(300_000), until),
        None
    );
}

#[test]
fn issued_submit_keeps_its_native_deadline_while_the_runtime_is_asleep() {
    let clock = input_clock();
    let until = HostInstant::from_micros(400_000);
    let not_after_ns = clock
        .not_after_ns(10_050_000_000, HostInstant::from_micros(150_000), until)
        .unwrap();
    let wire = encode_request(
        3,
        Request::Submit {
            operation: key(KeyTransition::Press),
            not_after_ns,
        },
    )
    .unwrap();
    let (_, Request::Submit { not_after_ns, .. }) = decode_request(&wire).unwrap() else {
        panic!("original submit required");
    };
    assert!(10_050_000_000 < not_after_ns);
    assert!(
        12_050_000_000 >= not_after_ns,
        "native submission must expire"
    );
    // Cleanup has no authority deadline and remains a release-only command.
    let release = Request::Release(key(KeyTransition::Release));
    assert_eq!(
        decode_request(&encode_request(4, release).unwrap()),
        Ok((4, release))
    );
}

#[test]
fn fresh_clock_sample_can_only_shorten_the_original_input_ceiling() {
    let clock = input_clock();
    // The native sample precedes 50 ms of work before the runtime sample.
    assert_eq!(
        clock.not_after_ns(
            10_100_000_000,
            HostInstant::from_micros(250_000),
            HostInstant::from_micros(400_000),
        ),
        Some(10_249_999_000)
    );
}

#[test]
fn original_clock_regression_and_overflow_never_issue_a_submit() {
    let clock = input_clock();
    let now = HostInstant::from_micros(100_000);
    let until = HostInstant::from_micros(400_000);
    assert_eq!(clock.not_after_ns(9_999_999_999, now, until), None);
    assert_eq!(
        clock.not_after_ns(10_000_000_000, HostInstant::from_micros(99_999), until),
        None
    );
    assert_eq!(clock.not_after_ns(u64::MAX, now, until), None);
    let overflow = DeadlineClock::new(u64::MAX - 10, now);
    assert_eq!(overflow.not_after_ns(u64::MAX - 10, now, until), None);
    assert_eq!(
        clock.not_after_ns(10_000_000_000, now, HostInstant::from_micros(u64::MAX)),
        None
    );
}

#[test]
fn diagnostics_hide_operations_and_epochs() {
    let a = format!(
        "{:?}",
        Request::Submit {
            operation: key(KeyTransition::Press),
            not_after_ns: 77,
        }
    );
    assert_eq!(a, "Submit");
    let hello = format!(
        "{:?}",
        Request::Hello {
            epoch: 0x1234_5678,
            bounds: bounds(),
            required: caps(),
        }
    );
    assert_eq!(hello, "Hello");
    assert_eq!(
        format!("{:?}", Request::Probe { epoch: 0x1234_5678 }),
        "Probe"
    );
    let ready = format!(
        "{:?}",
        Reply::Ready {
            epoch: 0x1234_5678,
            capabilities: caps(),
            repeat_requires_pair: false,
            line_scroll_requires_pairs: false,
        }
    );
    assert!(!ready.contains("305419896") && !ready.contains("12345678"));
}

use fr_core::{
    clipboard::{Binding, Endpoint, Stamp},
    ids::{InputLeaseId, RemoteSessionId},
    limits::{LimitOverrides, ProtocolLimits},
};
use fr_wire::{WireError, clipboard::send::Sender, clipboard::*};
fn context() -> Context {
    Context {
        scope: Binding {
            session: RemoteSessionId::from_raw(1),
            lease: InputLeaseId::from_raw(2),
        },
        channel: 9,
        sender: Role::Controller,
        lane: Lane::Clipboard,
    }
}
fn stamp() -> Stamp {
    Stamp {
        id: 3,
        source: Endpoint::Controller,
        sequence: 4,
    }
}
fn message(body: Body<'_>) -> Message<'_> {
    Message {
        stamp: stamp(),
        body,
    }
}
fn encoded(body: Body<'_>) -> Vec<u8> {
    let mut out = vec![0; 65_536];
    let n = encode(
        message(body),
        context(),
        &ProtocolLimits::ABSOLUTE,
        &mut out,
    )
    .unwrap();
    out.truncate(n);
    out
}
fn fixture(kind: u8, suffix: &[u8]) -> Vec<u8> {
    let len = 57 + suffix.len();
    let mut v = vec![0; 81];
    v[..4].copy_from_slice(b"FRD0");
    v[7] = kind;
    v[12..16].copy_from_slice(&u32::try_from(len).unwrap().to_be_bytes());
    v[19] = 9;
    v[39] = 1;
    v[55] = 2;
    v[71] = 3;
    v[72] = 2;
    v[80] = 4;
    v.extend_from_slice(suffix);
    v
}
#[test]
fn all_four_kinds_match_independent_byte_fixtures() {
    let cases = [
        (
            Body::Begin {
                total_bytes: 3,
                chunks: 1,
            },
            fixture(0x50, &[0, 0, 0, 3, 0, 0, 0, 1]),
        ),
        (
            Body::Chunk {
                index: 0,
                offset: 0,
                bytes: b"abc",
            },
            fixture(
                0x51,
                &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3, b'a', b'b', b'c'],
            ),
        ),
        (
            Body::Commit { total_bytes: 3 },
            fixture(0x52, &[0, 0, 0, 3]),
        ),
        (Body::Cancel(CancelReason::User), fixture(0x53, &[0, 1])),
    ];
    for (body, expected) in cases {
        assert_eq!(encoded(body), expected);
        assert_eq!(
            decode(&expected, context(), &ProtocolLimits::ABSOLUTE),
            Ok(message(body))
        );
        for end in 0..expected.len() {
            assert!(
                decode(&expected[..end], context(), &ProtocolLimits::ABSOLUTE).is_err(),
                "{end}"
            );
        }
        let mut trailing = expected;
        trailing.push(0);
        assert!(decode(&trailing, context(), &ProtocolLimits::ABSOLUTE).is_err());
    }
}
#[test]
fn source_direction_is_checked_independently_of_payload_labels() {
    let data = encoded(Body::Commit { total_bytes: 0 });
    for sender in [Role::Host, Role::Observer] {
        assert_eq!(
            decode(
                &data,
                Context {
                    sender,
                    ..context()
                },
                &ProtocolLimits::ABSOLUTE
            ),
            Err(WireError::WrongRole)
        );
    }
    let mut out = [0; BEGIN_BYTES];
    assert!(
        encode(
            message(Body::Begin {
                total_bytes: 0,
                chunks: 0
            }),
            Context {
                sender: Role::Observer,
                ..context()
            },
            &ProtocolLimits::ABSOLUTE,
            &mut out
        )
        .is_err()
    );
    let host = Message {
        stamp: Stamp {
            source: Endpoint::Host,
            ..stamp()
        },
        body: Body::Commit { total_bytes: 0 },
    };
    let ctx = Context {
        sender: Role::Host,
        ..context()
    };
    let n = encode(host, ctx, &ProtocolLimits::ABSOLUTE, &mut out).unwrap();
    assert_eq!(decode(&out[..n], ctx, &ProtocolLimits::ABSOLUTE), Ok(host));
}
#[test]
fn binding_channel_and_reserved_fields_are_not_payload_authority() {
    let data = encoded(Body::Begin {
        total_bytes: 4,
        chunks: 1,
    });
    assert_eq!(
        decode(
            &data,
            Context {
                lane: Lane::Other,
                ..context()
            },
            &ProtocolLimits::ABSOLUTE
        ),
        Err(WireError::WrongChannel)
    );
    assert_eq!(
        decode(
            &data,
            Context {
                channel: 8,
                ..context()
            },
            &ProtocolLimits::ABSOLUTE
        ),
        Err(WireError::InvalidBinding)
    );
    for offset in [4, 8, 10, 39, 55, 72] {
        let mut bad = data.clone();
        bad[offset] ^= 0xff;
        assert!(
            decode(&bad, context(), &ProtocolLimits::ABSOLUTE).is_err(),
            "{offset}"
        );
    }
    for offset in [71, 80] {
        let mut bad = data.clone();
        bad[offset] = 0;
        assert!(decode(&bad, context(), &ProtocolLimits::ABSOLUTE).is_err());
    }
}
#[test]
fn hostile_chunk_lengths_and_offsets_refuse_without_allocation() {
    let good = encoded(Body::Chunk {
        index: 0,
        offset: 0,
        bytes: b"a",
    });
    for at in [81, 85, 89] {
        let mut bad = good.clone();
        bad[at..at + 4].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(decode(&bad, context(), &ProtocolLimits::ABSOLUTE).is_err());
    }
    let mut out = vec![0; 1_048_576 + CHUNK_OVERHEAD];
    let oversized = vec![1; 16_385];
    assert_eq!(
        encode(
            message(Body::Chunk {
                index: 0,
                offset: 0,
                bytes: &oversized
            }),
            context(),
            &ProtocolLimits::ABSOLUTE,
            &mut out
        ),
        Err(WireError::ResourceLimit)
    );
    assert_eq!(
        decode(&out, context(), &ProtocolLimits::ABSOLUTE),
        Err(WireError::ResourceLimit)
    );
}
#[test]
fn unknown_cancel_reason_and_unrelated_kinds_refuse() {
    let mut data = encoded(Body::Cancel(CancelReason::Failed));
    data[CANCEL_BYTES - 1] = 6;
    assert_eq!(
        decode(&data, context(), &ProtocolLimits::ABSOLUTE),
        Err(WireError::InvalidValue)
    );
    data[7] = 0x40;
    assert_eq!(
        decode(&data, context(), &ProtocolLimits::ABSOLUTE),
        Err(WireError::UnsupportedKind)
    );
}
#[test]
fn bounded_single_byte_mutations_never_panic_or_ignore_fixed_fields() {
    let data = encoded(Body::Begin {
        total_bytes: 4096,
        chunks: 1,
    });
    for offset in 0..data.len() {
        for xor in [1, 128, 255] {
            let mut changed = data.clone();
            changed[offset] ^= xor;
            if let Ok(parsed) = decode(&changed, context(), &ProtocolLimits::ABSOLUTE) {
                let mut out = [0; BEGIN_BYTES];
                let n = encode(parsed, context(), &ProtocolLimits::ABSOLUTE, &mut out).unwrap();
                assert_eq!(out[..n], changed);
            }
        }
    }
}
#[test]
fn sender_is_lazy_repeatable_under_backpressure_and_releases_payload() {
    let text = "🦀".repeat(262_144);
    let mut sender = Sender::new(&text, stamp(), context(), ProtocolLimits::ABSOLUTE).unwrap();
    assert_eq!(sender.retained_bytes(), text.len());
    let mut out = vec![0; 16_384 + CHUNK_OVERHEAD];
    let mut second = out.clone();
    let mut records = 0;
    let mut received = Vec::new();
    while let Some(n) = sender.encode_next(&mut out).unwrap() {
        assert_eq!(sender.encode_next(&mut second).unwrap(), Some(n));
        assert_eq!(out[..n], second[..n]);
        if let Body::Chunk { bytes, .. } = decode(&out[..n], context(), &ProtocolLimits::ABSOLUTE)
            .unwrap()
            .body
        {
            received.extend_from_slice(bytes);
        }
        sender.accepted();
        records += 1;
    }
    assert_eq!(records, 66);
    assert_eq!(received, text.as_bytes());
    assert!(sender.is_finished());
    assert_eq!(sender.retained_bytes(), 0);
    sender.accepted();
    assert_eq!(sender.encode_next(&mut out).unwrap(), None);
}
#[test]
fn smaller_negotiated_records_split_without_oversized_control_exception() {
    let limits = ProtocolLimits::with_overrides(LimitOverrides {
        max_control_message_bytes: Some(4096),
        ..LimitOverrides::default()
    })
    .unwrap();
    let text = "界".repeat(5000);
    let mut sender = Sender::new(&text, stamp(), context(), limits).unwrap();
    let mut out = [0; 4096];
    let mut bytes = Vec::new();
    let mut frames = 0;
    while let Some(n) = sender.encode_next(&mut out).unwrap() {
        assert!(n <= 4096);
        if let Body::Chunk { bytes: chunk, .. } =
            decode(&out[..n], context(), &limits).unwrap().body
        {
            bytes.extend_from_slice(chunk);
        }
        sender.accepted();
        frames += 1;
    }
    assert_eq!(bytes, text.as_bytes());
    assert_eq!(frames, 6);
}
#[test]
fn empty_item_uses_begin_and_commit_without_zero_length_chunk() {
    let mut sender = Sender::new("", stamp(), context(), ProtocolLimits::ABSOLUTE).unwrap();
    let mut out = [0; BEGIN_BYTES];
    let n = sender.encode_next(&mut out).unwrap().unwrap();
    assert_eq!(
        decode(&out[..n], context(), &ProtocolLimits::ABSOLUTE)
            .unwrap()
            .body,
        Body::Begin {
            total_bytes: 0,
            chunks: 0
        }
    );
    sender.accepted();
    let n = sender.encode_next(&mut out).unwrap().unwrap();
    assert_eq!(
        decode(&out[..n], context(), &ProtocolLimits::ABSOLUTE)
            .unwrap()
            .body,
        Body::Commit { total_bytes: 0 }
    );
    sender.accepted();
    assert!(sender.is_finished());
}
#[test]
fn diagnostics_redact_text() {
    let data = encoded(Body::Chunk {
        index: 0,
        offset: 0,
        bytes: b"SENSITIVE SENTINEL",
    });
    let decoded = decode(&data, context(), &ProtocolLimits::ABSOLUTE).unwrap();
    let sender = Sender::new(
        "SENSITIVE SENTINEL",
        stamp(),
        context(),
        ProtocolLimits::ABSOLUTE,
    )
    .unwrap();
    assert!(!format!("{decoded:?} {sender:?}").contains("SENSITIVE"));
}

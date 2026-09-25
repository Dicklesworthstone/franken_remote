use super::*;

fn stamp(sequence: u64) -> Stamp {
    Stamp {
        id: u128::MAX - u128::from(sequence),
        source: Endpoint::Controller,
        sequence,
    }
}
fn requests() -> Vec<Request> {
    vec![
        Request::Hello {
            epoch: 7,
            max_item_bytes: MAX_ITEM_BYTES,
        },
        Request::Watch,
        Request::Changes,
        Request::Prepare {
            stamp: stamp(3),
            revision: Some(u64::MAX),
            len: MAX_ITEM_BYTES,
        },
        Request::Prepare {
            stamp: stamp(4),
            revision: None,
            len: 0,
        },
        Request::Publish {
            stamp: stamp(5),
            not_after_ns: 1,
        },
        Request::CancelPrepared,
        Request::BeginRead,
        Request::PollRead,
        Request::CancelRead,
        Request::Suspend,
        Request::Stop,
    ]
}
fn replies() -> Vec<Reply> {
    let mut all = vec![
        Reply::Ready { epoch: u128::MAX },
        Reply::Refused(PlatformError::Permission),
        Reply::Watching { revision: 1 },
        Reply::Changes {
            revision: 9,
            latest: None,
            settled: false,
        },
        Reply::Changes {
            revision: 9,
            latest: Some(Change {
                revision: 9,
                has_selection: true,
                origin: Some(Stamp {
                    source: Endpoint::Host,
                    ..stamp(8)
                }),
            }),
            settled: true,
        },
        Reply::Changes {
            revision: 10,
            latest: Some(Change {
                revision: 10,
                has_selection: false,
                origin: None,
            }),
            settled: true,
        },
        Reply::Prepared { revision: 2 },
        Reply::PrepareFailed {
            revision: 2,
            error: PlatformError::LocalChanged,
        },
        Reply::Done { revision: 3 },
        Reply::ReadPending { revision: 4 },
        Reply::ReadText {
            revision: 5,
            origin: None,
            len: 12,
        },
        Reply::ReadText {
            revision: 5,
            origin: Some(stamp(6)),
            len: MAX_ITEM_BYTES,
        },
        Reply::Stopped,
    ];
    for publication in [
        Publication::SubmittedToOs,
        Publication::NotSubmitted(PlatformError::Unavailable),
        Publication::UnknownEffect,
    ] {
        all.push(Reply::Published {
            revision: 6,
            publication,
        });
    }
    for failure in [
        Failure::Platform(PlatformError::Unsupported),
        Failure::Unsupported,
        Failure::NotWatching,
        Failure::AlreadyWatching,
        Failure::Exhausted,
        Failure::Busy,
        Failure::NotReading,
        Failure::NoSelection,
        Failure::Expired,
        Failure::LocalChanged,
        Failure::Limit,
        Failure::Allocation,
        Failure::InvalidUtf8,
        Failure::Malformed,
    ] {
        all.push(Reply::Failed {
            revision: 7,
            failure,
        });
    }
    all
}

#[test]
fn every_request_and_reply_round_trips_exactly() {
    for (i, request) in requests().into_iter().enumerate() {
        let sequence = i as u64 + 1;
        let bytes = encode_request(sequence, request).unwrap();
        assert_eq!(
            decode_request(&bytes, MAX_ITEM_BYTES).unwrap(),
            (sequence, request)
        );
    }
    for (i, reply) in replies().into_iter().enumerate() {
        let sequence = u64::MAX - i as u64;
        let bytes = encode_reply(sequence, reply).unwrap();
        assert_eq!(
            decode_reply(&bytes, MAX_ITEM_BYTES).unwrap(),
            (sequence, reply)
        );
    }
}

#[test]
fn an_oversized_item_is_refused_from_its_header_before_any_payload() {
    // Encoding never announces more than the absolute ceiling.
    let oversized = Request::Prepare {
        stamp: stamp(1),
        revision: None,
        len: MAX_ITEM_BYTES + 1,
    };
    assert_eq!(encode_request(1, oversized), Err(CodecError::Length));
    assert_eq!(
        encode_reply(
            1,
            Reply::ReadText {
                revision: 1,
                origin: None,
                len: MAX_ITEM_BYTES + 1
            }
        ),
        Err(CodecError::Length)
    );
    // A peer bound lowered by Hello is enforced from the 64-byte header
    // alone: decoding returns before the caller reserves or reads payload.
    let at = |len| {
        encode_request(
            1,
            Request::Prepare {
                stamp: stamp(1),
                revision: None,
                len,
            },
        )
        .unwrap()
    };
    assert!(decode_request(&at(1024), 1024).is_ok());
    assert_eq!(decode_request(&at(1025), 1024), Err(CodecError::Length));
    let text = |len| {
        encode_reply(
            1,
            Reply::ReadText {
                revision: 1,
                origin: None,
                len,
            },
        )
        .unwrap()
    };
    assert!(decode_reply(&text(64), 64).is_ok());
    assert_eq!(decode_reply(&text(65), 64), Err(CodecError::Length));
    // A header forged past the absolute ceiling is refused even with a
    // careless (too large) caller bound.
    let mut forged = at(0);
    forged[HEADER_BYTES + 34..HEADER_BYTES + 38].copy_from_slice(&u32::MAX.to_be_bytes());
    assert_eq!(decode_request(&forged, u32::MAX), Err(CodecError::Length));
    // Hello cannot raise the ceiling either.
    for max_item_bytes in [0, MAX_ITEM_BYTES + 1] {
        assert_eq!(
            encode_request(
                1,
                Request::Hello {
                    epoch: 1,
                    max_item_bytes
                }
            ),
            Err(CodecError::Value)
        );
    }
}

#[test]
fn malformed_frames_are_refused_not_guessed() {
    let good = encode_request(3, Request::Watch).unwrap();
    assert_eq!(
        decode_request(&good[..63], MAX_ITEM_BYTES),
        Err(CodecError::Length)
    );
    let mut bad = good;
    bad[0] ^= 1;
    assert_eq!(decode_request(&bad, MAX_ITEM_BYTES), Err(CodecError::Magic));
    let mut bad = good;
    bad[4] = 2;
    assert_eq!(
        decode_request(&bad, MAX_ITEM_BYTES),
        Err(CodecError::Version)
    );
    let mut bad = good;
    bad[6] = 1;
    assert_eq!(
        decode_request(&bad, MAX_ITEM_BYTES),
        Err(CodecError::Reserved)
    );
    let mut bad = good;
    bad[8..16].fill(0);
    assert_eq!(
        decode_request(&bad, MAX_ITEM_BYTES),
        Err(CodecError::Sequence)
    );
    let mut bad = good;
    bad[63] = 1;
    assert_eq!(
        decode_request(&bad, MAX_ITEM_BYTES),
        Err(CodecError::Padding)
    );
    let mut bad = good;
    bad[5] = 0x7f;
    assert_eq!(decode_request(&bad, MAX_ITEM_BYTES), Err(CodecError::Kind));
    // Replies and requests are distinct kinds: neither parses as the other.
    assert_eq!(decode_reply(&good, MAX_ITEM_BYTES), Err(CodecError::Kind));
    let stopped = encode_reply(3, Reply::Stopped).unwrap();
    assert_eq!(
        decode_request(&stopped, MAX_ITEM_BYTES),
        Err(CodecError::Kind)
    );
    // An invalid stamp (zero ID or sequence, unknown source) never decodes.
    let publish = encode_request(
        1,
        Request::Publish {
            stamp: stamp(1),
            not_after_ns: 9,
        },
    )
    .unwrap();
    let mut bad = publish;
    bad[HEADER_BYTES..HEADER_BYTES + 16].fill(0);
    assert_eq!(decode_request(&bad, MAX_ITEM_BYTES), Err(CodecError::Value));
    let mut bad = publish;
    bad[HEADER_BYTES + 16] = 3;
    assert_eq!(decode_request(&bad, MAX_ITEM_BYTES), Err(CodecError::Value));
    // A publication deadline of zero would mean "no deadline": refused.
    let mut bad = publish;
    bad[HEADER_BYTES + 25..HEADER_BYTES + 33].fill(0);
    assert_eq!(decode_request(&bad, MAX_ITEM_BYTES), Err(CodecError::Value));
    assert_eq!(
        encode_request(
            1,
            Request::Publish {
                stamp: stamp(1),
                not_after_ns: 0
            }
        ),
        Err(CodecError::Value)
    );
    // Metadata of an absent change must be zero.
    let mut bad = encode_reply(
        1,
        Reply::Changes {
            revision: 1,
            latest: None,
            settled: true,
        },
    )
    .unwrap();
    bad[HEADER_BYTES + 8] |= 0b10;
    assert_eq!(decode_reply(&bad, MAX_ITEM_BYTES), Err(CodecError::Value));
}

#[test]
fn debug_names_the_message_kind_only() {
    let prepare = Request::Prepare {
        stamp: stamp(77),
        revision: Some(12_345),
        len: 4242,
    };
    assert_eq!(format!("{prepare:?}"), "Prepare");
    let text = Reply::ReadText {
        revision: 12_345,
        origin: Some(stamp(77)),
        len: 4242,
    };
    assert_eq!(format!("{text:?}"), "ReadText");
    for rendered in [
        format!(
            "{:?}",
            Request::Hello {
                epoch: 0xdead_beef,
                max_item_bytes: 99
            }
        ),
        format!("{:?}", Reply::Ready { epoch: 0xdead_beef }),
        format!(
            "{:?}",
            Reply::Changes {
                revision: 12_345,
                latest: Some(Change {
                    revision: 12_345,
                    has_selection: true,
                    origin: Some(stamp(77))
                }),
                settled: true
            }
        ),
    ] {
        for secret in ["4242", "12345", "deadbeef", "3735928559", "340282366"] {
            assert!(!rendered.contains(secret), "{rendered}");
        }
    }
}

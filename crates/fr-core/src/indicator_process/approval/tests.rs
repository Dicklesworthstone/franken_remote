use super::*;

fn hello(controlling: bool) -> Frame {
    Frame {
        kind: if controlling {
            Kind::ApproveControlDetails
        } else {
            Kind::ApproveViewDetails
        },
        sequence: 1,
        epoch: 29,
    }
}
fn context() -> Context {
    Context::new(
        "nVerified_37",
        Some("workstation.example."),
        Features::PLAYBACK,
        11,
    )
    .unwrap()
}

#[test]
fn bounded_context_round_trips_each_exact_role_and_negotiated_feature_set() {
    for controlling in [false, true] {
        for flags in 0..=Features::ALL {
            let features = Features(flags);
            let context = Context::new("nOriginal", None, features, 1).unwrap();
            if !features.valid(controlling) {
                assert_eq!(context.encode(hello(controlling)), Err(Error));
                continue;
            }
            let encoded = context.encode(hello(controlling)).unwrap();
            assert_eq!(
                Context::decode(hello(controlling), &encoded),
                Ok(context.clone())
            );
            let details = context.details(hello(controlling)).unwrap();
            assert!(details.contains(if controlling {
                "across the host login session"
            } else {
                "request is read-only"
            }));
            assert_eq!(
                details.contains("including apps on other displays"),
                features.contains(Features::PLAYBACK)
            );
        }
    }
}

#[test]
fn context_refuses_truncation_extension_unknown_flags_and_hidden_suffixes() {
    let encoded = context().encode(hello(false)).unwrap();
    for end in 0..CONTEXT_BYTES {
        assert_eq!(Context::decode(hello(false), &encoded[..end]), Err(Error));
    }
    let mut extended = encoded.to_vec();
    extended.push(0);
    assert_eq!(Context::decode(hello(false), &extended), Err(Error));
    for index in [
        0,
        1,
        2,
        3,
        4,
        7,
        END,
        CONTEXT_BYTES - 1,
        NODE_START + 50,
        NAME_START + 80,
    ] {
        let mut bytes = encoded;
        bytes[index] ^= 1;
        assert_eq!(
            Context::decode(hello(false), &bytes),
            Err(Error),
            "byte {index}"
        );
    }
    for flags in [
        Features::CLIPBOARD.0,
        Features::FILE_SEND.0,
        Features::FILE_RECEIVE.0,
        Features::MICROPHONE.0,
        32,
        255,
    ] {
        let mut bytes = encoded;
        bytes[6] = flags;
        assert_eq!(Context::decode(hello(false), &bytes), Err(Error));
    }
    for (offset, length) in [(40, 0_u16), (40, 129), (42, 255)] {
        let mut bytes = encoded;
        bytes[offset..offset + 2].copy_from_slice(&length.to_be_bytes());
        assert_eq!(Context::decode(hello(false), &bytes), Err(Error));
    }
    let mut zero = encoded;
    zero[32..40].fill(0);
    assert_eq!(Context::decode(hello(false), &zero), Err(Error));
}

#[test]
fn old_roles_another_epoch_sequence_or_role_cannot_reuse_attribution() {
    let encoded = context().encode(hello(false)).unwrap();
    for foreign in [
        Frame {
            epoch: 30,
            ..hello(false)
        },
        Frame {
            epoch: 0,
            ..hello(false)
        },
        Frame {
            sequence: 2,
            ..hello(false)
        },
        hello(true),
    ] {
        assert_eq!(Context::decode(foreign, &encoded), Err(Error));
    }
    for kind in [
        Kind::Open,
        Kind::Check,
        Kind::ApproveView,
        Kind::ApproveControl,
        Kind::ApprovalCheck,
        Kind::Ready,
        Kind::Allowed,
    ] {
        let foreign = Frame {
            kind,
            ..hello(false)
        };
        assert_eq!(Context::decode(foreign, &encoded), Err(Error));
        assert_eq!(context().encode(foreign), Err(Error));
    }
}

#[test]
fn unsafe_or_ambiguous_labels_refuse_without_lossy_identity_substitution() {
    for node in [
        "",
        "node name",
        "node\nAllow",
        "node\t",
        "node/1",
        "node\0",
        "n\u{202e}trusted",
        "n\u{e9}",
    ] {
        assert!(Context::new(node, None, Features::NONE, 1).is_err());
    }
    assert!(Context::new(&"n".repeat(129), None, Features::NONE, 1).is_err());
    for name in [
        "",
        "a..b",
        "-bad.example",
        "bad-.example",
        "evil\nAllow",
        "a\u{202e}.b",
        "a/b",
        ".",
        "a b",
        "a\0b",
    ] {
        assert!(Context::new("nGood", Some(name), Features::NONE, 1).is_err());
    }
    assert!(Context::new("nGood", Some(&"a".repeat(64)), Features::NONE, 1).is_err());
    assert!(Context::new("nGood", None, Features::NONE, 0).is_err());
    let encoded = context().encode(hello(false)).unwrap();
    for index in [NODE_START, NAME_START] {
        for invalid in [b'\n', b'\0', 0xff, b':'] {
            let mut bytes = encoded;
            bytes[index] = invalid;
            assert_eq!(Context::decode(hello(false), &bytes), Err(Error));
        }
    }
}

#[test]
fn complete_maximal_labels_fit_and_same_alias_never_hides_distinct_nodes() {
    let node = "n".repeat(NODE_BYTES);
    let name = format!(
        "{}.{}.{}.{}.",
        "a".repeat(63),
        "b".repeat(63),
        "c".repeat(63),
        "d".repeat(61)
    );
    assert_eq!(name.len(), NAME_BYTES);
    let context = Context::new(&node, Some(&name), Features(Features::ALL), u64::MAX).unwrap();
    let details = context.details(hello(true)).unwrap();
    assert!(details.len() <= MAX_DETAILS_BYTES);
    assert!(details.lines().count() <= 14);
    assert!(details.lines().all(|line| line.len() <= 78));
    assert!(
        details
            .bytes()
            .all(|b| b.is_ascii_graphic() || matches!(b, b' ' | b'\n'))
    );
    let joined = details.replace("\n  ", "");
    assert!(joined.contains(&node));
    assert!(joined.contains(&name));
    assert!(details.contains("display selection follows approval"));
    let a = Context::new("nDistinctA", Some("same.example"), Features::NONE, 2).unwrap();
    let b = Context::new("nDistinctB", Some("same.example"), Features::NONE, 3).unwrap();
    assert_ne!(a.details(hello(false)), b.details(hello(false)));
    assert!(a.details(hello(false)).unwrap().contains("nDistinctA"));
    assert!(b.details(hello(false)).unwrap().contains("nDistinctB"));
    let debug = format!("{a:?}");
    assert!(!debug.contains("nDistinctA") && !debug.contains("same.example"));
}

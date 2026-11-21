use fr_core::limits::ProtocolLimits;
use fr_wire::negotiation::{self, Offer, Role, browser};

fn hello() -> browser::Hello {
    browser::Hello::new(
        Offer {
            versions: vec![0],
            profile: browser::WSS_PROFILE,
            profile_version: 0,
            role: Role::Observe,
            limits: ProtocolLimits::ABSOLUTE,
            capabilities: vec![],
        },
        [0xa5; 32],
    )
    .unwrap()
}

#[test]
fn browser_hello_has_standard_framing_and_cannot_enter_native_negotiation() {
    let mut expected = b"FRD0\0\0\0\x01\0\0\0\0\0\0\0\x50\0\0\0\0\0\0\0\0".to_vec();
    expected.extend_from_slice(&[0, 0, 0, 1, 0, 0, 3, 0, 0, 0]);
    expected.extend_from_slice(&[0, 1, 0, 0, 0, 16, 0, 0, 1, 0, 0, 0, 0, 0, 32, 0]);
    expected.extend_from_slice(&[0, 0, 0, 0, 1, 0, 0, 0, 12, 0, 0, 0, 0, 2, 0, 0, 0]);
    expected.extend_from_slice(&[0, 0, 0, 0, 1]);
    expected.extend_from_slice(&[0xa5; 32]);
    let mut bytes = [0; browser::MAX_RECORD];
    let length = browser::encode_hello(&hello(), browser::MAX_RECORD, &mut bytes).unwrap();
    assert_eq!(&bytes[..length], expected);
    assert_eq!(
        browser::decode_hello(&expected, browser::MAX_RECORD).unwrap(),
        hello()
    );
    assert_eq!(
        negotiation::decode(&expected, browser::MAX_RECORD, 0),
        Err(negotiation::Error::Profile)
    );
    assert!(!format!("{:?}", hello()).contains("165"));
}

#[test]
fn malformed_profile_nonce_limits_and_trailing_bytes_refuse() {
    let mut bytes = [0; browser::MAX_RECORD];
    let length = browser::encode_hello(&hello(), browser::MAX_RECORD, &mut bytes).unwrap();
    for end in 0..length {
        assert!(
            browser::decode_hello(&bytes[..end], browser::MAX_RECORD).is_err(),
            "truncated at {end}"
        );
    }
    assert!(browser::decode_hello(&bytes[..=length], browser::MAX_RECORD).is_err());
    assert!(browser::decode_hello(&bytes[..length], length - 1).is_err());
    for (at, value) in [(30, 1), (32, 1), (length - 33, 0), (16, 1), (6, 1)] {
        let mut malformed = bytes;
        malformed[at] = value;
        assert!(
            browser::decode_hello(&malformed[..length], browser::MAX_RECORD).is_err(),
            "accepted mutation at {at}"
        );
    }
    let mut small = vec![0x55; length - 1];
    assert!(browser::encode_hello(&hello(), browser::MAX_RECORD, &mut small).is_err());
    assert!(small.iter().all(|b| *b == 0x55));
}

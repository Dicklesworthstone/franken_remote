use fr_core::{ids::RemoteSessionId, limits::ProtocolLimits};
use fr_wire::{
    HEADER_BYTES, WireError,
    authority::Binding,
    closure,
    input::{InputDelivery, InputDirection},
    inspection,
    negotiation::{self, Capability, Offer, Role},
    stream::RecordStream,
};

fn offer() -> Offer {
    Offer {
        versions: vec![0],
        profile: negotiation::NATIVE_PROFILE,
        profile_version: negotiation::PROFILE_VERSION,
        role: Role::Observe,
        limits: ProtocolLimits::ABSOLUTE,
        capabilities: vec![],
    }
}
fn request_fixture() -> Vec<u8> {
    let mut bytes = b"FRD0\0\0\0\x1d\0\0\0\0\0\0\0\x13\0\0\0\0\0\0\0\0".to_vec();
    bytes.extend_from_slice(&[0; 16]);
    bytes.extend_from_slice(&[1, 0, 4]);
    bytes
}
fn closed_fixture() -> Vec<u8> {
    let mut bytes = b"FRD0\0\0\0\x1e\0\0\0\0\0\0\0\x1c\0\0\0\0\0\0\0\0".to_vec();
    bytes.extend_from_slice(&[0; 16]);
    bytes.extend_from_slice(&[0, 1, 2, 1]);
    bytes.extend_from_slice(&[0; 8]);
    bytes
}

#[test]
fn independent_fixed_fixtures_and_every_changed_byte_are_checked() {
    for (request, expected) in [(true, request_fixture()), (false, closed_fixture())] {
        let mut out = [0x55; inspection::CLOSED_BYTES];
        let len = if request {
            inspection::encode_request(4096, &mut out)
        } else {
            inspection::encode_closed(4096, &mut out)
        }
        .unwrap();
        assert_eq!(&out[..len], expected);
        let decode = |bytes: &[u8], limit| {
            if request {
                inspection::decode_request(bytes, limit)
            } else {
                inspection::decode_closed(bytes, limit)
            }
        };
        assert_eq!(decode(&expected, 4096), Ok(()));
        for cut in 0..expected.len() {
            assert!(decode(&expected[..cut], 4096).is_err());
            let mut altered = expected.clone();
            altered[cut] ^= 1;
            assert!(decode(&altered, 4096).is_err(), "altered byte {cut}");
        }
        let mut extra = expected.clone();
        extra.push(0);
        assert!(decode(&extra, 4096).is_err());
        assert_eq!(decode(&expected, len - 1), Err(WireError::ResourceLimit));
        assert!(inspection::decode_closed(&request_fixture(), 4096).is_err());
        assert!(inspection::decode_request(&closed_fixture(), 4096).is_err());
        for boundary in 0..=len {
            let mut stream = RecordStream::negotiation(4096, 100).unwrap();
            assert_eq!(stream.push(&expected[..boundary], 1), Ok(boundary));
            assert_eq!(stream.push(&expected[boundary..], 2), Ok(len - boundary));
            assert_eq!(stream.frame(3).unwrap(), Some(expected.as_slice()));
        }
    }
}
#[test]
fn bootstrap_close_does_not_make_zero_a_session_authority_binding() {
    let binding = Binding {
        channel: 0,
        session: RemoteSessionId::from_raw(0),
    };
    let limits = ProtocolLimits::ABSOLUTE;
    assert_eq!(
        closure::decode_request(
            &request_fixture(),
            binding,
            &limits,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
        ),
        Err(WireError::InvalidBinding),
    );
    assert_eq!(
        closure::decode_closed(
            &closed_fixture(),
            binding,
            &limits,
            InputDirection::HostToViewer,
            InputDelivery::Reliable,
        ),
        Err(WireError::InvalidBinding),
    );
    assert!(negotiation::decode(&request_fixture(), 4096, 0).is_err());
    assert!(negotiation::decode(&closed_fixture(), 4096, 0).is_err());
}
#[test]
fn framing_rejects_expensive_or_extended_close_before_payload_allocation() {
    for mut bytes in [request_fixture(), closed_fixture()] {
        bytes[12..16].copy_from_slice(&4096_u32.to_be_bytes());
        let mut stream = RecordStream::negotiation(4096, 100).unwrap();
        assert!(stream.push(&bytes[..HEADER_BYTES], 0).is_err());
        assert_eq!(stream.allocated_bytes(), 0);
    }
    let mut extended = request_fixture();
    extended[23] = 1;
    let mut stream = RecordStream::negotiation(4096, 100).unwrap();
    assert!(stream.push(&extended[..HEADER_BYTES], 0).is_err());
    assert_eq!(stream.allocated_bytes(), 0);
}
#[test]
fn preparation_is_explicit_required_observe_only_and_bound_by_existing_count() {
    let prepared = inspection::prepare_offer(offer()).unwrap();
    assert!(inspection::requested(&prepared));
    assert_eq!(
        inspection::prepare_offer(prepared.clone()),
        Ok(prepared.clone())
    );
    assert_eq!(
        offer().intersect(&prepared),
        Err(negotiation::Error::RequiredCapability)
    );
    let mut controller = offer();
    controller.role = Role::RequestControl;
    assert_eq!(
        inspection::prepare_offer(controller),
        Err(negotiation::Error::Invalid)
    );
    let mut optional = prepared;
    optional.capabilities[0].required = false;
    assert!(!inspection::requested(&optional));
    assert!(inspection::prepare_offer(optional).is_err());

    let mut full = offer();
    full.capabilities = (0..negotiation::MAX_CAPABILITIES)
        .map(|n| Capability {
            name: format!("feature-{n:02}"),
            version: 1,
            required: false,
        })
        .collect();
    assert!(inspection::prepare_offer(full.clone()).is_err());
    let mut peer = full.clone();
    peer.capabilities.pop();
    let peer = inspection::prepare_offer(peer).unwrap();
    let result = inspection::intersect(&full, &peer).unwrap();
    assert_eq!(result.capabilities.len(), negotiation::MAX_CAPABILITIES);
    assert!(inspection::requested(&result));
    peer.check_host(&result).unwrap();
    full.capabilities[23].required = true;
    assert_eq!(
        inspection::intersect(&full, &peer),
        Err(negotiation::Error::RequiredCapability)
    );
}

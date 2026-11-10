use fr_core::limits::ProtocolLimits;
use fr_wire::{
    HEADER_BYTES, WireError,
    directory::{self, Assembler, Entry, Listing},
    inspection,
    negotiation::{self, Offer, Role},
    refusal::{self, Reason, Refused},
    stream::RecordStream,
};
use std::net::{Ipv4Addr, Ipv6Addr};

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
fn entry(i: u16, serving: bool) -> Entry {
    let [hi, lo] = i.to_be_bytes();
    Entry {
        name: format!("machine-{i}.tail1f21e.ts.net"),
        node_id: format!("n{i:04}CNTRL"),
        os: if i.is_multiple_of(2) {
            "macOS"
        } else {
            "linux"
        }
        .into(),
        online: serving || !i.is_multiple_of(3),
        serving,
        ipv4: (i % 5 != 1).then(|| Ipv4Addr::new(100, 64 + (hi & 0x3f), lo, 1)),
        ipv6: (i % 5 != 2).then(|| Ipv6Addr::new(0xfd7a, 0x115c, 0xa1e0, 0, 0, 0, 0, i + 1)),
    }
}
fn listing(n: u16) -> Listing {
    Listing {
        entries: (0..n).map(|i| entry(i, i == 0)).collect(),
        truncated: false,
    }
}
fn assemble(records: &[Vec<u8>], maximum: usize) -> Result<Listing, WireError> {
    let mut assembler = Assembler::default();
    for (i, record) in records.iter().enumerate() {
        let done = assembler.push(record, maximum)?;
        assert_eq!(
            done,
            i + 1 == records.len(),
            "final flag on the last record only"
        );
    }
    assembler.finish()
}

#[test]
fn the_request_is_one_exact_form_and_every_changed_byte_is_refused() {
    let mut expected = b"FRD0\0\0\0\x86\0\0\0\0\0\0\0\x04\0\0\0\0\0\0\0\0".to_vec();
    expected.extend_from_slice(&[0, 1, 0, 0]);
    let mut out = [0x55; directory::REQUEST_BYTES];
    let len = directory::encode_request(4096, &mut out).unwrap();
    assert_eq!(&out[..len], expected);
    assert_eq!(directory::decode_request(&expected, 4096), Ok(()));
    for cut in 0..expected.len() {
        assert!(directory::decode_request(&expected[..cut], 4096).is_err());
        let mut altered = expected.clone();
        altered[cut] ^= 1;
        assert!(
            directory::decode_request(&altered, 4096).is_err(),
            "byte {cut}"
        );
    }
    assert_eq!(
        directory::encode_request(directory::REQUEST_BYTES - 1, &mut out),
        Err(WireError::ResourceLimit)
    );
}

#[test]
fn the_marker_is_required_observe_only_and_never_combined_with_inspection() {
    let prepared = directory::prepare_offer(offer()).unwrap();
    assert!(directory::requested(&prepared));
    assert!(!directory::requested(&offer()));
    let mut control = offer();
    control.role = Role::RequestControl;
    assert_eq!(
        directory::prepare_offer(control),
        Err(negotiation::Error::Invalid)
    );
    let inspecting = inspection::prepare_offer(offer()).unwrap();
    assert_eq!(
        directory::prepare_offer(inspecting.clone()),
        Err(negotiation::Error::Invalid)
    );
    // The host's answer carries the marker; a peer without it is refused.
    let answer = directory::intersect(&offer(), &prepared).unwrap();
    assert!(directory::requested(&answer));
    assert_eq!(
        directory::intersect(&offer(), &offer()),
        Err(negotiation::Error::RequiredCapability)
    );
    // An optional or other-version marker is not this request.
    let mut optional = prepared;
    optional.capabilities[0].required = false;
    assert!(!directory::requested(&optional));
    assert_eq!(
        directory::prepare_offer(optional),
        Err(negotiation::Error::RequiredCapability)
    );
}

#[test]
fn listings_round_trip_in_one_or_many_bounded_records() {
    for (n, maximum) in [
        (1, 4096),
        (4, 4096),
        (256, 4096),
        (256, directory::MIN_RECORD),
    ] {
        let original = listing(n);
        let records = directory::encode_listing(&original, maximum).unwrap();
        assert!(records.len() <= directory::MAX_RECORDS);
        assert!(records.iter().all(|r| r.len() <= maximum));
        if maximum == directory::MIN_RECORD {
            assert!(records.len() > 1, "a small record limit splits the listing");
        }
        for record in &records {
            assert_eq!(directory::check_record(record, maximum), Ok(()));
        }
        assert_eq!(
            assemble(&records, maximum).unwrap(),
            original,
            "{n} at {maximum}"
        );
    }
}

#[test]
fn every_bound_truncates_visibly_instead_of_silently() {
    // More than 256 machines: the first 256 are sent and the listing says so.
    let records = directory::encode_listing(&listing(300), 4096).unwrap();
    let received = assemble(&records, 4096).unwrap();
    assert_eq!(received.entries.len(), directory::MAX_ENTRIES);
    assert!(received.truncated);
    assert_eq!(received.entries[..], listing(300).entries[..256]);

    // Long entries in the smallest records: the record bound truncates.
    let long = Listing {
        entries: (0..200)
            .map(|i| {
                let mut e = entry(i, i == 0);
                let label = format!("{i:0>63}");
                e.name = format!("{label}.{label}.{label}.ts.net");
                e
            })
            .collect(),
        truncated: false,
    };
    let records = directory::encode_listing(&long, directory::MIN_RECORD).unwrap();
    assert_eq!(records.len(), directory::MAX_RECORDS);
    let received = assemble(&records, directory::MIN_RECORD).unwrap();
    assert!(received.truncated);
    assert!(received.entries.len() < long.entries.len());

    // The host's own truncation flag is carried even when everything fits.
    let mut flagged = listing(3);
    flagged.truncated = true;
    let records = directory::encode_listing(&flagged, 4096).unwrap();
    assert!(assemble(&records, 4096).unwrap().truncated);

    // A record limit too small for one entry refuses to encode at all.
    assert_eq!(
        directory::encode_listing(&listing(1), directory::MIN_RECORD - 1),
        Err(WireError::ResourceLimit)
    );
}

#[test]
fn the_encoder_refuses_entries_it_must_never_list() {
    let refuse = |change: &dyn Fn(&mut Listing)| {
        let mut l = listing(3);
        change(&mut l);
        assert!(directory::encode_listing(&l, 4096).is_err());
    };
    refuse(&|l| l.entries.clear());
    refuse(&|l| l.entries[0].serving = false); // the serving host comes first
    refuse(&|l| l.entries[2].serving = true); // and only once
    refuse(&|l| l.entries[0].online = false); // it is answering
    refuse(&|l| l.entries[1].ipv4 = Some(Ipv4Addr::new(192, 168, 1, 2)));
    refuse(&|l| l.entries[1].ipv6 = Some(Ipv6Addr::LOCALHOST));
    refuse(&|l| {
        l.entries[1].ipv4 = None;
        l.entries[1].ipv6 = None;
    });
    refuse(&|l| l.entries[1].name = "Machine.tail1f21e.ts.net".into());
    refuse(&|l| l.entries[1].name = "machine.tail1f21e.ts.net.".into());
    refuse(&|l| l.entries[1].name = "-machine.tail1f21e.ts.net".into());
    refuse(&|l| l.entries[1].name = "machine".into());
    refuse(&|l| l.entries[2].name = l.entries[1].name.clone());
    refuse(&|l| l.entries[2].node_id = l.entries[1].node_id.clone());
    refuse(&|l| l.entries[1].node_id = "n1/../x".into());
    refuse(&|l| l.entries[1].os = "mac\nOS".into());
    refuse(&|l| l.entries[1].os = "x".repeat(directory::MAX_OS + 1));
}

/// Re-encode a record with one change at a byte offset into its payload.
fn altered(record: &[u8], at: usize, value: u8) -> Vec<u8> {
    let mut bytes = record.to_vec();
    bytes[HEADER_BYTES + at] = value;
    bytes
}

#[test]
fn the_assembler_refuses_reordering_repeats_gaps_and_forged_fields() {
    let records = directory::encode_listing(&listing(256), directory::MIN_RECORD).unwrap();
    let maximum = directory::MIN_RECORD;
    assert!(records.len() >= 3);

    // Out of order, repeated, or after the final record.
    let mut a = Assembler::default();
    assert_eq!(a.push(&records[1], maximum), Err(WireError::InvalidValue));
    let mut a = Assembler::default();
    a.push(&records[0], maximum).unwrap();
    assert_eq!(a.push(&records[0], maximum), Err(WireError::InvalidValue));
    let single = directory::encode_listing(&listing(2), 4096).unwrap();
    let mut a = Assembler::default();
    assert!(a.push(&single[0], 4096).unwrap());
    assert_eq!(a.push(&single[0], 4096), Err(WireError::InvalidValue));
    // An unfinished listing is not a listing.
    let mut a = Assembler::default();
    a.push(&records[0], maximum).unwrap();
    assert_eq!(a.finish(), Err(WireError::Truncated));

    // Forged record heads: unknown flags, truncated without final, a zero
    // count, nonzero reserved byte.
    for (at, value) in [(1, 4), (1, 2), (2, 0), (3, 1)] {
        let forged = altered(&single[0], at, value);
        assert!(
            Assembler::default().push(&forged, 4096).is_err(),
            "{at}={value}"
        );
        assert!(directory::check_record(&forged, 4096).is_err());
    }
    // The first entry's flags: unknown bit, or not serving.
    for value in [0x10, 0x01 | 0x04 | 0x08] {
        let forged = altered(&single[0], 4, value);
        assert!(
            Assembler::default().push(&forged, 4096).is_err(),
            "{value:#x}"
        );
    }
    // A second record that also claims to be the serving host.
    let mut a = Assembler::default();
    a.push(&records[0], maximum).unwrap();
    let second = altered(&records[1], 4, 0x01 | 0x02 | 0x04 | 0x08);
    assert!(a.push(&second, maximum).is_err());

    // Uppercase name bytes and an address outside the tailnet ranges.
    let name_at = 4 + 1 + 1;
    let upper = altered(&single[0], name_at, b'M');
    assert!(Assembler::default().push(&upper, 4096).is_err());
    let len = single[0].len();
    let mut outside = single[0].clone();
    // The second entry ends with its IPv6 address; leave the tailnet /48.
    outside[len - 16] = 0xfe;
    assert!(Assembler::default().push(&outside, 4096).is_err());

    // A record above the negotiated maximum, or with an extension area.
    assert_eq!(
        Assembler::default().push(&single[0], single[0].len() - 1),
        Err(WireError::ResourceLimit)
    );
    let mut extended = single[0].clone();
    extended[23] = 1;
    assert!(Assembler::default().push(&extended, 4096).is_err());
}

#[test]
fn the_bootstrap_framer_accepts_the_pair_and_the_typed_refusals_round_trip() {
    let mut request = [0; directory::REQUEST_BYTES];
    directory::encode_request(4096, &mut request).unwrap();
    let records = directory::encode_listing(&listing(40), 4096).unwrap();
    for bytes in std::iter::once(&request[..]).chain(records.iter().map(Vec::as_slice)) {
        let mut stream = RecordStream::negotiation(4096, 1_000_000).unwrap();
        assert_eq!(stream.push(bytes, 0).unwrap(), bytes.len());
        assert_eq!(stream.frame(0).unwrap(), Some(bytes));
    }
    // A request with a different payload length never allocates.
    let mut long = request.to_vec();
    long[15] = 5;
    long.push(0);
    let mut stream = RecordStream::negotiation(4096, 1_000_000).unwrap();
    assert!(stream.push(&long, 0).is_err());
    // A listing with an extension area is refused at the header.
    let mut extended = records[0].clone();
    extended[23] = 4;
    let mut stream = RecordStream::negotiation(4096, 1_000_000).unwrap();
    assert!(stream.push(&extended, 0).is_err());

    for reason in [Reason::DirectoryDisabled, Reason::DirectoryUnavailable] {
        let mut out = [0; refusal::MAX_BYTES];
        let len = refusal::encode(
            Refused::connection(reason),
            0,
            4096,
            &mut out,
            fr_wire::input::InputDelivery::Reliable,
        )
        .unwrap();
        assert_eq!(
            refusal::decode(
                &out[..len],
                0,
                4096,
                fr_wire::input::InputDelivery::Reliable
            )
            .unwrap()
            .reason,
            reason
        );
        assert_eq!(
            negotiation::decode(&out[..len], 4096, 0),
            Err(negotiation::Error::Refused(Refused::connection(reason)))
        );
    }
    // Listing kinds are not negotiation messages.
    assert_eq!(
        negotiation::decode(&records[0], 4096, 0),
        Err(negotiation::Error::Wire(WireError::UnsupportedKind))
    );
}

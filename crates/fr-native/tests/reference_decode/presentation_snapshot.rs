//! Actual confined worker exports of retained pixels, including fitted output.
//! Reads use the original worker pipe and never advance the codec or display.
use super::*;
use fr_core::ids::RecoveryGeneration;
use fr_media::access_unit::FrameKind;
use fr_media::worker::presentation_snapshot::{Absent, Snapshot, Stamp};

fn stamp(unit: &EncodedAccessUnit, recovery: RecoveryGeneration) -> Stamp {
    Stamp {
        frame: unit.frame(),
        configuration: unit.config_generation(),
        recovery,
    }
}
fn relabel(unit: EncodedAccessUnit, frame: u64, kind: FrameKind) -> EncodedAccessUnit {
    EncodedAccessUnit::new(
        &ProtocolLimits::ABSOLUTE,
        FrameId::from_raw(frame),
        kind,
        unit.config_generation(),
        unit.capture_micros(),
        unit.into_bytes(),
    )
    .unwrap()
}
fn read(worker: &mut Worker, expected: Stamp) -> Result<Snapshot, Absent> {
    let reply = worker.request(Kind::ReadPresentation, expected.encode().to_vec());
    match reply.header.kind {
        Kind::PresentationPixels => Ok(Snapshot::decode(reply.into_body()).unwrap()),
        Kind::PresentationAbsent => Err(Absent::decode(reply.body()).unwrap()),
        kind => panic!("unexpected snapshot reply: {kind:?}"),
    }
}
fn submit(worker: &mut Worker, kind: Kind, unit: &EncodedAccessUnit) {
    let reply = worker.request(kind, worker::unit_payload(unit).unwrap());
    assert_eq!(
        reply.header.kind,
        if kind == Kind::Present {
            Kind::Presented
        } else {
            Kind::Decoded
        }
    );
    assert_eq!(reply.body(), unit.frame().as_raw().to_be_bytes());
}

#[test]
fn explicit_exports_match_independent_decode_and_keep_absence_nonfatal() {
    for fit in [false, true] {
        let display = Display::start();
        let mut encoder = encoder();
        let recovery = RecoveryGeneration::from_raw(7);
        let first = relabel(encode(&mut encoder, 0), 0, FrameKind::Idr { recovery });
        let mut baseline = decoder(&first);
        let (mut worker, mut ui, mut fitted) = configured(&display, &first, fit);
        assert_eq!(
            read(&mut worker, stamp(&first, recovery)).err(),
            Some(Absent::NoFrame)
        );
        baseline.submit(&first).unwrap();
        baseline.poll_reference().unwrap();
        submit(&mut worker, Kind::Decode, &first);
        assert_eq!(
            read(&mut worker, stamp(&first, recovery)).err(),
            Some(Absent::NoFrame)
        );

        let next = encode(&mut encoder, 1);
        baseline.submit(&next).unwrap();
        let (_, pixels) = baseline.poll_output().unwrap();
        submit(&mut worker, Kind::Present, &next);
        let expected = stamp(&next, recovery);
        let snapshot = read(&mut worker, expected).unwrap();
        let shown = match &mut fitted {
            Some(fitted) => fitted.render(&pixels).unwrap(),
            None => &pixels,
        };
        assert_eq!(snapshot.stamp(), expected);
        assert_eq!(
            (snapshot.width(), snapshot.height()),
            (shown.width(), shown.height())
        );
        assert_eq!(snapshot.pixels(), shown.pixels());
        assert_eq!(snapshot.pixels(), ui.snapshot().unwrap().pixels());
        assert_eq!(
            (snapshot.width(), snapshot.height()),
            if fit { (96, 96) } else { (192, 128) }
        );

        for stale in [
            Stamp {
                frame: expected.frame.next().unwrap(),
                ..expected
            },
            Stamp {
                configuration: expected.configuration.next().unwrap(),
                ..expected
            },
            Stamp {
                recovery: expected.recovery.next().unwrap(),
                ..expected
            },
        ] {
            assert_eq!(read(&mut worker, stale).err(), Some(Absent::NotCurrent));
        }
        assert_eq!(
            worker.request(Kind::Poll, vec![]).header.kind,
            Kind::NeedInput
        );
        assert_eq!(
            read(&mut worker, expected).unwrap().pixels(),
            snapshot.pixels()
        );

        // Routine absence did not poison the worker or consume its retained image.
        let next = encode(&mut encoder, 2);
        baseline.submit(&next).unwrap();
        let (_, pixels) = baseline.poll_output().unwrap();
        submit(&mut worker, Kind::Present, &next);
        let newest = read(&mut worker, stamp(&next, recovery)).unwrap();
        let shown = match &mut fitted {
            Some(fitted) => fitted.render(&pixels).unwrap(),
            None => &pixels,
        };
        assert_eq!(newest.pixels(), shown.pixels());
        assert_eq!(newest.pixels(), ui.snapshot().unwrap().pixels());
        assert_eq!(read(&mut worker, expected).err(), Some(Absent::NotCurrent));
        worker.stop();
    }
}

#[test]
fn export_stamp_survives_reference_only_recovery_and_tracks_current_cursor() {
    for fit in [false, true] {
        let display = Display::start();
        let mut encoder = encoder();
        let recovery = RecoveryGeneration::from_raw(11);
        let first = relabel(encode(&mut encoder, 0), 0, FrameKind::Idr { recovery });
        let (mut worker, mut ui, mut fitted) = configured(&display, &first, fit);
        submit(&mut worker, Kind::Present, &first);
        let original_stamp = stamp(&first, recovery);
        let original = read(&mut worker, original_stamp).unwrap();
        let original_pixels = original.pixels().to_vec();
        let rgba = [255; 4 * 4 * 4];
        worker.update(&Update::Shape {
            x: 16,
            y: 16,
            width: 4,
            height: 4,
            hotspot_x: 0,
            hotspot_y: 0,
            rgba: &rgba,
        });
        let with_cursor = read(&mut worker, original_stamp).unwrap();
        assert_eq!(with_cursor.stamp(), original_stamp);
        assert_eq!(with_cursor.pixels(), ui.snapshot().unwrap().pixels());
        assert_ne!(with_cursor.pixels(), original.pixels());
        for n in 1..7 {
            let unit = encode(&mut encoder, n);
            submit(&mut worker, Kind::Decode, &unit);
            assert_eq!(
                read(&mut worker, stamp(&unit, recovery)).err(),
                Some(Absent::NotCurrent)
            );
            assert_eq!(
                read(&mut worker, original_stamp).unwrap().pixels(),
                with_cursor.pixels()
            );
        }
        worker.update(&Update::Move { x: 40, y: 40 });
        let moved = read(&mut worker, original_stamp).unwrap();
        assert_eq!(moved.pixels(), ui.snapshot().unwrap().pixels());
        assert_ne!(moved.pixels(), with_cursor.pixels());
        worker.update(&Update::Hidden);
        assert_eq!(
            read(&mut worker, original_stamp).unwrap().pixels(),
            original_pixels
        );
        assert_eq!(original.pixels(), original_pixels);

        // A genuine new IDR starts another admitted reference chain. Its
        // decode-only completion must not relabel the older displayed picture.
        let next_recovery = recovery.next().unwrap();
        let mut replacement_encoder = super::super::encoder();
        let idr = relabel(
            encode(&mut replacement_encoder, 0),
            100,
            FrameKind::Idr {
                recovery: next_recovery,
            },
        );
        let mut baseline = decoder(&idr);
        baseline.submit(&idr).unwrap();
        baseline.poll_reference().unwrap();
        submit(&mut worker, Kind::Decode, &idr);
        assert_eq!(
            read(&mut worker, stamp(&idr, next_recovery)).err(),
            Some(Absent::NotCurrent)
        );
        assert_eq!(
            read(&mut worker, original_stamp).unwrap().pixels(),
            original_pixels
        );

        let predicted = relabel(
            encode(&mut replacement_encoder, 1),
            101,
            FrameKind::Predicted {
                references: idr.frame(),
            },
        );
        baseline.submit(&predicted).unwrap();
        let (_, pixels) = baseline.poll_output().unwrap();
        submit(&mut worker, Kind::Present, &predicted);
        let snapshot = read(&mut worker, stamp(&predicted, next_recovery)).unwrap();
        let shown = match &mut fitted {
            Some(fitted) => fitted.render(&pixels).unwrap(),
            None => &pixels,
        };
        assert_eq!(snapshot.pixels(), shown.pixels());
        assert_eq!(snapshot.pixels(), ui.snapshot().unwrap().pixels());
        assert_eq!(
            read(&mut worker, original_stamp).err(),
            Some(Absent::NotCurrent)
        );
        worker.stop();
    }
}

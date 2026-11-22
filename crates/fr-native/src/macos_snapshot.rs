//! Original presentation identity and bounded storage for explicit Mac exports.
//! Decode-only reference progress never relabels the retained visible picture.
#![forbid(unsafe_code)]

use fr_core::ids::{CodecConfigurationGeneration, RecoveryGeneration};
use fr_media::{
    access_unit::{FrameId, FrameKind},
    worker::{
        Error,
        presentation_snapshot::{self, Absent, Stamp},
    },
};

#[derive(Default)]
pub(crate) struct Retained {
    recovery: Option<RecoveryGeneration>,
    presented: Option<Stamp>,
}

impl Retained {
    /// Called immediately before native decode, after bitstream admission.
    /// A new recovery or attempted presentation cannot export its predecessor
    /// while the native call is pending or after a failed call.
    pub(crate) fn begin(&mut self, kind: FrameKind, present: bool) {
        if matches!(kind, FrameKind::Idr { .. }) {
            self.recovery = None;
            self.presented = None;
        } else if present {
            self.presented = None;
        }
    }

    pub(crate) fn completed(
        &mut self,
        frame: FrameId,
        configuration: CodecConfigurationGeneration,
        kind: FrameKind,
        present: bool,
    ) {
        if let FrameKind::Idr { recovery } = kind {
            self.recovery = Some(recovery);
        }
        if present {
            self.presented = self.recovery.map(|recovery| Stamp {
                frame,
                configuration,
                recovery,
            });
        }
    }

    pub(crate) fn failed(&mut self) {
        self.recovery = None;
        self.presented = None;
    }

    pub(crate) fn check(&self, expected: Stamp) -> Result<(), Absent> {
        match self.presented {
            None => Err(Absent::NoFrame),
            Some(current) if current != expected => Err(Absent::NotCurrent),
            Some(_) => Ok(()),
        }
    }
}

/// The actual presented viewport includes Retina scaling and letterboxing.
/// Its dimensions are independent of the decoded source, but remain within
/// the protocol's absolute dimension bounds and the 32 MiB export ceiling.
pub(crate) fn pixel_bytes(width: u32, height: u32) -> Result<usize, Absent> {
    presentation_snapshot::pixel_bytes(width, height).map_err(|_| Absent::TooLarge)
}

/// One final response allocation. Native writes only the already-sized pixel
/// suffix; Snapshot then adopts this allocation without another raster copy.
pub(crate) fn prepare(
    stamp: Stamp,
    width: u32,
    height: u32,
    length: usize,
    maximum_body_bytes: usize,
) -> Result<Vec<u8>, Error> {
    if presentation_snapshot::pixel_bytes(width, height)? != length {
        return Err(Error::Malformed);
    }
    let total = presentation_snapshot::HEADER_BYTES
        .checked_add(length)
        .filter(|bytes| {
            *bytes <= presentation_snapshot::MAX_BODY_BYTES && *bytes <= maximum_body_bytes
        })
        .ok_or(Error::ResourceLimit)?;
    let mut body = Vec::new();
    body.try_reserve_exact(total)
        .map_err(|_| Error::Allocation)?;
    if body.capacity() > presentation_snapshot::MAX_BODY_BYTES
        || body.capacity() > maximum_body_bytes
    {
        return Err(Error::ResourceLimit);
    }
    body.extend_from_slice(&stamp.encode());
    body.extend_from_slice(&width.to_be_bytes());
    body.extend_from_slice(&height.to_be_bytes());
    body.resize(total, 0);
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fr_media::worker::presentation_snapshot::Snapshot;

    fn stamp(frame: u64, recovery: u64) -> Stamp {
        Stamp {
            frame: FrameId::from_raw(frame),
            configuration: CodecConfigurationGeneration::from_raw(3),
            recovery: RecoveryGeneration::from_raw(recovery),
        }
    }

    fn submit(state: &mut Retained, value: Stamp, kind: FrameKind, present: bool) {
        state.begin(kind, present);
        state.completed(value.frame, value.configuration, kind, present);
    }

    #[test]
    fn reference_only_decode_keeps_the_original_presented_identity() {
        let mut state = Retained::default();
        let original = stamp(7, 2);
        assert_eq!(state.check(original), Err(Absent::NoFrame));
        submit(
            &mut state,
            original,
            FrameKind::Idr {
                recovery: original.recovery,
            },
            true,
        );
        submit(
            &mut state,
            stamp(8, 2),
            FrameKind::Predicted {
                references: original.frame,
            },
            false,
        );
        assert_eq!(state.check(original), Ok(()));
        assert_eq!(state.check(stamp(8, 2)), Err(Absent::NotCurrent));
        let next = stamp(9, 2);
        submit(
            &mut state,
            next,
            FrameKind::Predicted {
                references: FrameId::from_raw(8),
            },
            true,
        );
        assert_eq!(state.check(original), Err(Absent::NotCurrent));
        assert_eq!(state.check(next), Ok(()));
    }

    #[test]
    fn new_recovery_and_failed_presentation_cannot_export_old_pixels() {
        let mut state = Retained::default();
        let original = stamp(7, 2);
        let idr = FrameKind::Idr {
            recovery: original.recovery,
        };
        submit(&mut state, original, idr, true);
        state.begin(
            FrameKind::Predicted {
                references: original.frame,
            },
            true,
        );
        assert_eq!(state.check(original), Err(Absent::NoFrame));
        state.failed();
        assert_eq!(state.check(original), Err(Absent::NoFrame));

        submit(&mut state, original, idr, true);
        let recovered = stamp(8, 4);
        submit(
            &mut state,
            recovered,
            FrameKind::Idr {
                recovery: recovered.recovery,
            },
            false,
        );
        assert_eq!(state.check(original), Err(Absent::NoFrame));
        assert_eq!(state.check(recovered), Err(Absent::NoFrame));
        let current = stamp(9, 4);
        submit(
            &mut state,
            current,
            FrameKind::Predicted {
                references: recovered.frame,
            },
            true,
        );
        assert_eq!(state.check(current), Ok(()));
        assert_eq!(state.check(stamp(9, 2)), Err(Absent::NotCurrent));
        let wrong_config = Stamp {
            configuration: CodecConfigurationGeneration::from_raw(4),
            ..current
        };
        assert_eq!(state.check(wrong_config), Err(Absent::NotCurrent));
    }

    #[test]
    fn export_bounds_admit_real_retina_viewports_and_refuse_oversize_before_allocation() {
        assert_eq!(pixel_bytes(128, 96), Ok(49_152));
        assert_eq!(pixel_bytes(64, 48), Ok(12_288));
        assert_eq!(pixel_bytes(256, 192), Ok(196_608));
        for (width, height) in [(0, 1), (1, 0), (u32::MAX, 1), (4096, 2160)] {
            assert_eq!(pixel_bytes(width, height), Err(Absent::TooLarge));
        }
        assert_eq!(pixel_bytes(3840, 2160), Ok(33_177_600));
    }

    #[test]
    fn response_storage_keeps_exact_stamp_and_owned_tight_rows() {
        let expected = stamp(7, 2);
        let maximum = presentation_snapshot::HEADER_BYTES + 8;
        let mut body = prepare(expected, 2, 1, 8, maximum).unwrap();
        let allocation = body.as_ptr();
        let pixels = [1, 2, 3, 255, 4, 5, 6, 255];
        body[presentation_snapshot::HEADER_BYTES..].copy_from_slice(&pixels);
        let snapshot = Snapshot::decode(body).unwrap();
        assert_eq!(snapshot.stamp(), expected);
        assert_eq!((snapshot.width(), snapshot.height()), (2, 1));
        assert_eq!(snapshot.pixels(), pixels);
        assert_eq!(
            snapshot.pixels().as_ptr().addr(),
            allocation.addr() + presentation_snapshot::HEADER_BYTES
        );
        assert_eq!(prepare(expected, 2, 1, 4, maximum), Err(Error::Malformed));
        assert_eq!(
            prepare(expected, 8193, 1, 8193 * 4, usize::MAX),
            Err(Error::ResourceLimit)
        );
        assert_eq!(
            prepare(expected, 2, 1, 8, maximum - 1),
            Err(Error::ResourceLimit)
        );
    }
}

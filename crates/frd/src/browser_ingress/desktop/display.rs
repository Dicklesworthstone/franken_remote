//! One catalog and one full-display choice on the original control session.
//! The catalog is bounded metadata, never a replacement worker or authority.
use super::{Catalog, ControlBinding, Delivery, Direction, Select, failed};
use fr_core::limits::ProtocolLimits;
use fr_wire::display;
use std::io;

// Fixed storage for at most eight displays; no allocation from peer counts.
#[allow(clippy::large_enum_variant)]
pub(super) enum Choice {
    Legacy,
    Discovering,
    Offered(Catalog),
    Selected(Select),
    Consumed,
}
impl Choice {
    pub(super) const fn new(selected: bool) -> Self {
        if selected {
            Self::Discovering
        } else {
            Self::Legacy
        }
    }
    /// The exact discovery snapshot has entered the bounded control queue.
    /// Empty catalogs remain explicit; no fallback display is invented.
    pub(super) fn offered(&mut self, catalog: Catalog) -> io::Result<()> {
        if !matches!(self, Self::Discovering) {
            return Err(failed());
        }
        *self = Self::Offered(catalog);
        Ok(())
    }
    pub(super) fn receive(
        &mut self,
        bytes: &[u8],
        binding: ControlBinding,
        limits: &ProtocolLimits,
    ) -> io::Result<()> {
        let Self::Offered(catalog) = self else {
            return Err(failed());
        };
        let display::Message::Select(request) = display::decode(
            bytes,
            binding,
            limits,
            Direction::ViewerToHost,
            Delivery::Reliable,
        )
        .map_err(|_| failed())?
        else {
            return Err(failed());
        };
        catalog.selected(request).map_err(|_| failed())?;
        *self = Self::Selected(request);
        Ok(())
    }
    /// Consume once without reopening the exchange. Late Select records remain
    /// terminal even after native configuration has taken the original choice.
    pub(super) fn take(&mut self) -> io::Result<Option<Select>> {
        match self {
            Self::Offered(_) => Ok(None),
            Self::Selected(request) => {
                let request = *request;
                *self = Self::Consumed;
                Ok(Some(request))
            }
            _ => Err(failed()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fr_core::ids::{DisplayGeometryGeneration, HostBootId, OsSessionId, RemoteSessionId};

    fn binding() -> ControlBinding {
        ControlBinding {
            id: 1,
            host_boot: HostBootId::from_raw(17),
            os_session: OsSessionId::from_raw(18),
            remote_session: RemoteSessionId::from_raw(19),
        }
    }
    fn catalog(revision: u64) -> Catalog {
        let display = display::Display {
            handle: 41,
            geometry: DisplayGeometryGeneration::INITIAL,
            x: 0,
            y: 0,
            pixel_width: 1920,
            pixel_height: 1080,
            logical_width: 1920,
            logical_height: 1080,
            scale_numerator: 1,
            scale_denominator: 1,
            rotation: 0,
        };
        Catalog::new(
            revision,
            &[
                display,
                display::Display {
                    handle: 42,
                    x: 1920,
                    pixel_width: 1280,
                    pixel_height: 720,
                    logical_width: 1280,
                    logical_height: 720,
                    ..display
                },
            ],
            &ProtocolLimits::ABSOLUTE,
        )
        .unwrap()
    }
    fn record(request: Select, binding: ControlBinding) -> Vec<u8> {
        let mut bytes = vec![0; display::SELECT_BYTES];
        let length = display::encode(
            &display::Message::Select(request),
            binding,
            &ProtocolLimits::ABSOLUTE,
            &mut bytes,
            Direction::ViewerToHost,
            Delivery::Reliable,
        )
        .unwrap();
        bytes.truncate(length);
        bytes
    }

    #[test]
    fn exactly_one_original_full_display_choice_precedes_configuration() {
        let catalog = catalog(7);
        let request = catalog.selection(42).unwrap();
        let bytes = record(request, binding());
        let mut choice = Choice::new(true);
        assert!(
            choice
                .receive(&bytes, binding(), &ProtocolLimits::ABSOLUTE)
                .is_err()
        );
        choice.offered(catalog).unwrap();
        assert_eq!(choice.take().unwrap(), None);
        choice
            .receive(&bytes, binding(), &ProtocolLimits::ABSOLUTE)
            .unwrap();
        assert!(
            choice
                .receive(&bytes, binding(), &ProtocolLimits::ABSOLUTE)
                .is_err()
        );
        assert_eq!(choice.take().unwrap(), Some(request));
        assert!(choice.take().is_err());
        assert!(choice.offered(catalog).is_err());
        assert!(
            choice
                .receive(&bytes, binding(), &ProtocolLimits::ABSOLUTE)
                .is_err()
        );
    }

    #[test]
    fn stale_cropped_foreign_and_unnegotiated_choices_never_enter_configuration() {
        let original = catalog(7);
        let selected = original.selection(42).unwrap();
        let cases = [
            Select {
                revision: 8,
                ..selected
            },
            Select {
                handle: 43,
                ..selected
            },
            Select {
                geometry: DisplayGeometryGeneration::from_raw(2),
                ..selected
            },
            Select {
                width: 640,
                ..selected
            },
            Select { x: 1, ..selected },
        ];
        for request in cases {
            let mut choice = Choice::new(true);
            choice.offered(original).unwrap();
            assert!(
                choice
                    .receive(
                        &record(request, binding()),
                        binding(),
                        &ProtocolLimits::ABSOLUTE,
                    )
                    .is_err()
            );
            assert_eq!(choice.take().unwrap(), None);
        }
        let mut choice = Choice::new(true);
        choice.offered(original).unwrap();
        let foreign = ControlBinding {
            remote_session: RemoteSessionId::from_raw(20),
            ..binding()
        };
        assert!(
            choice
                .receive(
                    &record(selected, foreign),
                    binding(),
                    &ProtocolLimits::ABSOLUTE,
                )
                .is_err()
        );
        let mut legacy = Choice::new(false);
        assert!(legacy.offered(original).is_err());
        assert!(
            legacy
                .receive(
                    &record(selected, binding()),
                    binding(),
                    &ProtocolLimits::ABSOLUTE,
                )
                .is_err()
        );
    }

    #[test]
    fn replacement_catalog_and_empty_disclosure_never_select_a_default() {
        let mut choice = Choice::new(true);
        choice.offered(catalog(7)).unwrap();
        assert!(choice.offered(catalog(8)).is_err());
        let mut empty = Choice::new(true);
        empty
            .offered(Catalog::new(7, &[], &ProtocolLimits::ABSOLUTE).unwrap())
            .unwrap();
        assert_eq!(empty.take().unwrap(), None);
        assert!(
            empty
                .receive(
                    &record(catalog(7).selection(41).unwrap(), binding()),
                    binding(),
                    &ProtocolLimits::ABSOLUTE,
                )
                .is_err()
        );
    }
}

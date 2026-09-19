//! Unique and physically shared IDRs use the SAME host acknowledgement machine.
use super::{
    CaptureUpdate, Configuration, Error, Host, HostPhase, ObservationControl, QuicRecords, Setup,
};
use crate::media::SharedCaptureUpdate;
use fr_core::ids::CodecConfigurationGeneration;

pub(super) enum Bootstrap {
    Unique(CaptureUpdate),
    Shared(SharedCaptureUpdate),
}
pub(super) struct Captured<'a> {
    pub(super) frame: u64,
    pub(super) configuration: CodecConfigurationGeneration,
    pub(super) capture_micros: u64,
    pub(super) idr: bool,
    pub(super) bytes: &'a [u8],
}
impl Bootstrap {
    pub(super) fn view(&self) -> Result<Captured<'_>, Error> {
        match self {
            Self::Unique(update) => {
                let unit = update.encoded().ok_or(Error::WrongState)?;
                Ok(Captured {
                    frame: unit.frame().as_raw(),
                    configuration: unit.config_generation(),
                    capture_micros: unit.capture_micros(),
                    idr: unit.is_idr(),
                    bytes: unit.bytes(),
                })
            }
            Self::Shared(update) => {
                let unit = update.encoded().ok_or(Error::WrongState)?;
                Ok(Captured {
                    frame: unit.frame().as_raw(),
                    configuration: unit.configuration(),
                    capture_micros: unit.capture_micros(),
                    idr: unit.kind().is_idr(),
                    bytes: unit.bytes(),
                })
            }
        }
    }
}
impl Host {
    /// Start one independently authorized viewer from an actual shared native
    /// IDR. Cloning the input retains its physical reservation, not another encode
    /// or a new timestamp. The original host parser, selected-view checks, actual
    /// HEVC parameter validation and two acknowledgement gates all still apply.
    ///
    /// Each waiting viewer has its own immutable deadline, connection and consent.
    /// Its failure/drop releases only its alias, never another viewer's authority
    /// or the source. The share-session must bound the number of startup owners
    /// and charge each pending viewer for the retention it can force. This is not
    /// admission of a new viewer, an input grant, or late-join IDR rate admission.
    pub fn new_shared(
        control: ObservationControl,
        transport: &QuicRecords,
        setup: Setup,
        cfg: Configuration,
        update: SharedCaptureUpdate,
    ) -> Result<Self, Error> {
        let charge = update
            .encoded()
            .ok_or(Error::WrongState)?
            .allocation_charge();
        if !u64::try_from(charge).is_ok_and(|n| n <= setup.limits.per_viewer_compressed_bytes()) {
            return Err(Error::Wire(fr_wire::WireError::ResourceLimit));
        }
        Self::from_capture(control, transport, setup, cfg, Bootstrap::Shared(update))
    }
    /// Transfer this viewer's alias only after its exact configured reply. The
    /// receiver-independent allocation and its capture-anchored deadline survive
    /// the handoff to the existing shared egress path. Calling the wrong unique/
    /// shared transfer method refuses WITHOUT consuming the ready output.
    pub fn take_shared_recovery(&mut self) -> Result<Option<SharedCaptureUpdate>, Error> {
        self.tick()?;
        if self.phase != HostPhase::RecoveryReady {
            return Ok(None);
        }
        if !matches!(self.update, Some(Bootstrap::Shared(_))) {
            return Err(Error::WrongState);
        }
        let Some(Bootstrap::Shared(update)) = self.update.take() else {
            return Err(Error::WrongState);
        };
        self.phase = HostPhase::AwaitingDecode;
        Ok(Some(update))
    }
}

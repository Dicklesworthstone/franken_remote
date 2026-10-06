//! Private session-UI IPC over inherited socketpairs. Observation indication
//! and one-use approval are different locally selected child roles. Neither
//! carries input operations, paths, text, or network authority. A UI decision
//! is not a grant: the parent must recheck its ORIGINAL pending capability.
use core::fmt;

pub const FRAME_BYTES: usize = 32;
const MAGIC: [u8; 4] = *b"FRIV";
const VERSION: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    Open = 1,
    Check = 2,
    Stop = 3,
    ApproveView = 4,
    ApproveControl = 5,
    ApprovalCheck = 6,
    Ready = 129,
    Stopped = 130,
    Refused = 131,
    Allowed = 132,
    Denied = 133,
}
impl Kind {
    pub const fn is_request(self) -> bool {
        matches!(self, Self::Open | Self::Check | Self::Stop | Self::ApproveView | Self::ApproveControl | Self::ApprovalCheck)
    }
    const fn parse(value: u8) -> Result<Self, Error> {
        match value {
            1 => Ok(Self::Open),
            2 => Ok(Self::Check),
            3 => Ok(Self::Stop),
            4 => Ok(Self::ApproveView),
            5 => Ok(Self::ApproveControl),
            6 => Ok(Self::ApprovalCheck),
            129 => Ok(Self::Ready),
            130 => Ok(Self::Stopped),
            131 => Ok(Self::Refused),
            132 => Ok(Self::Allowed),
            133 => Ok(Self::Denied),
            _ => Err(Error),
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Error;

/// Fixed-size correlation, not an authorization token. Neither matching an
/// epoch nor a Ready reply establishes permission to capture or inject input.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Frame {
    pub kind: Kind,
    pub sequence: u64,
    pub epoch: u128,
}
impl fmt::Debug for Frame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IndicatorFrame")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}
impl Frame {
    pub fn encode(self) -> Result<[u8; FRAME_BYTES], Error> {
        if self.sequence == 0 || self.epoch == 0 {
            return Err(Error);
        }
        let mut bytes = [0; FRAME_BYTES];
        bytes[..4].copy_from_slice(&MAGIC);
        bytes[4] = VERSION;
        bytes[5] = self.kind as u8;
        bytes[8..16].copy_from_slice(&self.sequence.to_be_bytes());
        bytes[16..].copy_from_slice(&self.epoch.to_be_bytes());
        Ok(bytes)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() != FRAME_BYTES
            || bytes[..4] != MAGIC
            || bytes[4] != VERSION
            || bytes[6..8] != [0, 0]
        {
            return Err(Error);
        }
        let frame = Self {
            kind: Kind::parse(bytes[5])?,
            sequence: u64::from_be_bytes(bytes[8..16].try_into().map_err(|_| Error)?),
            epoch: u128::from_be_bytes(bytes[16..].try_into().map_err(|_| Error)?),
        };
        if frame.sequence == 0 || frame.epoch == 0 {
            return Err(Error);
        }
        Ok(frame)
    }
    pub const fn reply(self, kind: Kind) -> Self {
        Self { kind, ..self }
    }
    /// Match every correlation field and direction before interpreting status.
    pub fn response_to(self, request: Self) -> Result<Kind, Error> {
        if self.kind.is_request()
            || !request.kind.is_request()
            || self.sequence != request.sequence
            || self.epoch != request.epoch
        {
            return Err(Error);
        }
        match (request.kind, self.kind) {
            (Kind::Open | Kind::Check, Kind::Ready | Kind::Refused)
            | (Kind::Stop, Kind::Stopped | Kind::Refused)
            | (Kind::ApproveView | Kind::ApproveControl | Kind::ApprovalCheck,
               Kind::Ready | Kind::Refused | Kind::Allowed | Kind::Denied) => Ok(self.kind),
            _ => Err(Error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> Frame {
        Frame { kind: Kind::Open, sequence: 1, epoch: 29 }
    }
    #[test]
    fn all_records_round_trip_with_constant_space() {
        for kind in [Kind::Open, Kind::Check, Kind::Stop, Kind::Ready, Kind::Stopped, Kind::Refused,
            Kind::ApproveView, Kind::ApproveControl, Kind::ApprovalCheck, Kind::Allowed, Kind::Denied] {
            for sequence in [1, 2, u64::MAX] {
                for epoch in [1, 29, u128::MAX] {
                    let frame = Frame { kind, sequence, epoch };
                    assert_eq!(Frame::decode(&frame.encode().unwrap()), Ok(frame));
                }
            }
        }
    }
    #[test]
    fn malformed_truncated_extended_or_cross_protocol_frames_refuse() {
        let encoded = request().encode().unwrap();
        for len in 0..FRAME_BYTES {
            assert_eq!(Frame::decode(&encoded[..len]), Err(Error));
        }
        let mut extended = encoded.to_vec();
        extended.push(0);
        assert_eq!(Frame::decode(&extended), Err(Error));
        for index in [0, 1, 2, 3, 4, 6, 7] {
            let mut changed = encoded;
            changed[index] ^= 1;
            assert_eq!(Frame::decode(&changed), Err(Error));
        }
        for value in 0..=u8::MAX {
            if Kind::parse(value).is_err() {
                let mut changed = encoded;
                changed[5] = value;
                assert_eq!(Frame::decode(&changed), Err(Error));
            }
        }
    }
    #[test]
    fn zero_identity_and_sequence_refuse_in_both_directions() {
        for (sequence, epoch, range) in [(0, 29, 8..16), (1, 0, 16..32)] {
            assert_eq!(Frame { sequence, epoch, ..request() }.encode(), Err(Error));
            let mut bytes = request().encode().unwrap();
            bytes[range].fill(0);
            assert_eq!(Frame::decode(&bytes), Err(Error));
        }
    }
    #[test]
    fn replies_cannot_cross_epochs_sequences_directions_or_stages() {
        let open = request();
        let ready = open.reply(Kind::Ready);
        assert_eq!(ready.response_to(open), Ok(Kind::Ready));
        assert_eq!(open.reply(Kind::Refused).response_to(open), Ok(Kind::Refused));
        for foreign in [
            Frame { sequence: 2, ..ready },
            Frame { epoch: 30, ..ready },
            open,
            open.reply(Kind::Stopped),
        ] {
            assert_eq!(foreign.response_to(open), Err(Error));
        }
        let stop = open.reply(Kind::Stop);
        assert_eq!(ready.response_to(stop), Err(Error));
        assert_eq!(stop.reply(Kind::Stopped).response_to(stop), Ok(Kind::Stopped));
        assert_eq!(ready.response_to(ready), Err(Error));
        assert!(!format!("{ready:?}").contains("29"));
    }
}

#[cfg(test)]
mod approval_tests {
    use super::*;

    #[test]
    fn ordinary_indicator_commands_can_never_accept_positive_consent() {
        for kind in [Kind::Open, Kind::Check, Kind::Stop] {
            let request = Frame { kind, sequence: 17, epoch: 23 };
            for decision in [Kind::Allowed, Kind::Denied] {
                assert_eq!(request.reply(decision).response_to(request), Err(Error));
            }
        }
    }
    #[test]
    fn approval_has_distinct_polling_and_exact_one_request_correlation() {
        for kind in [Kind::ApproveView, Kind::ApproveControl, Kind::ApprovalCheck] {
            let request = Frame { kind, sequence: 17, epoch: 23 };
            for decision in [Kind::Ready, Kind::Refused, Kind::Allowed, Kind::Denied] {
                let reply = request.reply(decision);
                assert_eq!(reply.response_to(request), Ok(decision));
                assert_eq!(Frame { epoch: 24, ..reply }.response_to(request), Err(Error));
                assert_eq!(Frame { sequence: 18, ..reply }.response_to(request), Err(Error));
            }
        }
    }
}

//! Fixed private permission-only request. Original deadlines are transported,
//! never recreated by a delayed process launch or native callback.
#![forbid(unsafe_code)]

pub(crate) const BYTES: usize = 24;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Reply {
    Granted = 1,
    Denied = 2,
    Configuration = 3,
    Expired = 4,
    Unavailable = 5,
}
impl Reply {
    pub(crate) fn decode(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(Self::Granted),
            2 => Some(Self::Denied),
            3 => Some(Self::Configuration),
            4 => Some(Self::Expired),
            5 => Some(Self::Unavailable),
            _ => None,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Request {
    pub launch_until_ns: u64,
    pub wait_until_ns: u64,
}
impl Request {
    pub(crate) fn new(launch_until_ns: u64, wait_until_ns: u64) -> Option<Self> {
        (launch_until_ns != 0 && wait_until_ns.checked_sub(launch_until_ns) == Some(59_750_000_000))
            .then_some(Self {
                launch_until_ns,
                wait_until_ns,
            })
    }
    pub(crate) fn may_launch(self, now: u64) -> bool {
        now < self.launch_until_ns
    }
    pub(crate) fn encode(self) -> [u8; BYTES] {
        let mut bytes = [0; BYTES];
        bytes[..8].copy_from_slice(b"FRMICPR1");
        bytes[8..16].copy_from_slice(&self.launch_until_ns.to_be_bytes());
        bytes[16..].copy_from_slice(&self.wait_until_ns.to_be_bytes());
        bytes
    }
    pub(crate) fn decode(bytes: [u8; BYTES]) -> Option<Self> {
        if &bytes[..8] != b"FRMICPR1" {
            return None;
        }
        Self::new(
            u64::from_be_bytes(bytes[8..16].try_into().ok()?),
            u64::from_be_bytes(bytes[16..].try_into().ok()?),
        )
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn delayed_spawn_cannot_mint_another_prompt_lifetime() {
        let original = Request::new(1_250_000_000, 61_000_000_000).unwrap();
        let received = Request::decode(original.encode()).unwrap();
        assert_eq!(received, original);
        assert!(received.may_launch(1_249_999_999));
        assert!(!received.may_launch(1_250_000_000));
        assert!(!received.may_launch(61_000_000_000));
        assert_eq!(received.wait_until_ns, 61_000_000_000);
    }
    #[test]
    fn unknown_versions_inconsistent_caps_and_overflow_are_refused() {
        let original = Request::new(1_250_000_000, 61_000_000_000).unwrap();
        let mut bytes = original.encode();
        bytes[7] = b'2';
        assert_eq!(Request::decode(bytes), None);
        for (launch, wait) in [
            (0, 59_750_000_000),
            (1, 1),
            (u64::MAX, 0),
            (1_250_000_000, 61_000_000_001),
        ] {
            assert_eq!(Request::new(launch, wait), None);
        }
    }
    #[test]
    fn native_denial_and_missing_signed_metadata_are_distinct_content_free_results() {
        assert_eq!(Reply::decode(1), Some(Reply::Granted));
        assert_eq!(Reply::decode(2), Some(Reply::Denied));
        assert_eq!(Reply::decode(3), Some(Reply::Configuration));
        assert_eq!(Reply::decode(4), Some(Reply::Expired));
        assert_eq!(Reply::decode(5), Some(Reply::Unavailable));
        assert_eq!(Reply::decode(0), None);
        assert_eq!(Reply::decode(255), None);
    }
}

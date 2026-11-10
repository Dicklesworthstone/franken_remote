//! Private inherited-pipe protocol. No network endpoint, credentials or permission grants.
use super::Error;

pub const SELECTION_BYTES: usize = 224;
pub const QUERY_BYTES: usize = 32;
pub const REPLY_BYTES: usize = 40;

/// The native evidence source. A macOS login session has no logind name,
/// seat or X11 display; those fields must never stand in for its OS identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Linux,
    Macos,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub platform: Platform,
    pub session: String,
    pub uid: u32,
    pub seat: String,
    pub display: String,
}
impl Selection {
    /// Select this process user's actual macOS graphical login session. The
    /// child binds its Security.framework session identity from native reads,
    /// not from a caller-provided session name or reusable authority token.
    pub fn macos(uid: u32) -> Self {
        Self {
            platform: Platform::Macos,
            session: String::new(),
            uid,
            seat: String::new(),
            display: String::new(),
        }
    }

    pub fn validate(&self) -> Result<(), Error> {
        if self.platform == Platform::Macos {
            return if self.session.is_empty() && self.seat.is_empty() && self.display.is_empty() {
                Ok(())
            } else {
                Err(Error::Selection)
            };
        }
        let name = |s: &str| {
            !s.is_empty()
                && s.len() <= 64
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        };
        if !name(&self.session)
            || !name(&self.seat)
            || self.display.len() > 64
            || !self.display.starts_with(':')
            || self.display.len() < 2
            || !self.display[1..]
                .bytes()
                .all(|b| b.is_ascii_digit() || b == b'.')
        {
            return Err(Error::Selection);
        }
        Ok(())
    }
    pub fn encode(&self, epoch: u128) -> Result<[u8; SELECTION_BYTES], Error> {
        self.validate()?;
        if epoch == 0 {
            return Err(Error::Protocol);
        }
        let mut out = [0; SELECTION_BYTES];
        out[..4].copy_from_slice(b"FRSM");
        out[4] = 1; // Protocol version.
        out[5] = 1; // Read-only session monitor, not input/consent/media.
        // Zero preserves the original Linux wire image. Unknown platforms
        // refuse rather than being interpreted as an X11/logind selection.
        out[6] = u8::from(self.platform == Platform::Macos);
        out[8..24].copy_from_slice(&epoch.to_be_bytes());
        out[24..28].copy_from_slice(&self.uid.to_be_bytes());
        for (i, value) in [&self.session, &self.seat, &self.display]
            .into_iter()
            .enumerate()
        {
            out[28 + i] = u8::try_from(value.len()).map_err(|_| Error::Selection)?;
            out[32 + i * 64..32 + i * 64 + value.len()].copy_from_slice(value.as_bytes());
        }
        Ok(out)
    }
    pub fn decode(bytes: &[u8; SELECTION_BYTES]) -> Result<(Self, u128), Error> {
        if &bytes[..4] != b"FRSM" || bytes[4..6] != [1, 1] || bytes[7] != 0 || bytes[31] != 0 {
            return Err(Error::Protocol);
        }
        let platform = match bytes[6] {
            0 => Platform::Linux,
            1 => Platform::Macos,
            _ => return Err(Error::Protocol),
        };
        let epoch = u128::from_be_bytes(bytes[8..24].try_into().map_err(|_| Error::Protocol)?);
        let uid = u32::from_be_bytes(bytes[24..28].try_into().map_err(|_| Error::Protocol)?);
        let mut names = Vec::with_capacity(3);
        for i in 0..3 {
            let n = usize::from(bytes[28 + i]);
            if n > 64
                || bytes[32 + i * 64 + n..32 + (i + 1) * 64]
                    .iter()
                    .any(|v| *v != 0)
            {
                return Err(Error::Protocol);
            }
            names.push(
                std::str::from_utf8(&bytes[32 + i * 64..32 + i * 64 + n])
                    .map_err(|_| Error::Protocol)?
                    .to_owned(),
            );
        }
        let selection = Self {
            platform,
            session: names[0].clone(),
            uid,
            seat: names[1].clone(),
            display: names[2].clone(),
        };
        selection.validate()?;
        if epoch == 0 {
            return Err(Error::Protocol);
        }
        Ok((selection, epoch))
    }
}

/// Native evidence only. Active never means that capture, consent or input was granted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum State {
    Opening = 0,
    Active = 1,
    Locked = 2,
    Inactive = 3,
    Suspending = 4,
    SessionUnavailable = 5,
    IdentityChanged = 6,
    ServiceUnavailable = 7,
    UnsupportedSession = 8,
    EvidenceExpired = 9,
    Failed = 10,
}
impl State {
    fn decode(value: u16) -> Result<Self, Error> {
        Ok(match value {
            0 => Self::Opening,
            1 => Self::Active,
            2 => Self::Locked,
            3 => Self::Inactive,
            4 => Self::Suspending,
            5 => Self::SessionUnavailable,
            6 => Self::IdentityChanged,
            7 => Self::ServiceUnavailable,
            8 => Self::UnsupportedSession,
            9 => Self::EvidenceExpired,
            10 => Self::Failed,
            _ => return Err(Error::Protocol),
        })
    }
}
pub fn query(epoch: u128, sequence: u64) -> Result<[u8; QUERY_BYTES], Error> {
    if epoch == 0 || sequence == 0 {
        return Err(Error::Protocol);
    }
    let mut out = [0; QUERY_BYTES];
    out[..4].copy_from_slice(b"FRMQ");
    out[4..20].copy_from_slice(&epoch.to_be_bytes());
    out[20..28].copy_from_slice(&sequence.to_be_bytes());
    Ok(out)
}
pub fn check_query(bytes: &[u8; QUERY_BYTES], epoch: u128, sequence: u64) -> Result<(), Error> {
    if *bytes != query(epoch, sequence)? {
        return Err(Error::Protocol);
    }
    Ok(())
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reply {
    pub state: State,
    /// Original native-read deadline on the shared suspend-aware kernel clock
    /// (`CLOCK_BOOTTIME` on Linux, `CLOCK_MONOTONIC` on macOS), never receipt time.
    pub until_ns: u64,
}
impl Reply {
    pub fn encode(self, epoch: u128, sequence: u64) -> Result<[u8; REPLY_BYTES], Error> {
        let q = query(epoch, sequence)?;
        if (self.state == State::Active) != (self.until_ns != 0) {
            return Err(Error::Protocol);
        }
        let mut out = [0; REPLY_BYTES];
        out[..28].copy_from_slice(&q[..28]);
        out[..4].copy_from_slice(b"FRMR");
        out[28..36].copy_from_slice(&self.until_ns.to_be_bytes());
        out[36..38].copy_from_slice(&(self.state as u16).to_be_bytes());
        Ok(out)
    }
    pub fn decode(bytes: &[u8; REPLY_BYTES], epoch: u128, sequence: u64) -> Result<Self, Error> {
        let q = query(epoch, sequence)?;
        if &bytes[..4] != b"FRMR" || bytes[4..28] != q[4..28] || bytes[38..] != [0, 0] {
            return Err(Error::Protocol);
        }
        let reply = Self {
            state: State::decode(u16::from_be_bytes(
                bytes[36..38].try_into().map_err(|_| Error::Protocol)?,
            ))?,
            until_ns: u64::from_be_bytes(bytes[28..36].try_into().map_err(|_| Error::Protocol)?),
        };
        if (reply.state == State::Active) != (reply.until_ns != 0) {
            return Err(Error::Protocol);
        }
        Ok(reply)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_selection_has_an_explicit_platform_and_no_fabricated_linux_identity() {
        let selected = Selection::macos(501);
        let bytes = selected.encode(17).unwrap();
        assert_eq!(&bytes[..8], b"FRSM\x01\x01\x01\x00");
        assert_eq!(&bytes[24..28], &501_u32.to_be_bytes());
        assert!(bytes[28..].iter().all(|byte| *byte == 0));
        assert_eq!(Selection::decode(&bytes), Ok((selected, 17)));
    }

    #[test]
    fn a_platform_tag_cannot_reinterpret_another_platforms_selection() {
        let linux = Selection {
            platform: Platform::Linux,
            session: "c2".into(),
            uid: 501,
            seat: "seat0".into(),
            display: ":0".into(),
        };
        let mut bytes = linux.encode(17).unwrap();
        bytes[6] = 1;
        assert_eq!(Selection::decode(&bytes), Err(Error::Selection));
        let mut bytes = Selection::macos(501).encode(17).unwrap();
        bytes[6] = 0;
        assert_eq!(Selection::decode(&bytes), Err(Error::Selection));
        for tag in [2, 127, 255] {
            bytes[6] = tag;
            assert_eq!(Selection::decode(&bytes), Err(Error::Protocol));
        }
    }

    #[test]
    fn macos_selection_refuses_session_seat_or_display_aliases_and_hidden_padding() {
        for field in 0..3 {
            let mut selected = Selection::macos(501);
            match field {
                0 => selected.session = "console".into(),
                1 => selected.seat = "aqua".into(),
                _ => selected.display = ":0".into(),
            }
            assert_eq!(selected.encode(17), Err(Error::Selection));
        }
        let mut bytes = Selection::macos(501).encode(17).unwrap();
        bytes[223] = 1;
        assert_eq!(Selection::decode(&bytes), Err(Error::Protocol));
    }

    #[test]
    fn wrong_platform_refuses_before_starting_any_native_process() {
        let mut selection = Selection::macos(501);
        if cfg!(target_os = "macos") {
            selection.platform = Platform::Linux;
            selection.session = "c2".into();
            selection.seat = "seat0".into();
            selection.display = ":0".into();
        }
        assert!(matches!(
            super::super::Monitor::start(super::super::Configuration {
                image: "/unprovisioned/fr-session-monitor".into(),
                selection,
            }),
            Err(Error::Selection)
        ));
    }
}

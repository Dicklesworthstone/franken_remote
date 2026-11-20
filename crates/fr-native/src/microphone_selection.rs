//! Local microphone selections crossing the private worker boundary. These
//! values never come from the remote host, and never select a default device.
#![forbid(unsafe_code)]

use std::path::{Component, PathBuf};

#[derive(Clone, PartialEq, Eq)]
pub enum Selection {
    Pulse { server: PathBuf, source: String },
    Mac { uid: String },
}

impl core::fmt::Debug for Selection {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Pulse { .. } => "MicrophoneSelection(Pulse)",
            Self::Mac { .. } => "MicrophoneSelection(CoreAudio)",
        })
    }
}

impl Selection {
    pub fn valid(&self) -> bool {
        match self {
            Self::Pulse { server, source } => {
                server.is_absolute()
                    && !server
                        .components()
                        .any(|part| matches!(part, Component::ParentDir))
                    && server.to_str().is_some_and(|server| {
                        (2..=100).contains(&server.len())
                            && server.bytes().all(|b| b > b' ' && b != 127 && b != b':')
                    })
                    && !source.is_empty()
                    && source.len() <= 255
                    && source
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_-.".contains(&b))
            }
            Self::Mac { uid } => Self::valid_mac_uid(uid),
        }
    }

    pub fn valid_mac_uid(uid: &str) -> bool {
        !uid.is_empty()
            && uid.len() <= 255
            && uid.trim() == uid
            && !uid.chars().any(char::is_control)
            && !matches!(uid, "default" | "system" | "auto")
    }

    pub const fn platform_available(&self) -> bool {
        match self {
            Self::Pulse { .. } => cfg!(target_os = "linux"),
            Self::Mac { .. } => cfg!(target_os = "macos"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_mac_uids_allow_device_punctuation_without_default_aliases() {
        for uid in [
            "BuiltInMicrophoneDevice",
            "AppleUSBAudioEngine:Device:Input",
            "USB microphone (1)",
            "麦克风",
        ] {
            assert!(Selection::valid_mac_uid(uid));
        }
        for uid in [
            "", "default", "auto", "system", " input", "input ", "a\0b", "a\nb",
        ] {
            assert!(!Selection::valid_mac_uid(uid));
        }
        assert!(Selection::valid_mac_uid(&"x".repeat(255)));
        assert!(!Selection::valid_mac_uid(&"x".repeat(256)));
    }

    #[test]
    fn pulse_selection_keeps_its_original_socket_and_device_constraints() {
        let selection = |server: &str, source: &str| Selection::Pulse {
            server: server.into(),
            source: source.into(),
        };
        assert!(selection("/run/user/1000/pulse/native", "alsa_input.usb-device").valid());
        for server in ["relative", "/run/../native", "tcp:localhost", "/has space"] {
            assert!(!selection(server, "mic").valid());
        }
        for source in ["", "@DEFAULT_SOURCE@", "mic/name", "a\0b"] {
            assert!(!selection("/run/pulse/native", source).valid());
        }
    }

    #[test]
    fn debug_never_exposes_local_source_or_server() {
        let selection = Selection::Pulse {
            server: "/private/server".into(),
            source: "private-source".into(),
        };
        assert_eq!(format!("{selection:?}"), "MicrophoneSelection(Pulse)");
        assert_eq!(
            format!(
                "{:?}",
                Selection::Mac {
                    uid: "private-source".into()
                }
            ),
            "MicrophoneSelection(CoreAudio)"
        );
    }
}

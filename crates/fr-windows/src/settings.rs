//! The desktop application's remembered choices, stored per user as a small
//! JSON document. Settings are conveniences: they preselect a row and the
//! connection options, and never grant, skip or replay any authorization.
#![forbid(unsafe_code)]

use serde_json::{Value, json};
use std::num::NonZeroU16;

/// Settings files larger than this are ignored (defaults are used).
pub(crate) const MAX_BYTES: usize = 16 * 1024;
/// The stored machine is a stable node ID; longer values are ignored.
const MAX_ID_BYTES: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent remembered check boxes"
)]
pub(crate) struct Settings {
    /// Stable node ID of the machine chosen last (row preselection only).
    pub last_machine: Option<String>,
    /// Request control when connecting (otherwise view only).
    pub control: bool,
    /// With control, share text clipboard (the host must also enable it).
    pub clipboard: bool,
    /// Ask which display to show after the host approves the session.
    pub choose_display: bool,
    /// Open the view full screen, asking for this monitor's own pixel size.
    pub fullscreen: bool,
    /// With control, send Windows shortcuts (Win, Alt+Tab, Ctrl+Esc...) to
    /// the host instead of letting Windows act on them.
    pub shortcuts: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            last_machine: None,
            control: true,
            clipboard: true,
            choose_display: false,
            fullscreen: true,
            shortcuts: true,
        }
    }
}

impl Settings {
    /// Lenient: a missing, oversized, malformed or partly invalid document
    /// yields defaults for the affected fields instead of an error.
    pub(crate) fn from_json(bytes: &[u8]) -> Self {
        let mut settings = Self::default();
        if bytes.len() > MAX_BYTES {
            return settings;
        }
        let Ok(Value::Object(map)) = serde_json::from_slice::<Value>(bytes) else {
            return settings;
        };
        let flag =
            |name: &str, default: bool| map.get(name).and_then(Value::as_bool).unwrap_or(default);
        settings.control = flag("control", settings.control);
        settings.clipboard = flag("clipboard", settings.clipboard);
        settings.choose_display = flag("choose_display", settings.choose_display);
        settings.fullscreen = flag("fullscreen", settings.fullscreen);
        settings.shortcuts = flag("shortcuts", settings.shortcuts);
        settings.last_machine = map
            .get("last_machine")
            .and_then(Value::as_str)
            .filter(|id| valid_id(id))
            .map(str::to_owned);
        settings
    }

    pub(crate) fn to_json(&self) -> String {
        let mut document = json!({
            "version": 1,
            "control": self.control,
            "clipboard": self.clipboard,
            "choose_display": self.choose_display,
            "fullscreen": self.fullscreen,
            "shortcuts": self.shortcuts,
        });
        if let Some(id) = self.last_machine.as_deref().filter(|id| valid_id(id)) {
            document["last_machine"] = json!(id);
        }
        serde_json::to_string_pretty(&document).unwrap_or_else(|_| "{}".to_owned())
    }
}

impl Settings {
    /// The command-line viewer arguments for one chosen machine with these
    /// options (the desktop application runs each session this way).
    pub(crate) fn viewer_args(
        &self,
        stable_id: &str,
        port: NonZeroU16,
        receive: Option<crate::receive_choice::Destination>,
    ) -> Vec<String> {
        // Always pass the original launch port, including the default. A child
        // or a later machine choice must not replace it with its own default.
        let mut args = vec![
            "view".to_owned(),
            stable_id.to_owned(),
            "--port".to_owned(),
            port.to_string(),
        ];
        if self.control {
            args.push("--control".to_owned());
            if !self.clipboard {
                args.push("--no-clipboard".to_owned());
            }
            if !self.shortcuts {
                args.push("--no-shortcuts".to_owned());
            }
            if let Some(receive) = receive {
                args.extend(receive.into_arguments());
            }
        }
        if self.choose_display {
            args.extend(["--display".to_owned(), "choose".to_owned()]);
        }
        if self.fullscreen {
            args.push("--fullscreen".to_owned());
        }
        args
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_ID_BYTES && !id.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn port() -> NonZeroU16 {
        NonZeroU16::new(8443).unwrap()
    }

    #[test]
    fn settings_round_trip_and_default_to_control_with_clipboard() {
        let defaults = Settings::default();
        assert!(defaults.control && defaults.clipboard && !defaults.choose_display);
        assert!(defaults.fullscreen && defaults.shortcuts);
        let chosen = Settings {
            last_machine: Some("nABC123CNTRL".to_owned()),
            control: false,
            clipboard: false,
            choose_display: true,
            fullscreen: false,
            shortcuts: false,
        };
        assert_eq!(Settings::from_json(chosen.to_json().as_bytes()), chosen);
        assert_eq!(Settings::from_json(defaults.to_json().as_bytes()), defaults);
    }

    #[test]
    fn viewer_args_are_the_command_lines_own_options() {
        use crate::options::{ConnectionCommand, DisplayChoice};
        let parse = |settings: &Settings| {
            let args = settings.viewer_args("nSTABLE1CNTRL", port(), None);
            let command = ConnectionCommand::parse(&args, 8443).expect("viewer arguments parse");
            assert!(command.view);
            assert_eq!(command.selection, Some("nSTABLE1CNTRL"));
            command.options
        };
        let view_only = Settings {
            control: false,
            clipboard: true,
            shortcuts: false,
            fullscreen: false,
            ..Settings::default()
        };
        // Clipboard sharing and shortcuts only exist with control, so a
        // view-only session carries neither flag.
        assert_eq!(
            view_only.viewer_args("nSTABLE1CNTRL", port(), None),
            ["view", "nSTABLE1CNTRL", "--port", "8443"]
        );
        let options = parse(&view_only);
        assert!(!options.control && !options.fullscreen);
        assert_eq!(options.display, DisplayChoice::Primary);
        let options = parse(&Settings::default());
        assert!(options.control && options.clipboard);
        assert!(options.fullscreen && options.shortcuts);
        let options = parse(&Settings {
            clipboard: false,
            choose_display: true,
            fullscreen: false,
            shortcuts: false,
            ..Settings::default()
        });
        assert!(options.control && !options.clipboard);
        assert!(!options.fullscreen && !options.shortcuts);
        assert_eq!(options.display, DisplayChoice::Choose);
    }

    #[test]
    fn malformed_or_hostile_documents_fall_back_field_by_field() {
        for bytes in [
            b"".to_vec(),
            b"not json".to_vec(),
            b"[1,2,3]".to_vec(),
            vec![b' '; MAX_BYTES + 1],
        ] {
            assert_eq!(Settings::from_json(&bytes), Settings::default());
        }
        let partial = br#"{"control": "yes", "clipboard": false, "last_machine": 42}"#;
        let settings = Settings::from_json(partial);
        assert!(settings.control);
        assert!(!settings.clipboard);
        assert_eq!(settings.last_machine, None);
        let long = format!(r#"{{"last_machine": "{}"}}"#, "n".repeat(MAX_ID_BYTES + 1));
        assert_eq!(Settings::from_json(long.as_bytes()).last_machine, None);
        let control = "{\"last_machine\": \"n\\u0007x\"}";
        assert_eq!(Settings::from_json(control.as_bytes()).last_machine, None);
    }

    #[test]
    fn receive_consent_is_one_connection_argument_and_never_a_saved_setting() {
        use crate::options::{ConnectionCommand, ReceiveConflict};
        use crate::receive_choice::Choice;

        let path = r"C:\Receive here\日本語";
        let units: Vec<u16> = path.encode_utf16().chain([0]).collect();
        let mut choice = Choice::default();
        let request = choice.begin(10).unwrap();
        choice.finish(request, Some(&units), 20).unwrap();
        let settings = Settings::default();
        let args = settings.viewer_args("nSTABLE1CNTRL", port(), choice.take(true));
        let parsed = ConnectionCommand::parse(&args, 8443).unwrap();
        let receive = parsed.options.receive.unwrap();
        assert_eq!(receive.directory, std::path::PathBuf::from(path));
        assert_eq!(receive.conflict, ReceiveConflict::Reject);
        assert!(receive.max_file_bytes.is_none());
        assert!(receive.max_session_bytes.is_none());
        assert!(choice.take(true).is_none());

        let saved = settings.to_json();
        assert!(!saved.contains("receive"));
        assert!(!saved.contains("日本語"));
        let fresh_args =
            Settings::from_json(saved.as_bytes()).viewer_args("nSTABLE1CNTRL", port(), None);
        assert!(
            ConnectionCommand::parse(&fresh_args, 8443)
                .unwrap()
                .options
                .receive
                .is_none()
        );

        let request = choice.begin(30).unwrap();
        choice.finish(request, Some(&units), 40).unwrap();
        let view_only = Settings {
            control: false,
            ..settings
        };
        let args = view_only.viewer_args("nSTABLE1CNTRL", port(), choice.take(true));
        assert!(
            ConnectionCommand::parse(&args, 8443)
                .unwrap()
                .options
                .receive
                .is_none()
        );
    }

    #[test]
    fn every_selected_child_keeps_the_launch_port_with_its_own_viewer_choices() {
        use crate::options::{AppCommand, ConnectionCommand, DisplayChoice};

        for value in [1, 8443, 47819, 65535] {
            let invocation = ["--port".to_owned(), value.to_string()];
            let AppCommand::Picker { port } = AppCommand::parse(&invocation, 8443).unwrap() else {
                panic!("viewing invocation");
            };
            for control in [false, true] {
                for choose_display in [false, true] {
                    let settings = Settings {
                        control,
                        choose_display,
                        ..Settings::default()
                    };
                    // Each return to the picker uses the original app port,
                    // even if the selected host or display preference changes.
                    for selected in ["nFIRST1", "nSECOND2"] {
                        let arguments = settings.viewer_args(selected, port, None);
                        let command = ConnectionCommand::parse(&arguments, 443).unwrap();
                        assert_eq!(command.selection, Some(selected));
                        assert_eq!(command.options.port, value);
                        assert_eq!(command.options.control, control);
                        assert_eq!(
                            command.options.display,
                            if choose_display {
                                DisplayChoice::Choose
                            } else {
                                DisplayChoice::Primary
                            }
                        );
                        assert_eq!(
                            arguments.iter().filter(|value| *value == "--port").count(),
                            1
                        );
                    }
                    // Saving ordinary preferences cannot change a later
                    // Start-menu launch's default endpoint.
                    let saved = settings.to_json();
                    assert!(!saved.contains("port"));
                    assert_eq!(Settings::from_json(saved.as_bytes()), settings);
                    assert_eq!(
                        AppCommand::parse(&[], 8443),
                        Some(AppCommand::Picker {
                            port: NonZeroU16::new(8443).unwrap(),
                        })
                    );
                }
            }
        }
    }

    #[test]
    fn cancelled_receive_choice_keeps_the_port_without_adding_file_consent() {
        use crate::options::ConnectionCommand;
        use crate::receive_choice::Choice;

        let port = NonZeroU16::new(47819).unwrap();
        let mut choice = Choice::default();
        let request = choice.begin(10).unwrap();
        choice.finish(request, None, 20).unwrap();
        let arguments = Settings::default().viewer_args("nHOST1", port, choice.take(true));
        let command = ConnectionCommand::parse(&arguments, 8443).unwrap();
        assert_eq!(command.options.port, 47819);
        assert!(command.options.receive.is_none());
    }
}

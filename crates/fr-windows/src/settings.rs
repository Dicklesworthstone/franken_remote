//! The desktop application's remembered choices, stored per user as a small
//! JSON document. Settings are conveniences: they preselect a row and the
//! connection options, and never grant, skip or replay any authorization.
#![forbid(unsafe_code)]

use serde_json::{Value, json};

/// Settings files larger than this are ignored (defaults are used).
pub(crate) const MAX_BYTES: usize = 16 * 1024;
/// The stored machine is a stable node ID; longer values are ignored.
const MAX_ID_BYTES: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Settings {
    /// Stable node ID of the machine chosen last (row preselection only).
    pub last_machine: Option<String>,
    /// Request control when connecting (otherwise view only).
    pub control: bool,
    /// With control, share text clipboard (the host must also enable it).
    pub clipboard: bool,
    /// Ask which display to show after the host approves the session.
    pub choose_display: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            last_machine: None,
            control: true,
            clipboard: true,
            choose_display: false,
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
    pub(crate) fn viewer_args(&self, stable_id: &str) -> Vec<String> {
        let mut args = vec!["view".to_owned(), stable_id.to_owned()];
        if self.control {
            args.push("--control".to_owned());
            if !self.clipboard {
                args.push("--no-clipboard".to_owned());
            }
        }
        if self.choose_display {
            args.extend(["--display".to_owned(), "choose".to_owned()]);
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

    #[test]
    fn settings_round_trip_and_default_to_control_with_clipboard() {
        let defaults = Settings::default();
        assert!(defaults.control && defaults.clipboard && !defaults.choose_display);
        let chosen = Settings {
            last_machine: Some("nABC123CNTRL".to_owned()),
            control: false,
            clipboard: false,
            choose_display: true,
        };
        assert_eq!(Settings::from_json(chosen.to_json().as_bytes()), chosen);
        assert_eq!(Settings::from_json(defaults.to_json().as_bytes()), defaults);
    }

    #[test]
    fn viewer_args_are_the_command_lines_own_options() {
        use crate::options::{ConnectionCommand, DisplayChoice};
        let parse = |settings: &Settings| {
            let args = settings.viewer_args("nSTABLE1CNTRL");
            let command = ConnectionCommand::parse(&args, 8443).expect("viewer arguments parse");
            assert!(command.view);
            assert_eq!(command.selection, Some("nSTABLE1CNTRL"));
            command.options
        };
        let view_only = Settings {
            control: false,
            clipboard: true,
            ..Settings::default()
        };
        // Clipboard sharing only exists with control, so a view-only
        // session carries no clipboard flag at all.
        assert_eq!(
            view_only.viewer_args("nSTABLE1CNTRL"),
            ["view", "nSTABLE1CNTRL"]
        );
        let options = parse(&view_only);
        assert!(!options.control);
        assert_eq!(options.display, DisplayChoice::Primary);
        let options = parse(&Settings::default());
        assert!(options.control && options.clipboard);
        let options = parse(&Settings {
            clipboard: false,
            choose_display: true,
            ..Settings::default()
        });
        assert!(options.control && !options.clipboard);
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
}

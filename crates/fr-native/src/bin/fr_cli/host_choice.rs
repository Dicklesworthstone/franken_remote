//! Shared machine-choice identity projection for the native client shells.
//!
//! A picker returns an original ID/name pair, never a cached network address
//! or admission result. The connector revalidates that exact pair before it
//! opens a remote session.
use crate::{
    Failure,
    options::{Connection, DisplayChoice, Target},
};
use fr_client::discovery::picker::{Candidate, Catalog, PathHint};

/// The desktop application's connection choices, shown beside the list for
/// `--pick-only` and reported with the chosen host. They shape only the next
/// command line; that connector requests control, clipboard and audio itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent local connection preferences"
)]
pub(super) struct Choices {
    pub control: bool,
    pub clipboard: bool,
    pub choose_display: bool,
    pub playback_audio: bool,
    /// A new local opt-in for this connection, never a saved preference.
    pub send_files: bool,
    /// The parent retains the destination; the picker receives only this hint.
    pub receive_available: bool,
    /// Explicit consent for this controlling connection, never remembered.
    pub receive_files: bool,
}
impl Choices {
    /// The window's initial choices, only for a `--pick-only` request.
    pub(super) fn initial(connection: &Connection) -> Option<Self> {
        connection.pick.as_ref().map(|pick| Self {
            control: connection.control,
            clipboard: pick.clipboard,
            choose_display: matches!(connection.display, DisplayChoice::Choose),
            playback_audio: pick.playback_audio,
            send_files: false,
            receive_available: pick.receive_available,
            receive_files: false,
        })
    }
}

pub(super) fn notice(connection: &Connection) -> Option<&str> {
    connection
        .pick
        .as_ref()
        .and_then(|pick| pick.notice.as_deref())
}

pub(super) fn validate(target: &Target) -> Result<(), Failure> {
    if !target.node.is_empty() || target.saved.is_some() || target.by_name || target.port == 0 {
        return Err(Failure::new(
            "invalid_host_choice",
            "Use --choose-host in place of the positional host, without --saved or --by-name.",
            2,
        ));
    }
    Ok(())
}

pub(super) fn catalog(snapshot: &fr_tailnet::Discovery) -> Result<Catalog, Failure> {
    if snapshot.peers().len() > fr_client::discovery::picker::MAX_CANDIDATES {
        return Err(invalid_snapshot());
    }
    let candidates = snapshot
        .peers()
        .iter()
        .map(|peer| {
            Candidate::new(
                peer.stable_id().to_owned(),
                peer.certificate_name().to_owned(),
                match peer.transport_path() {
                    fr_tailnet::PeerTransportPath::Direct { .. } => PathHint::Direct,
                    fr_tailnet::PeerTransportPath::DerpRelayed { .. } => PathHint::Relayed,
                    fr_tailnet::PeerTransportPath::Unknown => PathHint::Unknown,
                },
            )
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| invalid_snapshot())?;
    Catalog::new(candidates).map_err(|_| invalid_snapshot())
}

pub(super) fn cancelled() -> Failure {
    Failure::new(
        "host_choice_cancelled",
        "Host selection was cancelled. No remote connection or input request was opened.",
        130,
    )
}

pub(super) fn invalid_snapshot() -> Failure {
    Failure::new(
        "host_choice_invalid_snapshot",
        "The discovery snapshot contains an invalid, ambiguous or out-of-bounds peer identity. Refresh installed Tailscale metadata before choosing again; no remote connection was opened.",
        1,
    )
}

pub(super) fn selected_report(
    candidate: &Candidate,
    port: u16,
    json: bool,
    choices: Option<Choices>,
) -> String {
    if json {
        let mut report = serde_json::json!({
            "schema_version": 1, "event": "host_selected",
            "node_id": candidate.stable_id(),
            "certificate_name": candidate.certificate_name(), "port": port,
            "selection_only": true, "fresh_lookup_required": true,
            "remote_session_started": false, "input_requested": false
        });
        if let (Some(choices), Some(object)) = (choices, report.as_object_mut()) {
            object.insert("control".into(), choices.control.into());
            object.insert("clipboard".into(), choices.clipboard.into());
            object.insert("choose_display".into(), choices.choose_display.into());
            object.insert("playback_audio".into(), choices.playback_audio.into());
            object.insert("send_files".into(), choices.send_files.into());
            object.insert("receive_files".into(), choices.receive_files.into());
        }
        format!("{report}\n")
    } else if let Some(choices) = choices {
        format!(
            "Selected {} ({}), port {port}: {}, clipboard {}, {}, host audio {}, file sending {}, file receiving {}.\n",
            candidate.certificate_name(),
            candidate.stable_id(),
            if choices.control {
                "view and control"
            } else {
                "view only"
            },
            if choices.control && choices.clipboard {
                "shared"
            } else {
                "not shared"
            },
            if choices.choose_display {
                "ask which display"
            } else {
                "only display"
            },
            if choices.playback_audio {
                "requested"
            } else {
                "off"
            },
            if choices.control && choices.send_files {
                "enabled for this connection"
            } else {
                "off"
            },
            if choices.control && choices.receive_available && choices.receive_files {
                "enabled for this connection"
            } else {
                "off"
            }
        )
    } else {
        format!(
            "Selected {} ({}), port {port}. Rechecking identity and host admission.\n",
            candidate.certificate_name(),
            candidate.stable_id()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> Target {
        let args = "connect --choose-host --view-only --experimental-native --display choose"
            .split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let crate::options::Command::Connect(connection) =
            crate::options::parse(&args).unwrap().command
        else {
            panic!("expected a connection");
        };
        connection.target
    }

    #[test]
    fn every_host_picker_starts_without_file_sending_consent() {
        for role in ["--control", "--view-only"] {
            let args = format!(
                "connect --choose-host --pick-only --experimental-native --display only {role} --audio"
            )
            .split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>();
            let crate::options::Command::Connect(connection) =
                crate::options::parse(&args).unwrap().command
            else {
                panic!("expected a connection");
            };
            let initial = Choices::initial(&connection).unwrap();
            assert!(!initial.send_files);
            assert!(initial.playback_audio);
        }
    }

    #[test]
    fn explicit_local_host_choice_has_no_hidden_identity_override() {
        assert!(validate(&target()).is_ok());
        let mut positional = target();
        positional.node = "n-other".to_owned();
        let mut by_name = target();
        by_name.by_name = true;
        let mut zero_port = target();
        zero_port.port = 0;
        for invalid in [positional, by_name, zero_port] {
            let error = validate(&invalid).unwrap_err();
            assert_eq!(error.code, "invalid_host_choice");
            assert_eq!(error.exit, 2);
        }
    }
}

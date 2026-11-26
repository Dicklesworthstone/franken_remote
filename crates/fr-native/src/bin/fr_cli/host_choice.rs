//! Shared machine-choice identity projection for the native client shells.
//!
//! A picker returns an original ID/name pair, never a cached network address
//! or admission result. The connector revalidates that exact pair before it
//! opens a remote session.
use crate::{
    Failure,
    options::{Connection, DisplayChoice, Target},
};
use fr_client::discovery::picker::{Candidate, Catalog, PathHint, ProbeHint};

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
    /// Choose initial files in the native file dialog for this connection.
    pub choose_files: bool,
    /// The parent retains the destination; the picker receives only this hint.
    pub receive_available: bool,
    /// Explicit consent for this controlling connection, never remembered.
    pub receive_files: bool,
    /// Open this controlling connection's muted microphone controls. Native
    /// permission and a separate fresh Talk action are still required.
    pub microphone_controls: bool,
    /// Capture system shortcuts in this Mac controlling window only. This
    /// always starts off and is never a remembered application preference.
    pub capture_system_shortcuts: bool,
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
            choose_files: false,
            receive_available: pick.receive_available,
            receive_files: false,
            microphone_controls: false,
            capture_system_shortcuts: false,
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
    if !target.node.is_empty()
        || target.saved.is_some()
        || target.expected_name.is_some()
        || target.by_name
        || target.port == 0
    {
        return Err(Failure::new(
            "invalid_host_choice",
            "Use --choose-host in place of the positional host, without --saved or --by-name.",
            2,
        ));
    }
    Ok(())
}

pub(super) fn catalog(snapshot: &fr_tailnet::Discovery) -> Result<Catalog, Failure> {
    project_catalog(snapshot, None)
}

/// Hint ordinal belongs to the exact discovery snapshot, before Catalog sorts
/// its rows. A failed/omitted probe never removes the original selectable peer.
pub(super) fn catalog_with_probe_hints(
    snapshot: &fr_tailnet::Discovery,
    hints: &[ProbeHint],
) -> Result<Catalog, Failure> {
    if hints.len() > snapshot.peers().len()
        || hints.len() > fr_client::discovery::batch::MAX_BATCH_TARGETS
    {
        return Err(invalid_snapshot());
    }
    project_catalog(snapshot, Some(hints))
}

fn project_catalog(
    snapshot: &fr_tailnet::Discovery,
    hints: Option<&[ProbeHint]>,
) -> Result<Catalog, Failure> {
    if snapshot.peers().len() > fr_client::discovery::picker::MAX_CANDIDATES {
        return Err(invalid_snapshot());
    }
    let candidates = snapshot
        .peers()
        .iter()
        .enumerate()
        .map(|(index, peer)| {
            let candidate = Candidate::new(
                peer.stable_id().to_owned(),
                peer.certificate_name().to_owned(),
                match peer.transport_path() {
                    fr_tailnet::PeerTransportPath::Direct { .. } => PathHint::Direct,
                    fr_tailnet::PeerTransportPath::DerpRelayed { .. } => PathHint::Relayed,
                    fr_tailnet::PeerTransportPath::Unknown => PathHint::Unknown,
                },
            )?;
            Ok::<_, fr_client::discovery::picker::Error>(match hints {
                Some(hints) => candidate
                    .with_probe_hint(hints.get(index).copied().unwrap_or(ProbeHint::NotProbed)),
                None => candidate,
            })
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| invalid_snapshot())?;
    Catalog::new(candidates).map_err(|_| invalid_snapshot())
}

pub(super) fn cancelled() -> Failure {
    Failure::new(
        "host_choice_cancelled",
        "Host selection was cancelled. No desktop observation or input request was opened.",
        130,
    )
}

pub(super) fn invalid_snapshot() -> Failure {
    Failure::new(
        "host_choice_invalid_snapshot",
        "The discovery snapshot contains an invalid, ambiguous or out-of-bounds peer identity. Refresh installed Tailscale metadata before choosing again; no desktop observation was opened.",
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
            object.insert("choose_files".into(), choices.choose_files.into());
            object.insert("receive_files".into(), choices.receive_files.into());
            object.insert(
                "microphone_controls".into(),
                choices.microphone_controls.into(),
            );
            object.insert(
                "capture_system_shortcuts".into(),
                choices.capture_system_shortcuts.into(),
            );
        }
        format!("{report}\n")
    } else if let Some(choices) = choices {
        format!(
            "Selected {} ({}), port {port}: {}, clipboard {}, {}, host audio {}, file sending {}, file receiving {}, microphone controls {}, system-shortcut capture {}.\n",
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
            if choices.control && choices.choose_files && choices.send_files {
                "choose local files before connecting and allow later drops"
            } else if choices.control && choices.choose_files {
                "choose local files before connecting"
            } else if choices.control && choices.send_files {
                "enabled for this connection"
            } else {
                "off"
            },
            if choices.control && choices.receive_available && choices.receive_files {
                "enabled for this connection"
            } else {
                "off"
            },
            if choices.control && choices.microphone_controls {
                "requested (starts muted; Allow microphone and Talk required)"
            } else {
                "off"
            },
            if choices.control && choices.capture_system_shortcuts {
                "requested for this connection (Accessibility permission required)"
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

    #[test]
    fn probe_results_stay_with_the_original_peer_after_sorting_and_omission() {
        use fr_tailnet::{DiscoveredPeer, Discovery, DiscoveryExclusions, PeerTransportPath};
        let snapshot = Discovery::from_parts(
            [
                ("n-z", "zulu.fixture.ts.net"),
                ("n-a", "alpha.fixture.ts.net"),
                ("n-m", "middle.fixture.ts.net"),
            ]
            .into_iter()
            .map(|(id, name)| {
                DiscoveredPeer::from_parts(
                    id.into(),
                    name.into(),
                    vec!["100.64.0.2".parse().unwrap()],
                    PeerTransportPath::Unknown,
                )
            })
            .collect(),
            DiscoveryExclusions {
                shared: 0,
                expired: 0,
                unusable: 0,
            },
            10,
            20,
        );
        let original = catalog_with_probe_hints(
            &snapshot,
            &[ProbeHint::ControlAdvertised, ProbeHint::IdentityRefused],
        )
        .unwrap();
        assert_eq!(
            original.len(),
            3,
            "failed and omitted peers remain selectable"
        );
        for (index, id, hint) in [
            (0, "n-a", ProbeHint::IdentityRefused),
            (1, "n-m", ProbeHint::NotProbed),
            (2, "n-z", ProbeHint::ControlAdvertised),
        ] {
            let row = original.get(index).unwrap();
            assert_eq!(row.stable_id(), id);
            assert_eq!(row.probe_hint(), Some(hint));
            let report: serde_json::Value =
                serde_json::from_str(&selected_report(row, 9443, true, None)).unwrap();
            assert_eq!(report["fresh_lookup_required"], true);
            assert_eq!(report["remote_session_started"], false);
            assert_eq!(report["input_requested"], false);
        }
        assert!(
            catalog(&snapshot)
                .unwrap()
                .candidates()
                .iter()
                .all(|row| row.probe_hint().is_none())
        );
        assert!(catalog_with_probe_hints(&snapshot, &[ProbeHint::NotProbed; 4]).is_err());
    }

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
    fn every_host_picker_starts_without_file_or_microphone_consent() {
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
            assert!(!initial.receive_files);
            assert!(!initial.microphone_controls);
            assert!(!initial.capture_system_shortcuts);
            assert!(initial.playback_audio);
        }
    }

    #[test]
    fn confirmed_microphone_choice_reports_only_a_request_for_muted_controls() {
        let candidate = Candidate::new(
            "n-mac".into(),
            "mac.fixture.ts.net".into(),
            PathHint::Direct,
        )
        .unwrap();
        for microphone_controls in [false, true] {
            let choices = Choices {
                control: true,
                clipboard: false,
                choose_display: false,
                playback_audio: false,
                send_files: false,
                choose_files: false,
                receive_available: false,
                receive_files: false,
                microphone_controls,
                capture_system_shortcuts: false,
            };
            let report: serde_json::Value =
                serde_json::from_str(&selected_report(&candidate, 8443, true, Some(choices)))
                    .unwrap();
            assert_eq!(
                report["microphone_controls"].as_bool(),
                Some(microphone_controls)
            );
            assert_eq!(report["selection_only"], true);
            assert_eq!(report["fresh_lookup_required"], true);
            assert_eq!(report["remote_session_started"], false);
            assert_eq!(report["input_requested"], false);
            for absent in [
                "microphone_permission",
                "microphone_active",
                "microphone_source",
            ] {
                assert!(report.get(absent).is_none());
            }
            let text = selected_report(&candidate, 8443, false, Some(choices));
            if microphone_controls {
                assert!(text.contains(
                    "microphone controls requested (starts muted; Allow microphone and Talk required)"
                ));
            } else {
                assert!(text.contains("microphone controls off"));
            }
        }
        let identity_only: serde_json::Value =
            serde_json::from_str(&selected_report(&candidate, 8443, true, None)).unwrap();
        assert!(identity_only.get("microphone_controls").is_none());
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

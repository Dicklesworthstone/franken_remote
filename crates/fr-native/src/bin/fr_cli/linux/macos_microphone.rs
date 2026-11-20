//! Explicit source preflight before connecting; no TCC prompt or recording.
use super::{Failure, Session};
use crate::options::MicrophoneRequest;

#[cfg(feature = "macos-microphone")]
pub(super) type Prepared = fr_native::audio_process::network::Options;
#[cfg(not(feature = "macos-microphone"))]
pub(super) type Prepared = ();

pub(super) fn check(request: Option<&MicrophoneRequest>) -> Result<(), Failure> {
    let Some(request) = request else {
        return Ok(());
    };
    if !cfg!(feature = "macos-microphone") {
        return Err(Failure::new(
            "microphone_unavailable_in_build",
            "Build fr with --features macos-desktop,macos-microphone and install its matching signed fr-audio-worker for explicit microphone forwarding.",
            2,
        ));
    }
    if request.server.is_some() {
        return Err(Failure::new(
            "microphone_server_unsupported",
            "--microphone-server is Linux-only. On macOS select --microphone-source default to pin the current input once, or pass an exact CoreAudio device UID.",
            2,
        ));
    }
    Ok(())
}
pub(super) fn prepare(request: Option<&MicrophoneRequest>) -> Result<Option<Prepared>, Failure> {
    check(request)?;
    let Some(request) = request else {
        return Ok(None);
    };
    #[cfg(feature = "macos-microphone")]
    {
        use fr_native::audio_process::{Selection, network::Options, transmit::Source};
        let uid = if request.source == "default" {
            fr_native::macos_microphone_capture::default_uid().map_err(|_| Failure::new(
                "microphone_source_unavailable", "Select an available local input in System Settings or supply an exact CoreAudio device UID before connecting.", 2))?
        } else {
            request.source.clone()
        };
        if !Selection::valid_mac_uid(&uid) {
            return Err(Failure::new(
                "microphone_source_invalid",
                "Select one exact CoreAudio device UID; recording never follows a default-device change.",
                2,
            ));
        }
        let image = std::env::current_exe().ok()
            .and_then(|exe| exe.parent().map(|dir| dir.join("fr-audio-worker")))
            .filter(|path| path.is_absolute() && path.is_file())
            .ok_or_else(|| Failure::new("microphone_worker_unavailable",
                "Install the matching signed fr-audio-worker beside fr. It carries the microphone usage description and audio-input entitlement used by both permission and capture.", 2))?;
        Ok(Some(Options {
            image,
            source: Source {
                selection: Selection::Mac { uid },
                bitrate: 64_000,
            },
            profile: fr_client::audio::uplink::Profile {
                channels: fr_core::audio::AudioChannels::Mono,
                frame_duration_ms: 10,
                max_packet_bytes: 512,
                max_record_bytes: fr_wire::audio::AUDIO_PACKET_OVERHEAD + 512,
                jitter_target_ms: 20,
            },
        }))
    }
    #[cfg(not(feature = "macos-microphone"))]
    {
        let _ = request;
        Ok(None)
    }
}
pub(super) fn install(session: Session, prepared: Option<Prepared>) -> Session {
    #[cfg(feature = "macos-microphone")]
    {
        match prepared {
            Some(options) => {
                session.with_microphone(fr_native::macos_microphone::Local::new(options))
            }
            None => session,
        }
    }
    #[cfg(not(feature = "macos-microphone"))]
    {
        let _ = prepared;
        session
    }
}

pub(super) fn offer(
    original: fr_wire::negotiation::Offer,
    requested: bool,
) -> Result<fr_wire::negotiation::Offer, Failure> {
    if !requested {
        return Ok(original);
    }
    fr_wire::audio_up::with_microphone(original).map_err(|_| Failure::new(
        "microphone_feature_limit", "The selected microphone, playback, clipboard and file profile exceeds the native capability limit. Use --no-clipboard or select fewer file directions.", 2))
}

pub(super) fn check_profile(
    clipboard: bool,
    directions: u32,
    playback: bool,
    requested: bool,
) -> Result<(), Failure> {
    if !requested {
        return Ok(());
    }
    let original = match directions {
        0 => fr_client::native::control_offer_with_auxiliary(clipboard, false, false),
        1 => fr_client::native::control_offer_with_auxiliary(clipboard, true, false),
        2 => fr_client::native::control_offer_with_duplex(clipboard, false),
        _ => {
            return Err(Failure::new(
                "microphone_feature_limit",
                "Unsupported file direction profile.",
                2,
            ));
        }
    };
    let original = super::playback::offer(original, playback)?;
    let original = fr_wire::control::with_pixel_scroll(original)
        .and_then(fr_wire::control::with_committed_text)
        .map_err(|_| {
            Failure::new(
                "microphone_feature_limit",
                "The selected feature profile cannot fit its control capabilities.",
                2,
            )
        })?;
    offer(original, true).map(|_| ())
}

pub(super) fn summary(
    report: Option<&fr_native::macos::session::MicrophoneReport>,
    json: bool,
) -> String {
    let Some(report) = report else {
        return String::new();
    };
    if json {
        let reason = report
            .reason
            .map_or_else(|| "null".into(), |reason| format!("\"{reason}\""));
        format!(
            ",\"microphone\":{{\"requested\":{},\"selected\":{},\"os_permission_granted\":{},\"native_capture_ready\":{},\"state\":\"{}\",\"reason\":{reason},\"host_audibility_proven\":false}}",
            report.requested,
            report.selected,
            report.permission_granted,
            report.capture_ready,
            report.state
        )
    } else {
        format!(
            "Microphone: {}; selected: {}; OS permission granted: {}; native capture ready: {}; reason: {}. Host application audibility is unproven.",
            report.state,
            report.selected,
            report.permission_granted,
            report.capture_ready,
            report.reason.unwrap_or("none")
        )
    }
}

pub(super) fn preserve(
    report: Option<&fr_native::macos::session::MicrophoneReport>,
    failure: &mut Failure,
) {
    if report.is_none()
        || failure
            .control_history
            .is_some_and(|previous| previous.json.contains(",\"microphone\":"))
    {
        return;
    }
    let (mut json, mut text) = failure.control_history.map_or_else(
        || (String::new(), String::new()),
        |previous| (previous.json.clone(), previous.text.clone()),
    );
    json.push_str(&summary(report, true));
    text.push(' ');
    text.push_str(&summary(report, false));
    failure.control_history = Some(Box::leak(Box::new(crate::ControlHistory { json, text })));
}

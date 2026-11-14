//! Explicit microphone on the original public control attempt. A mapped local
//! panel owns talk intent; connecting, host approval, OS access and reconnects
//! cannot create it. Native capture retains the same independent UI fence.
use super::super::{Failure, options::MicrophoneRequest};
use frd::{
    native_connection::reconnect::CallbackError,
    session_startup::{ControlledViewer, viewer_microphone as mic},
};
#[cfg(feature = "linux-microphone")]
use std::time::Duration;
use std::{
    cell::RefCell,
    path::{Component, Path, PathBuf},
    rc::Rc,
};

#[cfg(feature = "linux-microphone")]
use fr_core::audio::{AudioChannels, MicPermission, MicTalkMode};
#[cfg(feature = "linux-microphone")]
use fr_native::{
    audio_process::{
        network::{CaptureRetirement, Options, Uplink},
        transmit::Source,
    },
    sharing_indicator::{MicrophonePanel, MicrophoneState, Status as PanelStatus},
};

/// One invocation's local selection and content-free outcome. The actual UI
/// and cleanup observer survive until the original connection is cleaned up.
// Lifetime fences and historical observations are independent: a stopped
// choice can have been selected, captured speech, or failed during cleanup.
#[allow(clippy::struct_excessive_bools)]
pub(super) struct Local {
    #[cfg(feature = "linux-microphone")]
    options: Option<Options>,
    #[cfg(feature = "linux-microphone")]
    display: String,
    #[cfg(feature = "linux-microphone")]
    panel: Option<MicrophonePanel>,
    #[cfg(feature = "linux-microphone")]
    retirement: Option<CaptureRetirement>,
    window: Option<fr_native::viewer_window::WindowControl>,
    used: bool,
    stopped: bool,
    selected: bool,
    // False means no Talking observation; RECORD can begin before a Ready
    // reply or a later callback, so it is not proof that capture never began.
    ever_talking: bool,
    state: &'static str,
    reason: Option<&'static str>,
    fatal: bool,
}

/// A source is always explicit. Socket discovery only resolves the local
/// `PulseAudio` server; it never selects a default microphone or starts RECORD.
pub(super) fn resolve(
    request: Option<&MicrophoneRequest>,
    display: impl FnOnce() -> Result<String, Failure>,
) -> Result<Option<Local>, Failure> {
    let Some(request) = request else {
        return Ok(None);
    };
    if !cfg!(feature = "linux-microphone") {
        return Err(Failure::new(
            "microphone_unavailable_in_build",
            "This fr needs --features linux-desktop,linux-microphone and its matching fr-audio-worker for explicit microphone forwarding.",
            2,
        ));
    }
    let server = server(request)?;
    let image = std::env::current_exe().ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("fr-audio-worker")))
        .filter(|path| path.is_absolute() && path.is_file())
        .ok_or_else(|| Failure::new(
            "microphone_worker_unavailable",
            "Install the matching fr-audio-worker beside fr; the microphone has no in-process or downloaded fallback.",
            2,
        ))?;
    let display = display()?;
    #[cfg(feature = "linux-microphone")]
    let options = Options {
        image,
        source: Source {
            server,
            name: request.source.clone(),
            bitrate: 64_000,
        },
        profile: fr_client::audio::uplink::Profile {
            channels: AudioChannels::Mono,
            frame_duration_ms: 10,
            max_packet_bytes: 512,
            max_record_bytes: fr_wire::audio::AUDIO_PACKET_OVERHEAD + 512,
            jitter_target_ms: 20,
        },
    };
    #[cfg(not(feature = "linux-microphone"))]
    let _ = (server, image, display);
    Ok(Some(Local {
        #[cfg(feature = "linux-microphone")]
        options: Some(options),
        #[cfg(feature = "linux-microphone")]
        display,
        #[cfg(feature = "linux-microphone")]
        panel: None,
        #[cfg(feature = "linux-microphone")]
        retirement: None,
        window: None,
        used: false,
        stopped: false,
        selected: false,
        ever_talking: false,
        state: "not_started",
        reason: None,
        fatal: false,
    }))
}
fn valid_server(path: &Path) -> bool {
    path.is_absolute()
        && !path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
        && path.to_str().is_some_and(|text| {
            (2..=100).contains(&text.len())
                && text
                    .bytes()
                    .all(|byte| byte > b' ' && byte != 127 && byte != b':')
        })
}
fn server(request: &MicrophoneRequest) -> Result<PathBuf, Failure> {
    use std::os::unix::fs::FileTypeExt;
    let selected = request.server.clone().or_else(|| {
        std::env::var_os("PULSE_SERVER")
            .map(|value| {
                let value = value.to_string_lossy();
                PathBuf::from(value.strip_prefix("unix:").unwrap_or(&value))
            })
            .or_else(|| {
                std::env::var_os("XDG_RUNTIME_DIR")
                    .map(|dir| PathBuf::from(dir).join("pulse/native"))
            })
    });
    selected.filter(|path| valid_server(path)
        && path.metadata().is_ok_and(|meta| meta.file_type().is_socket()))
        .ok_or_else(|| Failure::new(
            "microphone_server_unavailable",
            "Use an existing absolute local PulseAudio socket: --microphone-server PATH, PULSE_SERVER or XDG_RUNTIME_DIR. A server address or default source is not accepted.",
            2,
        ))
}

impl Local {
    pub(super) fn may_offer(&self) -> bool {
        !self.used && !self.stopped
    }
    pub(super) fn ready(&mut self, window: Option<fr_native::viewer_window::WindowControl>) {
        if !self.used && !self.stopped {
            self.window = window;
        }
    }
    /// Terminal for this invocation's microphone choice. The original native
    /// guard closes before later cleanup polling; a viewing retry cannot reset it.
    pub(super) fn stop(&mut self) {
        self.stopped = true;
        self.state = "stopped";
        #[cfg(feature = "linux-microphone")]
        {
            self.options = None;
            if let Some(panel) = &self.panel {
                panel.control().stop();
            }
        }
    }
    #[cfg(feature = "linux-microphone")]
    fn sample(&mut self, status: Option<mic::Status>) {
        if let Some(status) = status {
            self.ever_talking |= status.state == mic::State::Talking;
            self.state = state_name(status.state);
            if let Some(error) = status.reason {
                self.reason = Some(error_code(error));
            }
        }
    }
    // Without the microphone feature the turn cannot fail; the signature is
    // the feature build's.
    #[cfg_attr(not(feature = "linux-microphone"), allow(clippy::unnecessary_wraps))]
    pub(super) fn turn(&mut self, viewer: &mut ControlledViewer) -> Result<(), CallbackError> {
        #[cfg(feature = "linux-microphone")]
        {
            self.native_turn(viewer)
        }
        #[cfg(not(feature = "linux-microphone"))]
        {
            let _ = viewer;
            self.reason = Some("microphone_unavailable_in_build");
            self.stop();
            Ok(())
        }
    }
    #[cfg(feature = "linux-microphone")]
    fn native_turn(&mut self, viewer: &mut ControlledViewer) -> Result<(), CallbackError> {
        if self.stopped {
            return Ok(());
        }
        if !self.used {
            self.used = true;
            let target = self.window.as_ref().and_then(|window| window.target().ok());
            let Some(target) = target else {
                return self.refuse(viewer, "microphone_controls_unavailable");
            };
            let panel = MicrophonePanel::start_for_viewer(&self.display, target, |_| {});
            let Ok(panel) = panel else {
                return self.refuse(viewer, "microphone_controls_unavailable");
            };
            let check = panel.talk_epoch();
            self.panel = Some(panel);
            let Some(options) = self.options.take() else {
                return self.refuse(viewer, "microphone_choice_spent");
            };
            match Uplink::expect_on_with_consent(viewer, options, Duration::from_secs(2), check) {
                Ok(retirement) => {
                    self.selected = true;
                    self.retirement = Some(retirement);
                }
                Err(mic::Error::NotSelected) => {
                    self.reason = Some("microphone_not_selected");
                    self.stop();
                    return Ok(());
                }
                Err(error) => return self.refuse(viewer, error_code(error)),
            }
        }
        let panel = self.panel.as_ref().ok_or(CallbackError)?;
        let control = panel.control();
        match control.status() {
            PanelStatus::Opening => return Ok(()),
            PanelStatus::Stopped(_) => return self.refuse(viewer, "microphone_controls_lost"),
            PanelStatus::Mapped if !control.responsive() => {
                return self.refuse(viewer, "microphone_controls_lost");
            }
            PanelStatus::Mapped => {}
        }
        // Mute is serviced first. Its native UI predicate has already fenced
        // the old capture and every retained packet, before this callback.
        if panel.take_mute_request() {
            if let Err(error) = viewer.mute_microphone() {
                return self.refuse(viewer, error_code(error));
            }
        } else if panel.take_talk_request() {
            // PulseAudio's actual RECORD authorization remains in the native
            // worker; this records the independent real-device user consent.
            // No capture begins until the exact host configuration ACK.
            let result = viewer
                .set_microphone_permission(MicPermission::Granted)
                .and_then(|()| viewer.begin_microphone_talk(MicTalkMode::OpenMic { active: true }));
            if let Err(error) = result {
                // Busy is a consumed gesture, never a queued future restart.
                let _ = panel.publish(MicrophoneState::Muted);
                self.reason = Some(error_code(error));
            } else {
                self.reason = None;
            }
        }
        let status = viewer.microphone_status();
        let label = status.map_or(MicrophoneState::Requesting, |status| match status.state {
            mic::State::Muted if status.retirement != mic::Retirement::Pending => {
                MicrophoneState::Muted
            }
            mic::State::Talking => MicrophoneState::Live,
            mic::State::Stopped => MicrophoneState::Muted,
            _ => MicrophoneState::Requesting,
        });
        if !panel.publish(label) {
            return self.refuse(viewer, "microphone_controls_lost");
        }
        self.sample(status);
        if status.is_some_and(|status| status.state == mic::State::Stopped) {
            self.stop();
        }
        Ok(())
    }
    #[cfg(feature = "linux-microphone")]
    fn refuse(
        &mut self,
        viewer: &mut ControlledViewer,
        reason: &'static str,
    ) -> Result<(), CallbackError> {
        self.reason = Some(reason);
        self.stop();
        let _ = viewer.mute_microphone();
        let retired = viewer.retire_microphone();
        self.fatal = !self.selected || retired.is_err();
        if self.fatal {
            Err(CallbackError)
        } else {
            Ok(())
        }
    }
    pub(super) fn cleanup_complete(&mut self) -> bool {
        #[cfg(feature = "linux-microphone")]
        {
            if !self.stopped {
                return false;
            }
            if let Some(retirement) = &self.retirement {
                self.sample(retirement.status());
            }
            let native = self
                .retirement
                .as_ref()
                .is_none_or(CaptureRetirement::is_complete);
            let ui = self
                .panel
                .as_mut()
                .is_none_or(|panel| panel.finish().is_some());
            native && ui
        }
        #[cfg(not(feature = "linux-microphone"))]
        {
            self.stopped
        }
    }
    pub(super) fn summary(&self, json: bool) -> String {
        let reason = self
            .reason
            .map_or_else(|| "null".into(), |reason| format!("\"{reason}\""));
        if json {
            format!(
                ",\"microphone_requested\":true,\"microphone_selected\":{},\"microphone_capture_observed\":{},\"microphone_last_state\":\"{}\",\"microphone_reason\":{reason},\"microphone_audibility_proven\":false",
                self.selected, self.ever_talking, self.state
            )
        } else {
            format!(
                " Microphone: {}; native capture observed: {}; reason: {}. Host application audibility is unproven.",
                self.state,
                self.ever_talking,
                self.reason.unwrap_or("none")
            )
        }
    }
    pub(super) fn failure(&self) -> Option<Failure> {
        self.fatal.then(|| Failure::new(
            self.reason.unwrap_or("microphone_failed"),
            "Microphone setup or safe retirement failed; restore the explicit local source and visible controls before another microphone session. Recording never resumes automatically.",
            1,
        ))
    }
}
impl Drop for Local {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Bounded cleanup on the independent local context, after the original
/// connection has stopped. Actual child/UI completion is inspected each turn.
pub(super) async fn cleanup(local: &Rc<RefCell<Local>>, cx: &asupersync::cx::Cx) -> bool {
    local.borrow_mut().stop();
    let Some(clock) = cx.timer_driver() else {
        return false;
    };
    let started = clock.now();
    let Some(until) = started.as_nanos().checked_add(2_000_000_000) else {
        return false;
    };
    loop {
        if local.borrow_mut().cleanup_complete() {
            return true;
        }
        let now = clock.now().as_nanos();
        if now < started.as_nanos() || now >= until || cx.checkpoint().is_err() {
            return false;
        }
        asupersync::time::sleep_until(asupersync::types::Time::from_nanos(
            now.saturating_add(10_000_000).min(until),
        ))
        .await;
    }
}

#[cfg(feature = "linux-microphone")]
fn state_name(state: mic::State) -> &'static str {
    match state {
        mic::State::Negotiating => "negotiating",
        mic::State::Muted => "muted",
        mic::State::AwaitingHost => "awaiting_host",
        mic::State::Starting => "starting",
        mic::State::Talking => "capturing",
        mic::State::Stopped => "stopped",
    }
}
pub(super) fn failure(error: mic::Error) -> Failure {
    Failure::new(
        error_code(error),
        "Microphone forwarding ended. Check the explicitly selected local source and the host microphone controls; a new session starts muted and requires another local Talk action.",
        1,
    )
}
fn error_code(error: mic::Error) -> &'static str {
    match error {
        mic::Error::NotSelected => "microphone_not_selected",
        mic::Error::Busy => "microphone_not_ready",
        mic::Error::SetupExpired => "microphone_setup_expired",
        mic::Error::Capture(mic::CaptureFailure::Denied) => "microphone_permission_denied",
        mic::Error::Capture(mic::CaptureFailure::Selection | mic::CaptureFailure::Unavailable) => {
            "microphone_source_unavailable"
        }
        mic::Error::Capture(mic::CaptureFailure::Discontinuity) => {
            "microphone_capture_discontinuity"
        }
        mic::Error::Capture(mic::CaptureFailure::Spawn) => "microphone_worker_failed",
        mic::Error::Cancelled | mic::Error::Closed => "microphone_session_ended",
        mic::Error::Consent(fr_client::audio::uplink::Error::Permission) => {
            "microphone_permission_missing"
        }
        mic::Error::EgressExpired => "microphone_speech_expired",
        mic::Error::ReceiveExpired => "microphone_receive_expired",
        mic::Error::SendExpired => "microphone_send_expired",
        _ => "microphone_failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receive_expiry_is_distinct_from_speech_custody_and_session_failure() {
        assert_eq!(
            error_code(mic::Error::ReceiveExpired),
            "microphone_receive_expired"
        );
        assert_eq!(
            error_code(mic::Error::Transport(fr_transport::quic::Error::Stream(
                fr_wire::stream::StreamError::Expired,
            ))),
            "microphone_failed"
        );
        assert_eq!(error_code(mic::Error::SendExpired), "microphone_send_expired");
        assert_eq!(
            error_code(mic::Error::Transport(fr_transport::quic::Error::SendExpired)),
            "microphone_failed"
        );
        assert_eq!(
            error_code(mic::Error::EgressExpired),
            "microphone_speech_expired"
        );
        assert_eq!(error_code(mic::Error::Closed), "microphone_session_ended");
    }

    fn local() -> Local {
        Local {
            #[cfg(feature = "linux-microphone")]
            options: None,
            #[cfg(feature = "linux-microphone")]
            display: ":0".into(),
            #[cfg(feature = "linux-microphone")]
            panel: None,
            #[cfg(feature = "linux-microphone")]
            retirement: None,
            window: None,
            used: false,
            stopped: false,
            selected: false,
            ever_talking: false,
            state: "not_started",
            reason: None,
            fatal: false,
        }
    }

    #[test]
    fn microphone_choice_never_crosses_a_spent_controller_or_viewing_reconnect() {
        use crate::linux::{Interface, Progress, control};
        use fr_native::desktop::reconnect::Ui;
        let microphone = Rc::new(RefCell::new(local()));
        let progress = Rc::new(RefCell::new(Progress {
            control: Some(control::Counters::default()),
            microphone: Some(microphone.clone()),
            ..Progress::default()
        }));
        let mut ui = Interface {
            display: crate::options::DisplayChoice::Only,
            progress: progress.clone(),
            attempt: None,
            clipboard: None,
            audio: None,
        };
        let original =
            fr_wire::audio_up::with_microphone(fr_client::native::control_offer()).unwrap();
        let offered = ui.offer(1, &original).unwrap();
        assert!(
            offered
                .capabilities
                .iter()
                .any(|cap| cap.name == fr_wire::audio_up::CAPABILITY)
        );
        progress
            .borrow_mut()
            .control
            .as_mut()
            .unwrap()
            .spend_request();
        for attempt in [1, 2, 3] {
            let offer = ui.offer(attempt, &original).unwrap();
            assert!(
                offer
                    .capabilities
                    .iter()
                    .all(|cap| cap.name != fr_wire::audio_up::CAPABILITY)
            );
        }
        microphone.borrow_mut().stop();
        assert!(!microphone.borrow().may_offer());
        assert!(!microphone.borrow().ever_talking);
        assert!(microphone.borrow_mut().cleanup_complete());
    }

    #[test]
    fn explicit_source_resolution_cannot_fall_back_to_network_or_parent_paths() {
        for path in [
            "relative",
            "tcp:host",
            "/run/user/7/../native",
            "/run/with space/native",
            "/run/host:native",
        ] {
            assert!(!valid_server(Path::new(path)), "{path}");
        }
        assert!(valid_server(Path::new("/run/user/7/pulse/native")));
        assert!(!valid_server(Path::new(&format!("/{}", "x".repeat(100)))));
        assert!(
            resolve(None, || panic!("unused display must not be accessed"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn native_permission_failure_is_typed_without_device_or_audio_content() {
        assert_eq!(
            failure(mic::Error::Capture(mic::CaptureFailure::Denied)).code,
            "microphone_permission_denied"
        );
        assert_eq!(
            failure(mic::Error::Capture(mic::CaptureFailure::Discontinuity)).code,
            "microphone_capture_discontinuity"
        );
        let mut local = local();
        local.reason = Some("microphone_not_selected");
        local.stop();
        let json = local.summary(true);
        assert!(json.contains("\"microphone_capture_observed\":false"));
        assert!(json.contains("\"microphone_reason\":\"microphone_not_selected\""));
    }
}

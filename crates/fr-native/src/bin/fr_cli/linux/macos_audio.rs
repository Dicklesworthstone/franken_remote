//! Explicit Mac playback selection, the original observer installer and
//! content-free completion. No output opens during option checking or dialing.
use super::{Failure, audio};
use crate::options::AudioRequest;
use fr_wire::negotiation::{Capability, Offer, Role};
#[cfg(target_os = "macos")]
use std::{cell::RefCell, rc::Rc};

#[cfg(target_os = "macos")]
pub(super) type Report = Rc<RefCell<audio::Report>>;

/// Pure preflight, also exercised by the Linux CLI tests. The first `CoreAudio`
/// subset pins the current local default output rather than interpreting a
/// Linux socket or sink name as a Mac device.
pub(super) fn check(request: Option<&AudioRequest>, available: bool) -> Result<(), Failure> {
    let Some(request) = request else {
        return Ok(());
    };
    if !available {
        return Err(Failure::new(
            "audio_unavailable_in_build",
            "This fr was built without macos-audio; build with --features macos-desktop,macos-audio and install the matching fr-opus-worker, or omit --audio.",
            2,
        ));
    }
    if request.server.is_some() || request.sink.is_some() {
        return Err(Failure::new(
            "audio_output_selection_unsupported",
            "macOS playback pins the current local default output. Select it in System Settings before connecting; --audio-server and --audio-sink are Linux-only.",
            2,
        ));
    }
    if request.volume > 100 {
        return Err(Failure::new(
            "invalid_audio_volume",
            "Use --audio-volume from 0 to 100.",
            2,
        ));
    }
    Ok(())
}

/// Check the actual bounded profile before host lookup or any native picker.
/// A single file direction uses the same count for upload or download.
pub(super) fn check_profile(
    control: bool,
    clipboard: bool,
    file_directions: u32,
    requested: bool,
) -> Result<(), Failure> {
    if !requested {
        return Ok(());
    }
    let original = if control {
        match file_directions {
            0 => fr_client::native::control_offer_with_auxiliary(clipboard, false, false),
            1 => fr_client::native::control_offer_with_auxiliary(clipboard, true, false),
            2 => fr_client::native::control_offer_with_duplex(clipboard, false),
            _ => return Err(invalid_offer()),
        }
    } else {
        fr_client::native::observation_offer()
    };
    offer(original, true).map(|_| ())
}

#[cfg(target_os = "macos")]
pub(super) fn prepare(request: Option<&AudioRequest>) -> Result<Option<Report>, Failure> {
    check(request, cfg!(feature = "macos-audio"))?;
    let Some(request) = request else {
        return Ok(None);
    };
    audio::decoder_image()?;
    Ok(Some(Rc::new(RefCell::new(audio::Report::for_request(
        request,
    )))))
}

/// Add only the two optional playback boundaries. Keep file directions,
/// approval/input gates, role and resource limits from the original offer.
pub(super) fn offer(mut original: Offer, requested: bool) -> Result<Offer, Failure> {
    if requested {
        for (name, version) in [
            (fr_wire::audio::CAPABILITY, fr_wire::audio::VERSION),
            (
                fr_wire::audio_control::CAPABILITY,
                fr_wire::audio_control::VERSION,
            ),
        ] {
            if name == fr_wire::audio_control::CAPABILITY && original.role == Role::Observe {
                continue;
            }
            if let Some(existing) = original.capabilities.iter().find(|cap| cap.name == name) {
                if existing.version != version {
                    return Err(invalid_offer());
                }
            } else {
                original.capabilities.push(Capability {
                    name: name.into(),
                    version,
                    required: false,
                });
            }
        }
        // Reserve the Mac session's pixel-scroll and text boundaries before
        // dialing. Drop only optional diagnostics/throughput extensions; never
        // remove a requested direction, cursor or an authority/privacy gate.
        let reserved = usize::from(original.role == Role::RequestControl) * 2;
        for optional in [
            fr_wire::quality_decision::CAPABILITY,
            fr_wire::attachment::BULK_RECORD_CAPABILITY,
            fr_wire::attachment::credit::CAPABILITY,
            fr_wire::receiver_metrics::CAPABILITY,
        ] {
            if original.capabilities.len() + reserved <= fr_wire::negotiation::MAX_CAPABILITIES {
                break;
            }
            original
                .capabilities
                .retain(|cap| cap.name != optional || cap.required);
        }
        if original.capabilities.len() + reserved > fr_wire::negotiation::MAX_CAPABILITIES {
            return Err(Failure::new(
                "audio_feature_limit",
                "The selected Mac playback, clipboard and file profile exceeds the negotiated capability bound. Use --no-clipboard or select one file direction with --audio; cursor and input safety capabilities are retained.",
                2,
            ));
        }
        original.capabilities.sort_by(|a, b| a.name.cmp(&b.name));
    }
    original.validate().map_err(|_| invalid_offer())?;
    Ok(original)
}

fn invalid_offer() -> Failure {
    Failure::new(
        "invalid_native_configuration",
        "The requested native capabilities exceed the admitted profile; select fewer optional features.",
        2,
    )
}

#[cfg(target_os = "macos")]
pub(super) fn install(
    session: fr_native::macos::session::Session,
    report: Option<&Report>,
) -> fr_native::macos::session::Session {
    #[cfg(feature = "macos-audio")]
    if let Some(report) = report {
        let report = report.clone();
        return session.with_audio_output(move |observer| {
            let volume = report.borrow_mut().begin_attempt();
            let (output, cleanup) = audio::macos_output(&report, volume.clone());
            if let Err(error) = observer.configure_audio(output) {
                let reason = match error {
                    frd::session_startup::ViewerAudioUnavailable::NotSelected => {
                        "host_did_not_offer"
                    }
                    frd::session_startup::ViewerAudioUnavailable::AlreadyConfigured => {
                        "audio_already_configured"
                    }
                };
                report.borrow_mut().absent(reason);
            }
            Box::new(Playback {
                cleanup,
                report: report.clone(),
                volume,
            })
        });
    }
    #[cfg(not(feature = "macos-audio"))]
    let _ = report;
    session
}

#[cfg(all(target_os = "macos", feature = "macos-audio"))]
struct Playback {
    cleanup: audio::Retirement,
    report: Report,
    volume: fr_client::audio::AudioVolumeControl,
}

#[cfg(all(target_os = "macos", feature = "macos-audio"))]
impl fr_native::macos::session::AudioRetirement for Playback {
    fn stop(&self) {
        self.cleanup.stop();
    }
    fn is_complete(&self) -> bool {
        self.cleanup.is_complete()
    }
    fn view(&self) -> fr_native::macos::session::AudioView {
        use fr_native::macos::session::{AudioState, AudioView};
        let report = self.report.borrow();
        let state = if report.absence.is_some() {
            AudioState::Unavailable
        } else if report.active() {
            AudioState::Playing
        } else {
            AudioState::Starting
        };
        let (volume, muted) = self.volume.snapshot();
        // AudioVolumeControl stores only finite gain in 0..=1.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let volume_percent = (volume * 100.0).round() as u8;
        AudioView {
            state,
            volume_percent,
            muted,
        }
    }
    fn preferences(&self, volume_percent: u8, muted: bool) {
        preferences(&self.volume, volume_percent, muted);
    }
    fn controls_failed(&self) {
        self.cleanup.stop();
        self.report
            .borrow_mut()
            .absent("audio_controls_unavailable");
    }
}

#[cfg(any(test, all(target_os = "macos", feature = "macos-audio")))]
fn preferences(original: &fr_client::audio::AudioVolumeControl, volume_percent: u8, muted: bool) {
    let mut original = original.clone();
    original.set_volume(f32::from(volume_percent.min(100)) / 100.0);
    original.set_muted(muted);
}

fn evidence(report: Option<&audio::Report>) -> crate::ControlHistory {
    let requested = report.is_some();
    let (volume, muted) = report.map_or((1.0, false), |report| report.volume.snapshot());
    let volume = f64::from(volume) * 100.0;
    let active = report.is_some_and(audio::Report::active);
    let submitted = report.map_or(0, |report| report.submitted);
    let dropped = report.map_or(0, |report| report.dropped_packets);
    let resets = report.map_or(0, |report| report.resets);
    let expired = report.map_or(0, |report| report.expired_generations);
    let failed = report.map_or(0, |report| report.failed_generations);
    let recovered = report.map_or(0, |report| report.recovered_generations);
    let first = report.and_then(|report| report.first_failure);
    let absence = report.map_or(Some("not_requested"), |report| {
        report
            .absence
            .or_else(|| (!report.acknowledged).then_some("audio_not_started"))
    });
    let code =
        |value: Option<&str>| value.map_or_else(|| "null".into(), |value| format!("\"{value}\""));
    crate::ControlHistory {
        json: format!(
            ",\"audio_requested\":{requested},\"audio_active\":{active},\"audio_frames_submitted\":{submitted},\"audio_output_resets\":{resets},\"audio_output_dropped_packets\":{dropped},\"audio_output_expired_generations\":{expired},\"audio_output_failed_generations\":{failed},\"audio_output_recovered_generations\":{recovered},\"audio_output_first_failure\":{},\"audio_absence\":{},\"audio_volume_percent\":{volume:.0},\"audio_muted\":{muted},\"audibility_proven\":false",
            code(first),
            code(absence),
        ),
        text: format!(
            " Audio: {}; {submitted} decoded frame(s) submitted to CoreAudio, {dropped} packets dropped before native custody, {resets} output resets; {expired} expired and {failed} failed generation(s), {recovered} recovered. Volume {volume:.0}%, muted {muted}. Native submission does not prove audibility.",
            absence.unwrap_or(if active { "submitting" } else { "configured" }),
        ),
    }
}

pub(super) fn preserve(report: Option<&audio::Report>, failure: &mut Failure) {
    if failure
        .control_history
        .is_some_and(|prior| prior.json.contains(",\"audio_requested\":"))
    {
        return;
    }
    let next = evidence(report);
    let (mut json, mut text) = failure.control_history.map_or_else(
        || (String::new(), String::new()),
        |prior| (prior.json.clone(), prior.text.clone()),
    );
    json.push_str(&next.json);
    text.push_str(&next.text);
    failure.control_history = Some(Box::leak(Box::new(crate::ControlHistory { json, text })));
}

pub(super) fn finish(
    report: Option<&audio::Report>,
    result: Result<String, Failure>,
    json: bool,
) -> Result<String, Failure> {
    match result {
        Err(mut failure) => {
            preserve(report, &mut failure);
            Err(failure)
        }
        Ok(text) if json => {
            let Some(prefix) = text.trim_end().strip_suffix('}') else {
                let mut failure = Failure::new(
                    "native_output_invalid",
                    "The original audio completion could not be formatted; its retained counters remain in this failure.",
                    1,
                );
                preserve(report, &mut failure);
                return Err(failure);
            };
            Ok(format!("{prefix}{}}}\n", evidence(report).json))
        }
        Ok(mut text) => {
            text.push_str(&evidence(report).text);
            text.push('\n');
            Ok(text)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> AudioRequest {
        AudioRequest {
            server: None,
            sink: None,
            volume: 37,
            muted: true,
        }
    }

    #[test]
    fn default_output_requires_explicit_enable_and_supported_build() {
        assert!(check(None, false).is_ok());
        assert_eq!(
            check(Some(&request()), false).unwrap_err().code,
            "audio_unavailable_in_build"
        );
        assert!(check(Some(&request()), true).is_ok());
        for source in [true, false] {
            let mut request = request();
            if source {
                request.server = Some("/unused".into());
            } else {
                request.sink = Some("speakers".into());
            }
            assert_eq!(
                check(Some(&request), true).unwrap_err().code,
                "audio_output_selection_unsupported"
            );
        }
    }

    #[test]
    fn local_mute_and_volume_preserve_the_original_attempt_boundary() {
        let mut report = audio::Report::for_request(&request());
        let original = report.begin_attempt();
        preferences(&original, 20, true);
        assert_eq!(report.volume.snapshot(), (0.2, true));
        let successor = report.begin_attempt();
        preferences(&original, 100, false);
        assert_eq!(successor.snapshot(), (0.2, true));
        preferences(&successor, 255, false);
        assert_eq!(report.volume.snapshot(), (1.0, false));
        assert!(!report.acknowledged);
        assert_eq!(report.submitted, 0, "preferences do not prove playback");
    }

    #[test]
    fn optional_audio_preserves_requested_directions_cursor_and_input_gates() {
        for base in [
            fr_client::native::observation_offer(),
            fr_client::native::control_offer(),
            fr_client::native::control_offer_with_clipboard(),
            fr_client::native::control_offer_with_auxiliary(true, true, false),
            fr_client::native::control_offer_with_duplex(false, false),
        ] {
            assert_eq!(offer(base.clone(), false).unwrap(), base);
            let selected = offer(base.clone(), true).unwrap();
            assert_eq!(selected.role, base.role);
            assert_eq!(selected.limits, base.limits);
            for original in &base.capabilities {
                if original.required
                    || !matches!(
                        original.name.as_str(),
                        fr_wire::quality_decision::CAPABILITY
                            | fr_wire::attachment::BULK_RECORD_CAPABILITY
                            | fr_wire::attachment::credit::CAPABILITY
                            | fr_wire::receiver_metrics::CAPABILITY
                    )
                {
                    assert!(selected.capabilities.contains(original));
                }
            }
            assert!(
                selected
                    .capabilities
                    .iter()
                    .any(|cap| cap.name == fr_wire::audio::CAPABILITY && !cap.required)
            );
            assert_eq!(
                selected
                    .capabilities
                    .iter()
                    .any(|cap| cap.name == fr_wire::audio_control::CAPABILITY),
                base.role == Role::RequestControl
            );
            assert!(
                !selected
                    .capabilities
                    .iter()
                    .any(|cap| cap.name == fr_wire::audio_up::CAPABILITY)
            );
            let selected = fr_wire::control::with_pixel_scroll(selected)
                .and_then(fr_wire::control::with_committed_text);
            if base.role == Role::RequestControl {
                assert!(
                    selected.is_ok(),
                    "full Mac control offer remains within its bound"
                );
            }
        }
        let fullest = fr_client::native::control_offer_with_duplex(true, false);
        assert_eq!(offer(fullest.clone(), false).unwrap(), fullest);
        assert_eq!(
            offer(fullest, true).unwrap_err().code,
            "audio_feature_limit"
        );
        for directions in 0..=2 {
            for clipboard in [false, true] {
                assert!(check_profile(true, clipboard, directions, false).is_ok());
                let requested = check_profile(true, clipboard, directions, true);
                assert_eq!(requested.is_err(), clipboard && directions == 2);
            }
        }
    }

    #[test]
    fn completion_keeps_volume_mute_and_real_effects_through_later_failure() {
        let mut report = audio::Report::for_request(&request());
        report.acknowledged = true;
        report.submitted = 7;
        let output = finish(
            Some(&report),
            Ok("{\"outcome\":\"success\"}\n".into()),
            true,
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(value["audio_frames_submitted"], 7);
        assert_eq!(value["audio_volume_percent"], 37);
        assert_eq!(value["audio_muted"], true);
        assert_eq!(value["audibility_proven"], false);
        let mut failed = Failure::new("native_cleanup_incomplete", "original cleanup", 1);
        preserve(Some(&report), &mut failed);
        preserve(None, &mut failed);
        assert_eq!(failed.code, "native_cleanup_incomplete");
        assert!(
            failed
                .control_history
                .unwrap()
                .json
                .contains("\"audio_frames_submitted\":7")
        );
    }
}

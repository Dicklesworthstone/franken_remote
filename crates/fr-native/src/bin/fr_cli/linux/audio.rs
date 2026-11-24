//! `fr connect --audio`: host playback in either viewing role into the selected local
//! native output. Host Opus is decoded only in the separate per-epoch
//! fr-opus-worker, never on the session thread or by an in-process fallback.
//! Configuration acknowledgement waits for BOTH real native owners. PCM replies
//! retain their original arrival/device deadlines; a receipt is not audibility.
//! The CLI uses a bounded output worker: no device call or native teardown
//! runs on the session thread. Linux requires seccomp; macOS retains a private
//! same-user child with parent-exit binding, without an OS sandbox claim.
use super::super::{Failure, options::AudioRequest};
use std::path::PathBuf;

pub(super) use fr_native::audio_output::Report;

pub(super) fn report(request: &AudioRequest) -> Report {
    Report::new(f32::from(request.volume) / 100.0, request.muted)
}

/// The view-only offer: the optional audio-down capability only with `--audio`.
#[cfg(target_os = "linux")]
pub(super) fn observation_offer(audio: bool) -> fr_wire::negotiation::Offer {
    if audio {
        fr_client::native::observation_offer_with_audio()
    } else {
        fr_client::native::observation_offer()
    }
}

/// An explicit local request only: a build without playback and a missing
/// local output socket are typed refusals before any connection attempt.
#[cfg(target_os = "linux")]
pub(super) fn resolve(
    request: Option<&AudioRequest>,
) -> Result<Option<(PathBuf, AudioRequest)>, Failure> {
    let Some(request) = request else {
        return Ok(None);
    };
    if !cfg!(feature = "linux-audio") {
        return Err(Failure::new(
            "audio_unavailable_in_build",
            "This fr was built without the linux-audio feature; rebuild with --features linux-desktop,linux-audio or omit --audio.",
            2,
        ));
    }
    decoder_image()?;
    Ok(Some((server(request)?, request.clone())))
}

/// The codec image is installed beside fr, never selected by a host record,
/// environment search path or downloaded on demand. Missing is a typed refusal.
pub(super) fn decoder_image() -> Result<PathBuf, Failure> {
    std::env::current_exe().ok().and_then(|exe| exe.parent().map(|p| p.join("fr-opus-worker")))
        .filter(|p| p.is_absolute() && p.is_file())
        .ok_or_else(|| Failure::new("audio_decoder_unavailable",
            "--audio requires the matching fr-opus-worker installed beside fr; build it with the native Opus process feature. No in-process decoder fallback was selected.", 2))
}

/// Resolve the local server socket once, before dialing: flag, then
/// `PULSE_SERVER`, then the user runtime dir. Missing is a typed refusal.
#[cfg(target_os = "linux")]
fn server(request: &AudioRequest) -> Result<PathBuf, Failure> {
    let server = request.server.clone().or_else(|| {
        std::env::var("PULSE_SERVER")
            .ok()
            .map(|v| PathBuf::from(v.strip_prefix("unix:").unwrap_or(&v)))
            .or_else(|| {
                std::env::var_os("XDG_RUNTIME_DIR")
                    .map(|dir| PathBuf::from(dir).join("pulse").join("native"))
            })
    });
    server
        .filter(|p| p.is_absolute() && p.exists())
        .ok_or_else(|| {
            Failure::new(
                "audio_output_unavailable",
                "--audio needs the local PulseAudio server socket: pass --audio-server /absolute/path/native (or set PULSE_SERVER / XDG_RUNTIME_DIR), or omit --audio.",
                2,
            )
        })
}

#[cfg(any(
    all(target_os = "linux", feature = "linux-audio"),
    all(target_os = "macos", feature = "macos-audio")
))]
pub(super) use fr_native::audio_output::Output;
#[cfg(all(target_os = "macos", feature = "macos-audio"))]
pub(super) use fr_native::audio_output::Retirement;

/// Build the output for one attempt; `None` when this build has no audio.
#[cfg(all(target_os = "linux", feature = "linux-audio"))]
pub(super) fn output(
    server: PathBuf,
    request: &AudioRequest,
    report: &std::rc::Rc<std::cell::RefCell<Report>>,
    volume: fr_client::audio::AudioVolumeControl,
) -> Box<dyn frd::session_startup::ViewerAudioOutput> {
    Box::new(Output::new(
        server,
        request.sink.clone(),
        report.clone(),
        volume,
    ))
}

/// The same bounded proxy and decoder ownership, selecting only the Mac's
/// current local default output. The caller retains the real cleanup witness.
#[cfg(all(target_os = "macos", feature = "macos-audio"))]
pub(super) fn macos_output(
    report: &std::rc::Rc<std::cell::RefCell<Report>>,
    volume: fr_client::audio::AudioVolumeControl,
) -> (Box<dyn frd::session_startup::ViewerAudioOutput>, Retirement) {
    let output = Output::macos(report.clone(), volume);
    let retirement = output.retirement();
    (Box::new(output), retirement)
}

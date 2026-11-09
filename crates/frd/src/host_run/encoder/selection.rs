//! Local encoder choices and the bounded hardware selection policy. The caller
//! supplies the real startup probe; selecting a name alone proves nothing.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoder {
    SoftwareExplicit,
    Nvenc,
    Vaapi,
    /// Apple's hardware HEVC encoder in the macOS capture worker.
    #[cfg(target_os = "macos")]
    VideoToolbox,
}
impl Encoder {
    /// Exact local names, never peer input or arbitrary `FFmpeg` codec names.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "software" => Some(Self::SoftwareExplicit),
            "nvenc" => Some(Self::Nvenc),
            "vaapi" => Some(Self::Vaapi),
            #[cfg(target_os = "macos")]
            "videotoolbox" => Some(Self::VideoToolbox),
            _ => None,
        }
    }
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SoftwareExplicit => "software",
            Self::Nvenc => "nvenc",
            Self::Vaapi => "vaapi",
            #[cfg(target_os = "macos")]
            Self::VideoToolbox => "videotoolbox",
        }
    }
    pub const fn is_hardware(self) -> bool {
        !matches!(self, Self::SoftwareExplicit)
    }
}

/// A startup request, distinct from the concrete backend retained by every
/// share. Automatic hardware selection must be explicitly requested locally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selection {
    Exact(Encoder),
    AutomaticHardware,
}
impl Selection {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Self::AutomaticHardware),
            _ => Encoder::parse(value).map(Self::Exact),
        }
    }
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Exact(encoder) => encoder.as_str(),
            Self::AutomaticHardware => "auto",
        }
    }
    pub const fn is_hardware(self) -> bool {
        match self {
            Self::Exact(encoder) => encoder.is_hardware(),
            Self::AutomaticHardware => true,
        }
    }
}
impl From<Encoder> for Selection {
    fn from(encoder: Encoder) -> Self {
        Self::Exact(encoder)
    }
}

pub(crate) enum Attempt {
    Ready,
    /// A completed probe's bounded, content-free refusal. A failed launch,
    /// timeout or uncertain cleanup is an error, not permission to try again.
    Refused(String),
}

/// A worker's success reply must describe geometry admitted by the current
/// protocol limits before the host can report a successful startup IDR.
pub(crate) fn valid_dimensions(
    width: u32,
    height: u32,
    max_dimension: u32,
    max_pixels: u64,
) -> bool {
    width != 0
        && height != 0
        && width <= max_dimension
        && height <= max_dimension
        && u64::from(width) * u64::from(height) <= max_pixels
}
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Failure<E> {
    Unavailable(Vec<(Encoder, String)>),
    InvalidReply { encoder: Encoder, reason: String },
    Probe(E),
    Stopped,
}

/// At most two probes, each with the caller's fixed deadline. A timeout or
/// uncollected child stops selection. Software is never an automatic candidate.
pub(crate) fn resolve<E>(
    selection: Selection,
    mut stopped: impl FnMut() -> bool,
    mut probe: impl FnMut(Encoder) -> Result<Attempt, E>,
) -> Result<Encoder, Failure<E>> {
    if stopped() {
        return Err(Failure::Stopped);
    }
    let candidates: &[Encoder] = match &selection {
        Selection::Exact(Encoder::SoftwareExplicit) => return Ok(Encoder::SoftwareExplicit),
        Selection::Exact(encoder) => std::slice::from_ref(encoder),
        #[cfg(not(target_os = "macos"))]
        Selection::AutomaticHardware => &[Encoder::Nvenc, Encoder::Vaapi],
        #[cfg(target_os = "macos")]
        Selection::AutomaticHardware => &[Encoder::VideoToolbox],
    };
    let mut refused = Vec::with_capacity(candidates.len());
    for &encoder in candidates {
        if stopped() {
            return Err(Failure::Stopped);
        }
        // A cleanup/launch failure remains an error even if Stop arrived while
        // the probe ran. A cleanly collected result can then acknowledge Stop.
        let attempt = probe(encoder).map_err(Failure::Probe)?;
        if stopped() {
            return Err(Failure::Stopped);
        }
        match attempt {
            Attempt::Ready => return Ok(encoder),
            Attempt::Refused(reason)
                if reason.is_empty()
                    || matches!(
                        reason.as_str(),
                        "no_probe_reply" | "malformed_probe_reply" | "unnamed"
                    ) =>
            {
                return Err(Failure::InvalidReply { encoder, reason });
            }
            Attempt::Refused(reason) => refused.push((encoder, reason)),
        }
    }
    Err(Failure::Unavailable(refused))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_success_requires_nonzero_bounded_geometry_and_pixel_product() {
        assert!(valid_dimensions(1920, 1080, 8192, 16_777_216));
        assert!(valid_dimensions(4096, 4096, 8192, 16_777_216));
        for (width, height) in [
            (0, 1080),
            (1920, 0),
            (8193, 1),
            (1, 8193),
            (8192, 8192),
            (u32::MAX, u32::MAX),
        ] {
            assert!(!valid_dimensions(width, height, 8192, 16_777_216));
        }
    }

    #[test]
    fn automatic_is_an_explicit_request_and_never_a_concrete_backend() {
        assert_eq!(Selection::parse("auto"), Some(Selection::AutomaticHardware));
        assert_eq!(Encoder::parse("auto"), None);
        assert_eq!(Selection::AutomaticHardware.as_str(), "auto");
        assert!(Selection::AutomaticHardware.is_hardware());
        for encoder in [Encoder::SoftwareExplicit, Encoder::Nvenc, Encoder::Vaapi] {
            assert_eq!(Selection::parse(encoder.as_str()), Some(encoder.into()));
            assert_eq!(
                Selection::from(encoder).is_hardware(),
                encoder.is_hardware()
            );
        }
        for name in [
            "",
            "AUTO",
            "hardware",
            "auto ",
            " auto",
            "nvenc,vaapi",
            "h264",
        ] {
            assert_eq!(Selection::parse(name), None);
        }
    }

    #[test]
    fn completed_first_refusal_selects_the_second_real_hardware_probe() {
        let mut calls = Vec::new();
        let selected = resolve::<()>(
            Selection::AutomaticHardware,
            || false,
            |encoder| {
                calls.push(encoder);
                Ok(if encoder == Encoder::Nvenc {
                    Attempt::Refused("encoder_unavailable".into())
                } else {
                    Attempt::Ready
                })
            },
        );
        assert_eq!(selected, Ok(Encoder::Vaapi));
        assert_eq!(calls, [Encoder::Nvenc, Encoder::Vaapi]);
    }

    #[test]
    fn first_success_stops_probing_and_retains_a_concrete_backend() {
        let mut calls = Vec::new();
        let selected = resolve::<()>(
            Selection::AutomaticHardware,
            || false,
            |encoder| {
                calls.push(encoder);
                Ok(Attempt::Ready)
            },
        );
        assert_eq!(selected, Ok(Encoder::Nvenc));
        assert_eq!(calls, [Encoder::Nvenc]);
    }

    #[test]
    fn both_refusals_are_retained_and_software_is_never_tried() {
        let mut calls = Vec::new();
        let selected = resolve::<()>(
            Selection::AutomaticHardware,
            || false,
            |encoder| {
                calls.push(encoder);
                Ok(Attempt::Refused(format!(
                    "{}_unavailable",
                    encoder.as_str()
                )))
            },
        );
        assert_eq!(calls, [Encoder::Nvenc, Encoder::Vaapi]);
        assert_eq!(
            selected,
            Err(Failure::Unavailable(vec![
                (Encoder::Nvenc, "nvenc_unavailable".into()),
                (Encoder::Vaapi, "vaapi_unavailable".into()),
            ]))
        );
    }

    #[test]
    fn exact_hardware_failure_cannot_substitute_another_backend() {
        for encoder in [Encoder::Nvenc, Encoder::Vaapi] {
            let mut calls = Vec::new();
            assert_eq!(
                resolve::<()>(
                    encoder.into(),
                    || false,
                    |selected| {
                        calls.push(selected);
                        Ok(Attempt::Refused("unavailable".into()))
                    }
                ),
                Err(Failure::Unavailable(vec![(encoder, "unavailable".into())]))
            );
            assert_eq!(calls, [encoder]);
        }
    }

    #[test]
    fn launch_timeout_and_cleanup_errors_stop_without_trying_another_backend() {
        for error in ["launch_failed", "timed_out", "reap_failed"] {
            let mut calls = Vec::new();
            assert_eq!(
                resolve(
                    Selection::AutomaticHardware,
                    || false,
                    |encoder| {
                        calls.push(encoder);
                        Err(error)
                    }
                ),
                Err(Failure::Probe(error))
            );
            assert_eq!(calls, [Encoder::Nvenc]);
        }
    }

    #[test]
    fn explicit_software_keeps_its_existing_no_probe_behavior() {
        assert_eq!(
            resolve::<()>(
                Encoder::SoftwareExplicit.into(),
                || false,
                |_| { panic!("the explicit software profile must not start a hardware probe") }
            ),
            Ok(Encoder::SoftwareExplicit)
        );
    }
    #[test]
    fn stop_after_a_collected_result_never_starts_a_second_probe_or_selects_a_winner() {
        use std::cell::Cell;
        for ready in [true, false] {
            let stopped = Cell::new(false);
            let mut calls = Vec::new();
            let result = resolve::<()>(
                Selection::AutomaticHardware,
                || stopped.get(),
                |encoder| {
                    calls.push(encoder);
                    stopped.set(true);
                    Ok(if ready {
                        Attempt::Ready
                    } else {
                        Attempt::Refused("unavailable".into())
                    })
                },
            );
            assert_eq!(result, Err(Failure::Stopped));
            assert_eq!(calls, [Encoder::Nvenc]);
        }
        assert_eq!(
            resolve::<()>(
                Selection::AutomaticHardware,
                || true,
                |_| { panic!("a stopped host must not launch a capture probe") }
            ),
            Err(Failure::Stopped)
        );
    }

    #[test]
    fn a_stop_cannot_hide_uncertain_probe_cleanup() {
        use std::cell::Cell;
        let stopped = Cell::new(false);
        let result = resolve(
            Selection::AutomaticHardware,
            || stopped.get(),
            |_| {
                stopped.set(true);
                Err("reap_failed")
            },
        );
        assert_eq!(result, Err(Failure::Probe("reap_failed")));
    }

    #[test]
    fn missing_malformed_or_unnamed_replies_cannot_trigger_a_second_capture() {
        for reason in ["", "no_probe_reply", "malformed_probe_reply", "unnamed"] {
            let mut calls = Vec::new();
            let result = resolve::<()>(
                Selection::AutomaticHardware,
                || false,
                |encoder| {
                    calls.push(encoder);
                    Ok(Attempt::Refused(reason.into()))
                },
            );
            assert_eq!(
                result,
                Err(Failure::InvalidReply {
                    encoder: Encoder::Nvenc,
                    reason: reason.into()
                })
            );
            assert_eq!(calls, [Encoder::Nvenc]);
        }
    }
}

//! Resolve the packaged desktop-consent backend without changing saved policy.
//! Missing local prerequisites are a refusal, never unattended fallback. The
//! original host validates the selected display/user and supervises logind.
use frd::{
    host_policy::{Approval, options::RunOptions},
    host_run::approval::Configuration,
};
#[cfg(not(windows))]
use std::path::Path;

#[cfg(not(any(target_os = "macos", windows)))]
const REQUIRED: &str = "local approval requires --logind-session for the shared desktop and an installed fr-observation-indicator beside frd, or the browser profile's explicit indicator path; headless sharing cannot use this consent profile";
#[cfg(target_os = "macos")]
const REQUIRED: &str = "local approval requires the installed fr-observation-indicator and the original monitored graphical login session; no unattended fallback is used";
#[cfg(windows)]
const REQUIRED: &str = "local approval requires this process's monitored interactive Windows logon session and its native consent window";

#[cfg(not(windows))]
pub fn configuration(
    options: &RunOptions,
    mode: Approval,
) -> Result<Option<Configuration>, &'static str> {
    let eligible = eligible(options);
    // A host without lifecycle evidence must not even try an ambient UI.
    let image = eligible
        .then(|| {
            let explicit = (cfg!(target_os = "macos") || options.browser_port.is_some())
                .then(|| options.observation_indicator.clone())
                .flatten();
            explicit.or_else(|| {
                std::env::current_exe().ok().and_then(|exe| {
                    exe.parent()
                        .map(|directory| directory.join("fr-observation-indicator"))
                })
            })
        })
        .flatten()
        .filter(|path| path.is_absolute() && path.is_file());
    selected(mode, eligible, image.as_deref())
}
#[cfg(windows)]
pub fn configuration(
    options: &RunOptions,
    mode: Approval,
) -> Result<Option<Configuration>, &'static str> {
    if windows_eligible(options) {
        // No window, native observation or approval is created here. The
        // original host validates this process's session and supervises its
        // logon before any admitted request may open the native prompt.
        Ok(Some(Configuration::windows()))
    } else if mode == Approval::Local {
        Err(REQUIRED)
    } else {
        Ok(None)
    }
}
#[cfg(any(windows, test))]
fn windows_eligible(options: &RunOptions) -> bool {
    !options.headless
        && options.display.is_none()
        && options.logind_session.is_none()
        && options.logind_seat.is_none()
        && options.session_monitor.is_none()
        && options.observation_indicator.is_none()
}
#[cfg(not(windows))]
fn eligible(options: &RunOptions) -> bool {
    if cfg!(target_os = "macos") {
        // The mandatory native monitor is resolved by macos_run and checked
        // against the actual inherited login in host_run::approval::validate.
        options.logind_session.is_none() && !options.headless && options.display.is_none()
    } else {
        options.logind_session.is_some()
            && !options.headless
            && (options.observation_indicator.is_none() || options.browser_port.is_some())
    }
}
#[cfg(not(windows))]
fn selected(
    mode: Approval,
    eligible: bool,
    image: Option<&Path>,
) -> Result<Option<Configuration>, &'static str> {
    let available = if eligible {
        image.and_then(|image| Configuration::new(image).ok())
    } else {
        None
    };
    if mode == Approval::Local && available.is_none() {
        return Err(REQUIRED);
    }
    // Preparing availability while policy is None opens no UI and grants no
    // approval. It lets a later saved-policy epoch switch to Local after the
    // old share is completely retired. The epoch, not this option, selects it.
    Ok(available)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(not(windows))]
    fn desktop_approval_missing_or_unmonitored_ui_never_becomes_unattended() {
        for (eligible, image) in [
            (false, None),
            (
                false,
                Some(Path::new("/installed/fr-observation-indicator")),
            ),
            (true, None),
            (true, Some(Path::new("relative-ui"))),
        ] {
            assert_eq!(
                selected(Approval::Local, eligible, image).err(),
                Some(REQUIRED)
            );
            assert!(selected(Approval::None, eligible, image).unwrap().is_none());
        }
    }

    #[test]
    #[cfg(not(windows))]
    fn desktop_approval_availability_preserves_current_and_future_policy_choices() {
        for mode in [Approval::None, Approval::Local] {
            assert!(
                selected(
                    mode,
                    true,
                    Some(Path::new("/installed/fr-observation-indicator"))
                )
                .unwrap()
                .is_some()
            );
        }
    }

    #[test]
    #[cfg(not(any(target_os = "macos", windows)))]
    fn desktop_approval_incompatible_profiles_refuse_before_ui_discovery() {
        for (headless, indicator, session) in [
            (true, None, Some("c2")),
            (
                false,
                Some("/installed/fr-observation-indicator"),
                Some("c2"),
            ),
            (false, None, None),
        ] {
            let options = RunOptions {
                headless,
                observation_indicator: indicator.map(Into::into),
                logind_session: session.map(Into::into),
                ..RunOptions::default()
            };
            assert_eq!(
                configuration(&options, Approval::Local).err(),
                Some(REQUIRED)
            );
            assert!(configuration(&options, Approval::None).unwrap().is_none());
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn browser_consent_uses_its_explicit_monitored_indicator_without_native_fallback() {
        let mut options = RunOptions {
            browser_port: Some(8444),
            logind_session: Some("c2".into()),
            observation_indicator: Some("/installed/browser-indicator".into()),
            ..RunOptions::default()
        };
        assert!(eligible(&options));
        assert!(
            selected(
                Approval::Local,
                eligible(&options),
                options.observation_indicator.as_deref()
            )
            .unwrap()
            .is_some()
        );
        options.browser_port = None;
        assert!(
            !eligible(&options),
            "native wrapper restrictions remain unchanged"
        );
        options.browser_port = Some(8444);
        options.logind_session = None;
        assert!(!eligible(&options));
        options.logind_session = Some("c2".into());
        options.headless = true;
        assert!(!eligible(&options));
        assert!(selected(Approval::Local, true, None).is_err());
    }

    #[test]
    fn windows_consent_uses_only_the_original_native_session_surface() {
        let options = RunOptions::default();
        assert!(windows_eligible(&options));
        for changed in [
            RunOptions {
                headless: true,
                ..RunOptions::default()
            },
            RunOptions {
                display: Some(":0".into()),
                ..RunOptions::default()
            },
            RunOptions {
                logind_session: Some("c2".into()),
                ..RunOptions::default()
            },
            RunOptions {
                logind_seat: Some("seat0".into()),
                ..RunOptions::default()
            },
            RunOptions {
                session_monitor: Some("C:\\other-monitor.exe".into()),
                ..RunOptions::default()
            },
            RunOptions {
                observation_indicator: Some("C:\\other-ui.exe".into()),
                ..RunOptions::default()
            },
        ] {
            assert!(!windows_eligible(&changed));
        }
    }

    #[test]
    #[cfg(windows)]
    fn windows_local_and_later_saved_policy_use_native_consent_without_a_helper() {
        for mode in [Approval::None, Approval::Local] {
            assert!(
                configuration(&RunOptions::default(), mode)
                    .unwrap()
                    .is_some()
            );
        }
        let options = RunOptions {
            headless: true,
            ..RunOptions::default()
        };
        assert_eq!(
            configuration(&options, Approval::Local).err(),
            Some(REQUIRED)
        );
        assert!(configuration(&options, Approval::None).unwrap().is_none());
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn macos_approval_keeps_the_native_indicator_and_requires_a_native_login() {
        let mut options = RunOptions::default();
        assert!(eligible(&options));
        options.observation_indicator = Some("/installed/fr-observation-indicator".into());
        assert!(eligible(&options));
        options.logind_session = Some("c2".into());
        assert!(!eligible(&options));
        options.logind_session = None;
        options.display = Some(":0".into());
        assert!(!eligible(&options));
        options.display = None;
        options.headless = true;
        assert!(!eligible(&options));
    }
}

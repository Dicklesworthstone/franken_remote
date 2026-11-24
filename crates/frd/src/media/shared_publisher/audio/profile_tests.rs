//! Source selection is local and bounded before an audio worker can start.
use super::*;

fn profile(source: Source, display: &str) -> Result<AudioProfile, Error> {
    AudioProfile::with_source(
        std::env::current_exe().unwrap(),
        display.to_owned(),
        source,
        Arc::new(|| Ok(7)),
        Arc::new(Mutex::new(None)),
    )
}

#[test]
fn endpoint_scope_never_becomes_an_x11_display_selection() {
    for source in [Source::MacSystem, Source::WindowsEndpoint] {
        assert!(matches!(
            profile(source.clone(), ":0"),
            Err(Error::InvalidBudget)
        ));
        let profile = profile(source.clone(), "").unwrap();
        assert!(profile.retired.lock().unwrap().is_none());
        let capture = profile.capture(AudioGeneration::from_raw(4));
        assert_eq!(capture.source, source);
        assert_eq!(capture.generation, AudioGeneration::from_raw(4));
        assert_eq!(capture.channels, AudioChannels::Stereo);
        capture.validate().unwrap();
    }
}

#[test]
fn windows_endpoint_launch_is_platform_specific_without_a_fallback() {
    let profile = profile(Source::WindowsEndpoint, "").unwrap();
    if cfg!(windows) {
        assert!(profile.launch(7).is_ok());
        assert!(matches!(
            profile.launch(0),
            Err(worker::Error::InvalidLaunch)
        ));
    } else {
        assert!(matches!(
            profile.launch(7),
            Err(worker::Error::InvalidLaunch)
        ));
    }
    // Constructing a profile or a Launch neither starts capture nor claims
    // cleanup for a child: demand and AudioReady retain those transitions.
    assert!(profile.retired.lock().unwrap().is_none());
}

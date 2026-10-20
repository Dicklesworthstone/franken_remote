//! Local preference sharing, not permission or physical-output qualification.
use fr_client::audio::{AudioVolumeControl, HostAudioStore};
use fr_core::audio::{AudioChannels, AudioGeneration};
use fr_media::audio::AudioPcmFrame;
use std::sync::{Arc, Barrier};

#[test]
fn cloned_controls_share_changes_but_new_connections_and_saved_values_do_not() {
    let mut local = AudioVolumeControl::new();
    let worker = local.clone();
    let independent = AudioVolumeControl::new();
    local.set_volume(0.25);
    local.set_muted(true);
    assert_eq!(worker.snapshot(), (0.25, true));
    assert_eq!(independent.snapshot(), (1.0, false));
    let mut settings = HostAudioStore::new();
    settings.save_settings("original", &local);
    local.toggle_mute();
    local.set_volume(0.75);
    let saved = settings.get_or_default("original");
    assert_eq!(saved.snapshot(), (0.25, true));
    assert_eq!(worker.snapshot(), (0.75, false));
    assert_eq!(settings.get_or_default("other").snapshot(), (1.0, false));
    assert_eq!(
        HostAudioStore::load_from_str(&settings.save_to_string())
            .get_or_default("original")
            .snapshot(),
        (0.25, true)
    );
}

#[test]
fn simultaneous_gain_and_mute_updates_never_overwrite_the_other_field() {
    let volume = AudioVolumeControl::new();
    let start = Arc::new(Barrier::new(3));
    let end = Arc::new(Barrier::new(3));
    std::thread::scope(|scope| {
        for muted_field in [false, true] {
            let mut writer = volume.clone();
            let start = &start;
            let end = &end;
            scope.spawn(move || {
                for i in 0..512 {
                    start.wait();
                    if muted_field {
                        writer.set_muted(i % 2 == 0);
                    } else {
                        writer.set_volume(if i % 2 == 0 { 0.25 } else { 0.75 });
                    }
                    end.wait();
                }
            });
        }
        for i in 0..512 {
            start.wait();
            end.wait();
            assert_eq!(
                volume.snapshot(),
                (if i % 2 == 0 { 0.25 } else { 0.75 }, i % 2 == 0)
            );
        }
    });
}

#[test]
fn racing_toggles_are_not_lost_and_a_frame_uses_one_gain_snapshot() {
    let volume = AudioVolumeControl::new();
    std::thread::scope(|scope| {
        for _ in 0..4 {
            let mut writer = volume.clone();
            scope.spawn(move || {
                for _ in 0..1001 {
                    writer.toggle_mute();
                }
            });
        }
    });
    assert_eq!(volume.snapshot(), (1.0, false));
    std::thread::scope(|scope| {
        let mut writer = volume.clone();
        scope.spawn(move || {
            for _ in 0..10_000 {
                writer.set_volume(0.25);
                writer.set_volume(0.75);
            }
        });
        for _ in 0..128 {
            let mut frame = AudioPcmFrame::from_interleaved(
                AudioGeneration::INITIAL,
                AudioChannels::Stereo,
                0,
                &[8000; 1920],
            )
            .unwrap();
            assert_eq!(volume, volume.clone());
            volume.apply_to_pcm(&mut frame);
            let first = frame.samples()[0];
            assert!([2000, 6000, 8000].contains(&first));
            assert!(frame.samples().iter().all(|sample| *sample == first));
        }
    });
}

#[test]
fn untrusted_numeric_settings_cannot_create_nonfinite_or_negative_gain() {
    for (input, expected) in [
        (f32::NAN, 0.0_f32),
        (f32::NEG_INFINITY, 0.0),
        (f32::INFINITY, 1.0),
        (-0.0, 0.0),
        (-3.0, 0.0),
        (2.0, 1.0),
        (0.5, 0.5),
    ] {
        let mut control = AudioVolumeControl::from_parts(input, false);
        assert_eq!(control.volume().to_bits(), expected.to_bits());
        assert!(!control.is_muted());
        control.set_muted(true);
        control.set_volume(input);
        assert_eq!(control.snapshot().0.to_bits(), expected.to_bits());
        assert!(control.is_muted());
        assert_eq!(control.effective_gain().to_bits(), 0.0_f32.to_bits());
    }
}

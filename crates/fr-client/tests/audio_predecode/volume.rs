//! The actual playout owner with the parent's explicit delayed-codec fixture.
use super::*;
use fr_client::audio::AudioVolumeControl;

#[test]
fn local_mute_changes_after_predecode_are_sampled_only_at_actual_submission() {
    for channels in [AudioChannels::Mono, AudioChannels::Stereo] {
        let mut c = Case::new(10, channels);
        let mut control = AudioVolumeControl::from_parts(0.25, false);
        *c.owner.volume_mut() = control.clone();
        c.first();
        c.trace.delay.set(12_000);
        assert_eq!(c.render(21).unwrap(), PlayoutResult::Waiting);
        control.set_muted(true);
        c.trace.now.set(33_000);
        let result = c.owner.render(
            || Ok(clock(33)),
            |pcm, receipt| {
                assert!(pcm.samples().iter().all(|s| *s == 0));
                assert_eq!(receipt.sequence, 1);
                assert_eq!(receipt.valid_until, ClientInstant(100_000));
                assert_eq!(receipt.output_samples, 30 * 48);
                Ok(())
            },
        );
        assert!(matches!(result, Ok(PlayoutResult::Submitted(_))));
        assert_eq!(c.trace.packets.get(), 2);
        assert_eq!(c.trace.plc.get(), 0);
    }
}

#[test]
fn changes_inside_the_final_permission_check_apply_without_replaying_decode() {
    let mut c = Case::new(10, AudioChannels::Stereo);
    c.first();
    c.render(21).unwrap();
    let mut control = c.owner.volume_mut().clone();
    let mut checks = 0;
    c.trace.now.set(30_000);
    assert!(matches!(
        c.owner.render(
            || {
                checks += 1;
                if checks == 2 {
                    control.set_volume(0.25);
                }
                Ok(clock(30))
            },
            |pcm, _| {
                assert!(pcm.samples().iter().all(|s| *s == 2000));
                Ok(())
            }
        ),
        Ok(PlayoutResult::Submitted(_))
    ));
    assert_eq!(checks, 2);
    assert_eq!(c.trace.packets.get(), 2);
}

#[test]
fn volume_handles_cannot_revive_stopped_or_permission_refused_playout() {
    for deny in [false, true] {
        let mut c = Case::new(10, AudioChannels::Mono);
        c.first();
        let mut control = c.owner.volume_mut().clone();
        if !deny {
            c.owner.stop();
        }
        control.set_volume(1.0);
        control.set_muted(false);
        assert!(
            c.owner
                .render(
                    || {
                        if deny {
                            Err(PlayoutError::Output)
                        } else {
                            Ok(clock(30))
                        }
                    },
                    |_, _| panic!("a preference is not playback permission")
                )
                .is_err()
        );
        assert_eq!(c.owner.queued_packets(), 0);
        assert_eq!(c.trace.dropped.get(), 1);
    }
}

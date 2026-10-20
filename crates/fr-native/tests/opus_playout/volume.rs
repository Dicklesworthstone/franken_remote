//! Real Opus history and PCM gain, with a fixture output clock and sink.
use super::*;
use fr_client::audio::AudioVolumeControl;

#[test]
fn shared_local_controls_change_real_opus_output_without_resetting_codec_history() {
    for channels in [AudioChannels::Mono, AudioChannels::Stereo] {
        for duration in [10, 20] {
            let offer = offer(AudioDirection::Downlink, channels, duration, duration);
            let control = AudioVolumeControl::new();
            let mut receiver = OpusPlayout::new(BINDING, offer, clock(0)).unwrap();
            *receiver.volume_mut() = control.clone();
            let mut reference = Decoder::new();
            reference.configure(config(offer)).unwrap();
            for packet in packets(offer, 16) {
                let time = packet.sequence() * u64::from(duration);
                receiver
                    .receive_record(&wire(&packet, BINDING), clock(time))
                    .unwrap();
                reference.submit_packet(&packet).unwrap();
                let mut expected = reference.poll_pcm().unwrap().unwrap();
                let muted = packet.sequence() % 4 == 1;
                let gain = if packet.sequence() % 4 == 2 {
                    0.25
                } else {
                    1.0
                };
                // Only the preference crosses threads; the actual libopus state
                // remains confined to this native owner.
                let mut shell = control.clone();
                std::thread::spawn(move || {
                    shell.set_volume(gain);
                    shell.set_muted(muted);
                })
                .join()
                .unwrap();
                // Independent scalar oracle: quarter gain rounds half away
                // from zero; the gain implementation is not its own reference.
                for sample in expected.samples_mut() {
                    let value = i32::from(*sample);
                    *sample = if muted {
                        0
                    } else if packet.sequence() % 4 == 2 {
                        i16::try_from(value.signum() * ((value.abs() + 2) / 4)).unwrap()
                    } else {
                        *sample
                    };
                }
                let due = clock(time + u64::from(duration));
                let result = receiver
                    .render(
                        || Ok(due),
                        |pcm, receipt| {
                            assert_eq!(pcm.samples(), expected.samples());
                            assert_eq!(receipt.sequence, packet.sequence());
                            assert!(!receipt.concealed);
                            assert_eq!(receipt.source_samples, packet.timestamp_samples());
                            Ok(())
                        },
                    )
                    .unwrap();
                assert!(matches!(result, PlayoutResult::Submitted(_)));
            }
            assert!(receiver.error().is_none());
            receiver.stop();
            let mut stale = control.clone();
            stale.set_muted(false);
            assert!(
                receiver
                    .render(|| Ok(clock(400)), |_, _| panic!("retired output"))
                    .is_err()
            );
        }
    }
}

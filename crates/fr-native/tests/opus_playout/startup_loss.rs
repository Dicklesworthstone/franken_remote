//! Lost startup packets through actual Opus encode -> wire -> jitter -> playout.
//! Clocks, loss and the PCM sink are fixtures. The reference is another instance
//! of libopus, proving history/order, not independent codec or speaker quality.
use super::*;

#[test]
fn every_bounded_startup_loss_pattern_preserves_native_history_without_a_restart() {
    for duration in [10, 20] {
        for channels in [AudioChannels::Mono, AudioChannels::Stereo] {
            let offer = offer(AudioDirection::Downlink, channels, duration, 40);
            let encoded = packets(offer, 20);
            let loss_bits = 80 / duration;
            for loss in 0_u16..(1 << loss_bits) {
                let mut playout = OpusPlayout::new(BINDING, offer, clock(0)).unwrap();
                let mut reference = Decoder::new();
                reference.configure(config(offer)).unwrap();
                let missing = |sequence: u64| {
                    sequence > 0
                        && sequence <= u64::from(loss_bits)
                        && loss & (1 << (sequence - 1)) != 0
                };
                let mut outputs = 0_usize;
                let mut concealed = 0;
                let last_due = 40 + 19 * u64::from(duration);
                for ms in 0..=last_due {
                    if ms % u64::from(duration) == 0 {
                        let sequence = ms / u64::from(duration);
                        if sequence < encoded.len() as u64 && !missing(sequence) {
                            assert_eq!(
                                playout
                                    .receive_record(
                                        &wire(
                                            &encoded[usize::try_from(sequence).unwrap()],
                                            BINDING
                                        ),
                                        clock(ms)
                                    )
                                    .unwrap(),
                                ReceiveResult::Queued
                            );
                        }
                    }
                    let result = playout
                        .render(
                            || Ok(clock(ms)),
                            |pcm, receipt| {
                                assert_eq!(receipt.sequence, outputs as u64);
                                assert_eq!(
                                    receipt.output_samples,
                                    clock(40 + receipt.sequence * u64::from(duration))
                                        .output_samples
                                );
                                assert_eq!(receipt.concealed, missing(receipt.sequence));
                                let expected = if receipt.concealed {
                                    concealed += 1;
                                    reference.decode_plc(duration * 48).unwrap()
                                } else {
                                    reference.submit_packet(&encoded[outputs]).unwrap();
                                    reference.poll_pcm().unwrap().unwrap()
                                };
                                assert_eq!(pcm.timestamp_samples(), expected.timestamp_samples());
                                assert!(
                                    pcm.samples() == expected.samples(),
                                    "native PCM history differs"
                                );
                                outputs += 1;
                                Ok(())
                            },
                        )
                        .unwrap();
                    if ms < 40 {
                        assert_eq!(result, PlayoutResult::Waiting);
                        assert_eq!(outputs, 0, "no early decode/output to defeat reordering");
                    }
                }
                assert_eq!(outputs, encoded.len());
                assert_eq!(concealed, loss.count_ones(), "no extra startup concealment");
            }
        }
    }
}

#[test]
fn first_real_packet_does_not_invent_native_history_before_its_sequence() {
    for channels in [AudioChannels::Mono, AudioChannels::Stereo] {
        let offer = offer(AudioDirection::Downlink, channels, 10, 40);
        let encoded = packets(offer, 9);
        let mut playout = OpusPlayout::new(BINDING, offer, clock(0)).unwrap();
        let mut reference = Decoder::new();
        reference.configure(config(offer)).unwrap();
        playout
            .receive_record(&wire(&encoded[7], BINDING), clock(0))
            .unwrap();
        assert_eq!(
            playout
                .render(|| Ok(clock(39)), |_, _| panic!("early output"))
                .unwrap(),
            PlayoutResult::Waiting
        );
        let result = playout
            .render(
                || Ok(clock(40)),
                |pcm, receipt| {
                    assert_eq!(receipt.sequence, 7);
                    assert!(!receipt.concealed);
                    reference.submit_packet(&encoded[7]).unwrap();
                    let expected = reference.poll_pcm().unwrap().unwrap();
                    assert!(
                        pcm.samples() == expected.samples(),
                        "no concealment before first native packet"
                    );
                    Ok(())
                },
            )
            .unwrap();
        assert!(matches!(result, PlayoutResult::Submitted(_)));
    }
}

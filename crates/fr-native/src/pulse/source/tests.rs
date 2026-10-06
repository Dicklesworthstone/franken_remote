//! Pure production accumulator/scratch checks, not PulseAudio or codec evidence.
use super::*;

#[test]
fn overload_retains_newest_frames_in_order_without_changing_timestamps() {
    let mut recent = Recent::new();
    for sequence in 0_u16..20 {
        recent
            .push(&[i16::try_from(sequence).unwrap(); 4], u64::from(sequence) * 960)
            .unwrap();
    }
    let kept: Vec<_> = recent.iter().map(|(pcm, time)| (pcm[0], time)).collect();
    assert_eq!(
        kept,
        [(16, 15_360), (17, 16_320), (18, 17_280), (19, 18_240)]
    );
    assert_eq!(recent.dropped(), 16);
}

#[test]
fn byte_capacity_is_fixed_and_invalid_frames_do_not_displace_good_work() {
    let mut recent = Recent::new();
    recent.push(&[9; MAX_FRAME_VALUES], 12).unwrap();
    assert_eq!(recent.push(&[], 13), Err(Error::BufferLimit));
    assert_eq!(
        recent.push(&[1; MAX_FRAME_VALUES + 1], 14),
        Err(Error::BufferLimit)
    );
    let kept: Vec<_> = recent.iter().collect();
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0], (&[9; MAX_FRAME_VALUES][..], 12));
    assert_eq!(recent.dropped(), 0);
}

#[test]
fn completed_frames_never_survive_a_pull_reset() {
    let mut recent = Recent::new();
    for timestamp in 0..10 {
        recent.push(&[7; MAX_FRAME_VALUES], timestamp).unwrap();
    }
    recent.clear();
    assert_eq!(recent.iter().count(), 0);
    assert_eq!(recent.dropped(), 0);
    recent.push(&[3; 480], 10_000).unwrap();
    let kept: Vec<_> = recent.iter().collect();
    assert_eq!(kept, [(&[3; 480][..], 10_000)]);
}

/// Feed the actual source accumulator in oddly split native fragments, then
/// select actual produced frames through Recent before any codec is involved.
#[test]
fn fragmented_samples_preserve_channel_order_while_old_complete_frames_are_dropped() {
    for (channels, samples) in [(1, 480), (2, 480), (1, 960), (2, 960)] {
        let values = channels * samples;
        let mut frame = [0; MAX_FRAME_VALUES];
        let (mut filled, mut carry, mut start) = (0, None, 0);
        let mut state = Accumulator {
            frame: &mut frame,
            filled: &mut filled,
            carry: &mut carry,
            start: &mut start,
            values,
            channels,
        };
        let mut recent = Recent::new();
        for index in 0_i16..7 {
            let pcm: Vec<_> = (0..values)
                .map(|n| index * 10 + i16::try_from(n % channels).unwrap())
                .collect();
            let bytes: Vec<_> = pcm.iter().flat_map(|v| v.to_ne_bytes()).collect();
            for fragment in bytes.chunks(137) {
                state
                    .push(Chunk::Bytes(fragment), &mut |pcm, timestamp| {
                        recent.push(pcm, timestamp)
                    })
                    .unwrap();
            }
        }
        let kept: Vec<_> = recent.iter().collect();
        assert_eq!(kept.len(), PULL_FRAMES);
        assert_eq!(recent.dropped(), 3);
        for (offset, (pcm, timestamp)) in kept.iter().enumerate() {
            let index = offset + 3;
            assert_eq!(*timestamp, u64::try_from(index * samples).unwrap());
            assert_eq!(pcm.len(), values);
            for (n, value) in pcm.iter().enumerate() {
                assert_eq!(
                    *value,
                    i16::try_from(index * 10 + n % channels).unwrap()
                );
            }
        }
        assert_eq!(filled, 0);
        assert_eq!(carry, None);
        assert_eq!(start, u64::try_from(7 * samples).unwrap());
    }
}

#[test]
fn partial_frames_survive_normal_pulls_but_completed_scratch_does_not() {
    let mut frame = [0; MAX_FRAME_VALUES];
    let (mut filled, mut carry, mut start) = (0, None, 0);
    let mut state = Accumulator {
        frame: &mut frame,
        filled: &mut filled,
        carry: &mut carry,
        start: &mut start,
        values: 960,
        channels: 2,
    };
    let mut recent = Recent::new();
    let bytes: Vec<_> = (0_i16..960).flat_map(i16::to_ne_bytes).collect();
    state
        .push(Chunk::Bytes(&bytes[..501]), &mut |pcm, time| recent.push(pcm, time))
        .unwrap();
    assert_eq!(recent.iter().count(), 0);
    recent.clear();
    state
        .push(Chunk::Bytes(&bytes[501..]), &mut |pcm, time| recent.push(pcm, time))
        .unwrap();
    let kept: Vec<_> = recent.iter().collect();
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].1, 0);
    assert_eq!(kept[0].0, &(0_i16..960).collect::<Vec<_>>()[..]);
}

#[test]
fn server_hole_is_an_explicit_timeline_gap_not_invented_pcm() {
    let mut frame = [0; MAX_FRAME_VALUES];
    let (mut filled, mut carry, mut start) = (0, None, 0);
    let mut state = Accumulator {
        frame: &mut frame,
        filled: &mut filled,
        carry: &mut carry,
        start: &mut start,
        values: 960,
        channels: 2,
    };
    let mut recent = Recent::new();
    let bytes: Vec<_> = [4_i16; 960].iter().flat_map(|v| v.to_ne_bytes()).collect();
    state
        .push(Chunk::Bytes(&bytes[..480]), &mut |pcm, time| recent.push(pcm, time))
        .unwrap();
    state
        .push(Chunk::Hole(480 * 4), &mut |pcm, time| recent.push(pcm, time))
        .unwrap();
    state
        .push(Chunk::Bytes(&bytes), &mut |pcm, time| recent.push(pcm, time))
        .unwrap();
    let kept: Vec<_> = recent.iter().collect();
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].1, 600); // 120 partial samples discarded, then a 480-sample hole.
    assert_eq!(kept[0].0, &[4; 960]);
}

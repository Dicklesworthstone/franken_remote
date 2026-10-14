use super::*;

fn compare(source_width: u32, target_width: usize) {
    // Every coordinate is unique in all four bytes; a 256-pixel alias must
    // not accidentally pass an integer-mapping regression.
    let source: Vec<_> = (0..source_width).map(u32::to_le_bytes).collect();
    let guard = [0xa5; 4];
    let mut output = vec![guard; target_width + 2];
    scale_row(&source, &mut output[1..=target_width]);
    assert_eq!(output[0], guard);
    assert_eq!(output[target_width + 1], guard);
    for (x, pixel) in output[1..=target_width].iter().enumerate() {
        let index = u64::try_from(x).unwrap() * u64::from(source_width)
            / u64::try_from(target_width).unwrap();
        assert_eq!(*pixel, u32::try_from(index).unwrap().to_le_bytes());
    }
}

#[test]
fn every_small_downscale_matches_exact_division_including_odd_ratios() {
    for source in 1..=256_u32 {
        for target in 1..=source {
            compare(source, usize::try_from(target).unwrap());
        }
    }
}

#[test]
fn workstation_widths_identity_and_extreme_downscales_match_exact_division() {
    for source in [1280, 1920, 2560, 3440, 3840, 5120, 7680, 8192] {
        for target in [
            1,
            2,
            3,
            17,
            255,
            640,
            1023,
            1024,
            1279,
            1280,
            source - 1,
            source,
        ] {
            compare(source, usize::try_from(target).unwrap());
        }
    }
}

#[test]
fn scaling_preserves_all_channels_and_rewrites_every_target_pixel() {
    let source = [
        [1, 2, 3, 4],
        [5, 6, 7, 8],
        [9, 10, 11, 12],
        [13, 14, 15, 16],
    ];
    let mut output = [[0xff; 4]; 3];
    scale_row(&source, &mut output);
    assert_eq!(output, [source[0], source[1], source[2]]);
    let changed = [[99, 88, 77, 66]; 4];
    scale_row(&changed, &mut output);
    assert_eq!(output, [changed[0]; 3]);
    let mut identity = [[0; 4]; 4];
    scale_row(&source, &mut identity);
    assert_eq!(identity, source);
}

#[test]
#[should_panic(expected = "width != 0 && width <= source.len()")]
fn an_empty_output_cannot_enter_the_downscale_kernel() {
    scale_row(&[[0; 4]], &mut []);
}

#[test]
#[should_panic(expected = "width != 0 && width <= source.len()")]
fn an_upscale_cannot_enter_the_downscale_kernel() {
    scale_row(&[[0; 4]], &mut [[0; 4]; 2]);
}

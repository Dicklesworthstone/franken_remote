use fr_core::ids::{CodecConfigurationGeneration, RecoveryGeneration};
use fr_core::limits::ProtocolLimits;
use fr_ffi::FfmpegDecoder;
use fr_media::access_unit::{EncodedAccessUnit, FrameId, FrameKind};
use fr_media::codec::{Decoder, MediaError};
use fr_media::config::{CodecConfiguration, CodedGeometry, ColorInfo, GopPolicy};
use fr_media::surface::SurfaceBackend;

fn test_config(
    width: u32,
    height: u32,
    generation: CodecConfigurationGeneration,
) -> CodecConfiguration {
    let limits = ProtocolLimits::ABSOLUTE;
    let geom = CodedGeometry::from_visible(&limits, width, height, 16).unwrap();
    let gop = GopPolicy::baseline_for_frame_rate(60).unwrap();
    CodecConfiguration::new_baseline(generation, geom, ColorInfo::sdr_bt709(), gop).unwrap()
}

fn mock_access_unit(
    frame: u64,
    config_gen: CodecConfigurationGeneration,
    is_idr: bool,
) -> EncodedAccessUnit {
    let limits = ProtocolLimits::ABSOLUTE;
    let frame_id = FrameId::from_raw(frame);
    let kind = if is_idr {
        FrameKind::Idr {
            recovery: RecoveryGeneration::INITIAL,
        }
    } else {
        FrameKind::Predicted {
            references: FrameId::from_raw(frame - 1),
        }
    };

    let mut payload = vec![0u8; 13];
    let nal_len: u32 = 9;
    payload[0..4].copy_from_slice(&nal_len.to_be_bytes());
    payload[4] = if is_idr { 19 << 1 } else { 1 << 1 };
    payload[5..13].copy_from_slice(&frame.to_be_bytes());

    EncodedAccessUnit::new(&limits, frame_id, kind, config_gen, 16666, payload).unwrap()
}

#[test]
fn unconfigured_decoder_refuses_operations() {
    let mut dec = FfmpegDecoder::new_mock(SurfaceBackend::Fake);
    let au = mock_access_unit(1, CodecConfigurationGeneration::INITIAL, true);

    assert_eq!(dec.submit(&au), Err(MediaError::NotConfigured));
    match dec.poll_output() {
        Err(e) => assert_eq!(e, MediaError::NotConfigured),
        Ok(_) => panic!("expected NotConfigured error"),
    }
    assert_eq!(dec.drain(), Err(MediaError::NotConfigured));
}

#[test]
fn decoder_rejects_mismatched_config_generation() {
    let mut dec = FfmpegDecoder::new_mock(SurfaceBackend::Fake);
    let gen1 = CodecConfigurationGeneration::INITIAL;
    dec.configure(test_config(64, 64, gen1)).unwrap();

    // Access unit carrying next generation
    let gen2 = gen1.next().unwrap();
    let bad_au = mock_access_unit(1, gen2, true);
    assert_eq!(dec.submit(&bad_au), Err(MediaError::ConfigMismatch));
}

#[test]
fn decoder_submit_and_poll_lifecycle() {
    let mut dec = FfmpegDecoder::new_mock(SurfaceBackend::Fake);
    let generation = CodecConfigurationGeneration::INITIAL;
    dec.configure(test_config(64, 64, generation)).unwrap();

    // Submit IDR frame 1
    let au1 = mock_access_unit(1, generation, true);
    dec.submit(&au1).unwrap();

    {
        let pic1 = dec.poll_output().unwrap();
        assert_eq!(pic1.frame_raw(), 1);
        assert_eq!(pic1.surface().width(), 64);
        assert_eq!(pic1.surface().height(), 64);
    }

    // Submit predicted frame 2
    let au2 = mock_access_unit(2, generation, false);
    dec.submit(&au2).unwrap();

    {
        let pic2 = dec.poll_output().unwrap();
        assert_eq!(pic2.frame_raw(), 2);
        assert_eq!(pic2.surface().width(), 64);
        assert_eq!(pic2.surface().height(), 64);
    }
}

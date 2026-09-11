//! Only the control integration fixture uses this synthetic process decoder.
//! No production constructor accepts fabricated startup proof or decoded bytes.
use super::*;
impl Presenter {
    pub(crate) async fn stream_fixture(
        cx: &Cx,
        q: &fr_transport::quic::QuicRecords,
        media: &crate::media_quic::NegotiatedMedia,
        receiver: &mut fr_media::delivery::ReceivePipeline,
    ) -> Self {
        use fr_core::{ids::CodecConfigurationGeneration, limits::ProtocolLimits};
        use fr_media::{config::*, hevc::HevcGuard, worker::*};
        use std::{
            os::unix::fs::PermissionsExt,
            sync::atomic::{AtomicU64, Ordering},
        };
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let cfg = CodecConfiguration::new_baseline(
            CodecConfigurationGeneration::INITIAL,
            CodedGeometry::new(&ProtocolLimits::ABSOLUTE, 320, 240, 320, 240, 2).unwrap(),
            ColorInfo::sdr_bt709(),
            GopPolicy::baseline_for_frame_rate(30).unwrap(),
        )
        .unwrap();
        let mut guard = HevcGuard::new(cfg, ProtocolLimits::ABSOLUTE, 4).unwrap();
        let mut au = Vec::new();
        for nal in [
            "40010c01ffff01600000030090000003000003003cba0240",
            "42010101600000030090000003000003003ca00a080f165ba4a4c2f016a020202080000003008000000f04",
            "4401c0718112",
            "2801ade06702f86753c11ead2f1f6a69",
        ] {
            au.extend_from_slice(&[0, 0, 0, 1]);
            for hex in nal.as_bytes().as_chunks::<2>().0 {
                au.push(u8::from_str_radix(std::str::from_utf8(hex).unwrap(), 16).unwrap());
            }
        }
        guard.validate_annex_b(&au, true).unwrap();
        let image = std::env::temp_dir().join(format!(
            "fr-control-decoder-{}-{}.py",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&image, include_str!("decoder_fixture.py")).unwrap();
        std::fs::set_permissions(&image, std::fs::Permissions::from_mode(0o700)).unwrap();
        let configuration = Configuration {
            width: 320,
            height: 240,
            fps: 30,
            backend: Backend::SoftwareExplicit,
            bitrate: 2_000_000,
            max_access_unit_bytes: ProtocolLimits::ABSOLUTE.max_encoded_access_unit_bytes(),
            generation: CodecConfigurationGeneration::INITIAL,
        };
        let mut presenter = Self::start(
            cx,
            crate::worker::Launch::new(&image, ":0", None, Role::Present, 1).unwrap(),
            configuration,
            &guard.decoder_record().unwrap(),
            receiver,
        )
        .await
        .unwrap();
        // This explicit fixture stamps only the original real test connection.
        // The native lane independently verifies production startup handoff.
        presenter.stream_binding = Some((q.binding(), media.binding()));
        presenter
    }
}

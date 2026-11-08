//! Shared actual-process fixtures; opaque payloads never qualify a native codec.
use asupersync::{
    cx::Cx,
    runtime::{Runtime, RuntimeBuilder},
};
use fr_core::ids::CodecConfigurationGeneration;
use fr_media::worker::{Backend, Configuration, Role};
use frd::{
    media::{CaptureSource, ObservationControl},
    worker::{Deadline, Launch},
};
use std::{
    fmt::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

pub(crate) fn runtime() -> Runtime {
    RuntimeBuilder::new()
        .worker_threads(1)
        .blocking_threads(1, 2)
        .enable_platform_reactor(true)
        .build()
        .unwrap()
}
pub(crate) fn configuration() -> Configuration {
    Configuration {
        width: 320,
        height: 240,
        fps: 30,
        backend: Backend::SoftwareExplicit,
        bitrate: 2_000_000,
        max_access_unit_bytes: 1024 * 1024,
        generation: CodecConfigurationGeneration::INITIAL,
    }
}
pub(crate) fn deadline(cx: &Cx, millis: u64) -> Deadline {
    Deadline::after(cx, Duration::from_millis(millis)).unwrap()
}
/// Keep every executable distinct, including parallel scenarios in one target.
/// The child writer preserves the existing ETXTBSY avoidance contract.
pub(crate) fn image(prefix: &str, contents: impl AsRef<[u8]>) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "{prefix}-{}-{}.py",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    crate::executable::write(&path, contents);
    path
}
pub(crate) async fn capture(
    control: &ObservationControl,
    path: &Path,
    epoch: u128,
) -> CaptureSource {
    CaptureSource::start(
        control,
        Launch::new(path, ":0", None, Role::Capture, epoch).unwrap(),
        configuration(),
    )
    .await
    .unwrap()
}

/// The admitted parameter-set/IDR fixture used by the HEVC parser tests.
/// Its bytes exercise bounded framing, not a live encoder or decoder.
pub(crate) fn hevc_access_unit(annex_b: bool) -> Vec<u8> {
    let mut bytes = Vec::new();
    for nal in [
        "40010c01ffff01600000030090000003000003003cba0240",
        "42010101600000030090000003000003003ca00a080f165ba4a4c2f016a020202080000003008000000f04",
        "4401c0718112",
        "2801ade06702f86753c11ead2f1f6a69",
    ] {
        let prefix = if annex_b {
            1
        } else {
            u32::try_from(nal.len() / 2).unwrap()
        };
        bytes.extend_from_slice(&prefix.to_be_bytes());
        for pair in nal.as_bytes().as_chunks::<2>().0 {
            bytes.push(u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap());
        }
    }
    bytes
}

/// Length-prefixed fixture bytes embedded in the scripted worker's reply.
pub(crate) fn hevc_payload() -> String {
    let mut payload = String::new();
    for value in hevc_access_unit(false) {
        write!(payload, "{value:02x}").unwrap();
    }
    payload
}

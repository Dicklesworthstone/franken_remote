//! `fr-windows capture-probe`: the Windows host media path on this machine,
//! end to end. Desktop Duplication, the D3D11 video processor and the
//! adapter's hardware HEVC encoder (fr-native `windows-media`), the same
//! admission every host applies, then this client's own Media Foundation
//! decoder on the units. Prints one JSON document of counts and timings; no
//! picture or bitstream leaves the process. Must run in the interactive
//! session (Desktop Duplication duplicates the input desktop).
use fr_core::ids::CodecConfigurationGeneration;
use fr_core::limits::ProtocolLimits;
use fr_media::access_unit::FrameId;
use fr_media::config::{CodecConfiguration, CodedGeometry, ColorInfo, GopPolicy};
use fr_media::hevc::HevcGuard;
use fr_mobile_ffi::video::Decoder;
use fr_native::windows_capture::{Captured, WinCapture, displays};
use serde_json::json;
use std::io::Write;
use std::process::ExitCode;
use std::time::{Duration, Instant};

struct Options {
    seconds: u64,
    fps: u32,
    bitrate: u32,
    display: usize,
    /// Encode every tick, even when nothing changed.
    all: bool,
}
impl Options {
    fn parse(mut rest: &[String]) -> Option<Self> {
        let mut options = Self {
            seconds: 10,
            fps: 30,
            bitrate: 8_000_000,
            display: 0,
            all: false,
        };
        loop {
            match rest {
                [] => return Some(options),
                [flag, tail @ ..] if flag == "--all" => {
                    options.all = true;
                    rest = tail;
                }
                [flag, value, tail @ ..] => {
                    match flag.as_str() {
                        "--seconds" => options.seconds = value.parse().ok().filter(|s| *s != 0)?,
                        "--fps" => {
                            options.fps = value.parse().ok().filter(|f| (1..=240).contains(f))?;
                        }
                        "--bitrate" => options.bitrate = value.parse().ok()?,
                        "--display" => options.display = value.parse().ok()?,
                        _ => return None,
                    }
                    rest = tail;
                }
                [_] => return None,
            }
        }
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        reason = "an index into a short list"
    )]
    let index = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[index.min(sorted.len() - 1)]
}

fn millis(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

pub fn run(rest: &[String]) -> ExitCode {
    let Some(options) = Options::parse(rest) else {
        eprint!("{}", crate::USAGE);
        return ExitCode::from(2);
    };
    let report = probe(&options);
    let ok = report["end"] == "ok";
    if writeln!(std::io::stdout().lock(), "{report}").is_err() {
        return ExitCode::from(74);
    }
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

#[allow(clippy::too_many_lines, reason = "one measured sequence")]
fn probe(options: &Options) -> serde_json::Value {
    let fail = |stage: &str, error: &dyn std::fmt::Debug| json!({"end": "failed", "stage": stage, "error": format!("{error:?}")});
    let list = match displays() {
        Ok(list) => list,
        Err(e) => return fail("displays", &e),
    };
    let Some(display) = list.get(options.display).copied() else {
        return json!({"end": "failed", "stage": "display", "displays": list.len()});
    };
    let limits = ProtocolLimits::ABSOLUTE;
    let config = match CodedGeometry::from_visible(&limits, display.width, display.height, 16)
        .map_err(|e| format!("{e:?}"))
        .and_then(|geometry| {
            GopPolicy::baseline_for_frame_rate(options.fps)
                .map_err(|e| format!("{e:?}"))
                .map(|gop| (geometry, gop))
        })
        .and_then(|(geometry, gop)| {
            CodecConfiguration::new_baseline(
                CodecConfigurationGeneration::INITIAL,
                geometry,
                ColorInfo::sdr_bt709(),
                gop,
            )
            .map_err(|e| format!("{e:?}"))
        }) {
        Ok(config) => config,
        Err(e) => return fail("configuration", &e),
    };
    let g = config.geometry();
    let opened = Instant::now();
    let mut capture = match WinCapture::open(&display, config, limits, options.fps, options.bitrate)
    {
        Ok(capture) => capture,
        Err(e) => return fail("open", &e),
    };
    let open_ms = millis(opened.elapsed());
    let encoder = capture.encoder().clone();
    // The viewer's side: admission of the length-prefixed units, then the
    // decoder built from the record the first IDR establishes.
    let mut viewer_guard = match HevcGuard::new(config, limits, 4) {
        Ok(guard) => guard,
        Err(e) => return fail("guard", &e),
    };
    let mut decoder: Option<Decoder> = None;
    let (mut pictures, mut sized, mut gpu_pictures) = (0_u64, 0_u64, 0_u64);
    let mut decode_error = None;
    let mut decode_ms = Vec::new();
    let interval = Duration::from_secs_f64(1.0 / f64::from(options.fps));
    let ticks = options.seconds * u64::from(options.fps);
    // One forced IDR mid-run checks the on-demand keyframe path.
    let forced_at = ticks / 2;
    let (mut forced_ok, mut units, mut bytes) = (None, 0_u64, 0_u64);
    let mut unit_ms = Vec::new();
    // poll, compose, convert, encode, admit
    let mut stage_ms: [Vec<f64>; 5] = Default::default();
    let mut first_unit_ms = None;
    let start = Instant::now();
    let mut end = "ok".to_string();
    for tick in 0..ticks {
        let due = start + interval * u32::try_from(tick).unwrap_or(u32::MAX);
        if let Some(wait) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
        let observed = u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX);
        let force = tick == forced_at;
        let began = Instant::now();
        let result = capture.capture(FrameId::from_raw(tick + 1), observed, force, !options.all);
        let took = began.elapsed();
        match result {
            Ok(Captured::Unchanged(_)) => {}
            Ok(Captured::Unit(unit)) => {
                units += 1;
                bytes += unit.bytes().len() as u64;
                unit_ms.push(millis(took));
                let s = capture.stages();
                for (list, us) in stage_ms
                    .iter_mut()
                    .zip([s.poll, s.compose, s.convert, s.encode, s.admit])
                {
                    list.push(f64::from(us) / 1000.0);
                }
                first_unit_ms.get_or_insert(millis(opened.elapsed()));
                if force {
                    forced_ok = Some(unit.is_idr());
                }
                if let Err(e) = viewer_guard.validate_length_prefixed(unit.bytes(), unit.is_idr()) {
                    end = format!("viewer_admission:{e:?}");
                    break;
                }
                if decoder.is_none() && unit.is_idr() {
                    match viewer_guard
                        .decoder_record()
                        .map_err(|e| format!("{e:?}"))
                        .and_then(|record| {
                            Decoder::new(config, limits, record.bytes())
                                .map_err(|e| format!("{e:?}"))
                        }) {
                        Ok(d) => decoder = Some(d),
                        Err(e) => {
                            decode_error = Some(format!("decoder_open:{e}"));
                        }
                    }
                }
                if let Some(d) = decoder.as_mut() {
                    let began = Instant::now();
                    match d.submit(&unit, true) {
                        Ok(Some(image)) => {
                            decode_ms.push(millis(began.elapsed()));
                            pictures += 1;
                            let (w, h, _, gpu) = image.describe();
                            if (w, h) == (g.crop_width(), g.crop_height()) {
                                sized += 1;
                            }
                            if gpu {
                                gpu_pictures += 1;
                            }
                        }
                        Ok(None) => {}
                        Err(e) => {
                            decode_error = Some(format!("decode:{e:?}"));
                            decoder = None;
                        }
                    }
                }
            }
            Err(e) => {
                end = format!("capture:{e:?}");
                if let Some(refusal) = capture.refusal() {
                    end = format!("{end} ({refusal})");
                }
                break;
            }
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    let stats = capture.statistics();
    unit_ms.sort_by(f64::total_cmp);
    for list in &mut stage_ms {
        list.sort_by(f64::total_cmp);
    }
    let stage = |index: usize| {
        json!({
            "p50": percentile(&stage_ms[index], 0.5),
            "p95": percentile(&stage_ms[index], 0.95),
        })
    };
    decode_ms.sort_by(f64::total_cmp);
    #[allow(clippy::cast_precision_loss, reason = "byte counts far below 2^52")]
    let kbps = if elapsed > 0.0 {
        bytes as f64 * 8.0 / elapsed / 1000.0
    } else {
        0.0
    };
    json!({
        "end": end,
        "display": {
            "index": options.display,
            "of": list.len(),
            "width": display.width,
            "height": display.height,
            "dpi": display.dpi,
            "rotation": display.rotation,
        },
        "coded": [g.coded_width(), g.coded_height()],
        "encoder": {
            "name": encoder.name,
            "refused": encoder.refused,
            "provides_samples": encoder.provides_samples,
        },
        "fps": options.fps,
        "bitrate": options.bitrate,
        "mode": if options.all { "every_tick" } else { "if_changed" },
        "open_ms": open_ms,
        "first_unit_ms": first_unit_ms,
        "ticks": ticks,
        "units": units,
        "unchanged": stats.unchanged,
        "images": stats.images,
        "pointer_only": stats.pointer_only,
        "recreated": stats.recreated,
        "idr": stats.idr,
        "unrequested_idr": stats.unrequested_idr,
        "forced_idr_honoured": forced_ok,
        "kbps": kbps,
        "unit_ms": {
            "p50": percentile(&unit_ms, 0.5),
            "p95": percentile(&unit_ms, 0.95),
            "max": unit_ms.last().copied().unwrap_or(0.0),
        },
        "stage_ms": {
            "poll": stage(0),
            "compose": stage(1),
            "convert": stage(2),
            "encode": stage(3),
            "admit": stage(4),
        },
        "decoder": {
            "hardware": decoder.as_ref().and_then(Decoder::hardware),
            "decoded": pictures,
            "picture_size_ok": sized,
            "gpu_pictures": gpu_pictures,
            "error": decode_error,
            "decode_ms_p50": percentile(&decode_ms, 0.5),
            "decode_ms_p95": percentile(&decode_ms, 0.95),
        },
        "seconds": elapsed,
    })
}

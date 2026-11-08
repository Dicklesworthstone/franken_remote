//! Plays an HEVC Annex B file, encoded with the host's libx265 settings, in
//! the macOS viewer window: Rust admission, `VideoToolbox` decode, Metal
//! presentation, `AppKit` input capture (printed, not sent anywhere).
//!
//! ```text
//! cargo run -p fr-native --no-default-features --features macos-desktop \
//!   --example macos_present -- FILE [--size 1920x1080] [--fps 30] [--seconds N]
//! ```
#[cfg(target_os = "macos")]
mod mac {
    use fr_core::ids::{CodecConfigurationGeneration, RecoveryGeneration};
    use fr_core::limits::ProtocolLimits;
    use fr_media::access_unit::{EncodedAccessUnit, FrameId, FrameKind};
    use fr_media::config::{CodecConfiguration, CodedGeometry, ColorInfo, GopPolicy};
    use fr_media::hevc::framing::annex_b_to_length_prefixed;
    use fr_media::hevc::{HevcGuard, strip_encoder_sei};
    use fr_native::macos::{self, HevcDecoder, Window, WindowEvent};
    use std::process::ExitCode;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    struct Options {
        path: String,
        width: u32,
        height: u32,
        fps: u32,
        seconds: Option<f64>,
    }

    fn options() -> Result<Options, String> {
        let mut args = std::env::args().skip(1);
        let mut o = Options {
            path: String::new(),
            width: 1920,
            height: 1080,
            fps: 30,
            seconds: None,
        };
        while let Some(arg) = args.next() {
            let mut value = || args.next().ok_or(format!("{arg} needs a value"));
            match arg.as_str() {
                "--size" => {
                    let v = value()?;
                    let (w, h) = v.split_once('x').ok_or("--size WxH")?;
                    o.width = w.parse().map_err(|_| "--size WxH")?;
                    o.height = h.parse().map_err(|_| "--size WxH")?;
                }
                "--fps" => o.fps = value()?.parse().map_err(|_| "--fps N")?,
                "--seconds" => o.seconds = Some(value()?.parse().map_err(|_| "--seconds N")?),
                _ if o.path.is_empty() => o.path = arg,
                _ => return Err(format!("unexpected argument {arg}")),
            }
        }
        if o.path.is_empty() || o.fps == 0 {
            return Err("usage: macos_present FILE [--size WxH] [--fps N] [--seconds N]".into());
        }
        Ok(o)
    }

    /// Annex B access units, split at each access unit delimiter (NAL 35).
    fn split(stream: &[u8]) -> Vec<&[u8]> {
        let mut starts: Vec<usize> = (0..stream.len().saturating_sub(4))
            .filter(|&i| stream[i..i + 4] == [0, 0, 0, 1] && (stream[i + 4] >> 1) & 0x3f == 35)
            .collect();
        starts.push(stream.len());
        starts.windows(2).map(|w| &stream[w[0]..w[1]]).collect()
    }

    #[allow(clippy::too_many_lines)]
    pub fn main() -> ExitCode {
        let o = match options() {
            Ok(o) => o,
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::from(2);
            }
        };
        let limits = ProtocolLimits::ABSOLUTE;
        let config = CodecConfiguration::new_baseline(
            CodecConfigurationGeneration::INITIAL,
            // The host's geometry: visible size rounded up to 16 (1080 codes as 1088).
            CodedGeometry::from_visible(&limits, o.width, o.height, 16).expect("geometry"),
            ColorInfo::sdr_bt709(),
            GopPolicy::baseline_for_frame_rate(o.fps).expect("frame rate"),
        )
        .expect("configuration");
        let stream = std::fs::read(&o.path).expect("read stream");
        // What the host sends: SEI stripped, four-byte length prefixes.
        let pictures: Vec<Vec<u8>> = split(&stream)
            .into_iter()
            .map(|au| annex_b_to_length_prefixed(&strip_encoder_sei(au).expect("sei"), limits))
            .collect::<Result<_, _>>()
            .expect("access units");
        // The record the host derives from its first IDR.
        let mut guard = HevcGuard::new(config, limits, 4).expect("guard");
        if let Err(e) = guard.validate_length_prefixed(&pictures[0], true) {
            eprintln!("first picture refused by admission: {e:?}");
            return ExitCode::from(1);
        }
        let record = guard.decoder_record().expect("record").bytes().to_vec();

        macos::initialize_application().expect("AppKit");
        let mut window =
            Window::open(o.width, o.height, "FrankenRemote macos_present").expect("window");
        println!("window {}", window.number());
        let surface = window.surface();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let fps = o.fps;
        let media = std::thread::spawn(move || {
            let mut decoder = HevcDecoder::new(surface, config, limits, &record).expect("decoder");
            let hardware = decoder.hardware();
            let start = Instant::now();
            let mut times = Vec::new();
            let mut frame = 0_u64;
            'play: while !stopping.load(Ordering::Relaxed) {
                for (i, bytes) in pictures.iter().enumerate() {
                    if stopping.load(Ordering::Relaxed) {
                        break 'play;
                    }
                    let kind = if i == 0 {
                        FrameKind::Idr {
                            recovery: RecoveryGeneration::INITIAL,
                        }
                    } else {
                        FrameKind::Predicted {
                            references: FrameId::from_raw(frame - 1),
                        }
                    };
                    let due = start + Duration::from_micros(frame * 1_000_000 / u64::from(fps));
                    if let Some(wait) = due.checked_duration_since(Instant::now()) {
                        std::thread::sleep(wait);
                    }
                    let unit = EncodedAccessUnit::new(
                        &limits,
                        FrameId::from_raw(frame),
                        kind,
                        CodecConfigurationGeneration::INITIAL,
                        frame * 1_000_000 / u64::from(fps),
                        bytes.clone(),
                    )
                    .expect("unit");
                    let t = Instant::now();
                    if let Err(e) = decoder.submit(&unit) {
                        eprintln!("frame {frame} refused: {e:?}");
                        break 'play;
                    }
                    times.push(t.elapsed());
                    frame += 1;
                }
            }
            (frame, decoder.surface().presented(), hardware, times)
        });

        let deadline = o
            .seconds
            .map(|s| Instant::now() + Duration::from_secs_f64(s));
        let mut events = Vec::new();
        let mut close = "deadline";
        loop {
            events.clear();
            window.poll(Duration::from_millis(16), &mut events);
            for e in &events {
                match e {
                    WindowEvent::Pointer(_) => {}
                    other => println!("event {other:?}"),
                }
            }
            if events.contains(&WindowEvent::CloseRequested) {
                close = "window closed";
                break;
            }
            if media.is_finished() {
                close = "media stopped";
                break;
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                break;
            }
        }
        stop.store(true, Ordering::Relaxed);
        let (frames, presented, hardware, mut times) = media.join().expect("media thread");
        drop(window);
        times.sort();
        let at = |q: usize| {
            times
                .get(times.len().saturating_sub(1) * q / 1000)
                .map_or(0.0, |d| d.as_secs_f64() * 1000.0)
        };
        println!(
            "{{\"end\":\"{close}\",\"frames\":{frames},\"presented\":{presented},\"hardware\":{hardware:?},\"decode_present_ms\":{{\"p50\":{:.2},\"p95\":{:.2},\"p99\":{:.2},\"max\":{:.2}}}}}",
            at(500),
            at(950),
            at(990),
            at(1000)
        );
        ExitCode::SUCCESS
    }
}

#[cfg(target_os = "macos")]
fn main() -> std::process::ExitCode {
    mac::main()
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("macos_present runs on macOS only");
}

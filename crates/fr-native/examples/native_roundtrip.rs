#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use fr_core::{
        ids::{CodecConfigurationGeneration, RecoveryGeneration},
        limits::ProtocolLimits,
    };
    use fr_media::{
        access_unit::{EncodedAccessUnit, FrameId},
        config::{CodecConfiguration, CodedGeometry, ColorInfo, GopPolicy},
        delivery::{DeliveryMode, MediaBindings, MediaEpoch},
    };
    use fr_native::{HevcDecoder, HevcEncoder, X11Surface};
    use fr_wire::{FrameDescriptor, MediaLimits, PipelineState, Progress, SourceObservation};
    let backend = backend()?;
    let limits = ProtocolLimits::ABSOLUTE;
    let mut capture = X11Surface::capture(None, limits)?;
    let (w, h) = (capture.width(), capture.height());
    let config = CodecConfiguration::new_baseline(
        CodecConfigurationGeneration::INITIAL,
        CodedGeometry::from_visible(&limits, w, h, 16)?,
        ColorInfo::sdr_bt709(),
        GopPolicy::baseline_for_frame_rate(30)?,
    )?;
    let mut encoder = HevcEncoder::new(config, limits, backend, 30, 4_000_000)?;
    let mut decoder = HevcDecoder::new(config, limits)?;
    let mut output = X11Surface::presenter(None, w, h, limits)?;
    let mut source = X11Surface::presenter(None, w, h, limits)?;
    let wire = MediaLimits::new(limits, 1_150, 16_384, 64)?;
    let bindings = MediaBindings::new(1, 2, 3, 4)?;
    let epoch = MediaEpoch {
        configuration: config.generation(),
        recovery: RecoveryGeneration::INITIAL,
    };
    let (mut send, mut receive, budget) = delivery(wire, bindings, epoch)?;
    for n in 0..12_u64 {
        let pattern = pattern(w, h, n, &limits)?;
        source.present(&pattern)?;
        let captured = capture.snapshot()?;
        if captured.pixels() != pattern.pixels() {
            return Err("X11 root capture does not match submitted source window".into());
        }
        let now = n * 33_333;
        encoder.submit(&captured, FrameId::from_raw(n), now, false)?;
        let unit = encoder.poll_output()?;
        let descriptor = FrameDescriptor {
            frame: n,
            total_bytes: u32::try_from(unit.bytes().len())?,
            stride: wire.fragment_stride(),
            capture_micros: now,
            reference: if unit.is_idr() { None } else { Some(n - 1) },
        };
        send.push(
            Progress {
                descriptor,
                observed_micros: now,
                observation: SourceObservation::Captured,
                pipeline: PipelineState::Running,
            },
            unit.bytes().to_vec(),
            if n == 0 {
                DeliveryMode::Recovery
            } else {
                DeliveryMode::Datagrams
            },
            now,
        )?;
        let mut packet = [0; 1_150];
        while let Some(offer) = send.next_packet(now, &mut packet)? {
            receive.receive(offer.channel(), &packet[..offer.byte_len()], now)?;
        }
        let picture = receive
            .take_decodable(now)?
            .ok_or("complete picture not delivered")?;
        let delivered = EncodedAccessUnit::new(
            &limits,
            unit.frame(),
            unit.kind(),
            unit.config_generation(),
            now,
            picture.bytes().to_vec(),
        )?;
        decoder.submit(&delivered)?;
        let (id, rendered) = decoder.poll_output()?;
        if id != unit.frame() {
            return Err("native decode identity mismatch".into());
        }
        receive.acknowledge_decode(&picture, true, now)?;
        output.present(&rendered)?;
        let presented = output.snapshot()?;
        if presented.pixels() != rendered.pixels() {
            return Err("X11 readback differs from rendered BGRA presentation".into());
        }
        drop(picture);
        if budget.usage().pictures != 0 {
            return Err("compressed decoder ownership not released".into());
        }
    }
    println!(
        "native_capture_encode_delivery_decode_present=passed frames=12 backend={backend:?} surface=CPU-staged display=X11 evidence=X11-readback not_optical=true"
    );
    Ok(())
}
#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("native Linux media path is unavailable on this target");
    std::process::exit(2);
}

#[cfg(target_os = "linux")]
fn pattern(
    width: u32,
    height: u32,
    frame: u64,
    limits: &fr_core::limits::ProtocolLimits,
) -> Result<fr_native::BgraFrame, Box<dyn std::error::Error>> {
    let mut pixels = vec![0; usize::try_from(u64::from(width) * u64::from(height) * 4)?];
    for (index, pixel) in pixels.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let x = u32::try_from(index)? % width;
        let y = u32::try_from(index)? / width;
        pixel.copy_from_slice(&[
            u8::try_from(x % 256)?,
            u8::try_from(y % 256)?,
            u8::try_from(frame * 17)?,
            255,
        ]);
    }
    Ok(fr_native::BgraFrame::new(width, height, pixels, limits)?)
}

#[cfg(target_os = "linux")]
fn delivery(
    wire: fr_wire::MediaLimits,
    bindings: fr_media::delivery::MediaBindings,
    epoch: fr_media::delivery::MediaEpoch,
) -> Result<
    (
        fr_media::delivery::SendCache,
        fr_media::delivery::ReceivePipeline,
        fr_media::delivery::MediaBudget,
    ),
    Box<dyn std::error::Error>,
> {
    use fr_media::delivery::{
        MediaBudget, ReceiveConfig, ReceivePipeline, ReceivePolicy, SendCache, SendPolicy,
    };
    let send = SendCache::new(wire, bindings, epoch, SendPolicy::default())?;
    let budget = MediaBudget::new(wire.protocol())?;
    let mut receive = ReceivePipeline::new(
        ReceiveConfig {
            limits: wire,
            bindings,
            epoch,
            policy: ReceivePolicy::default(),
        },
        budget.clone(),
    )?;
    receive.decoder_configured(0)?;
    Ok((send, receive, budget))
}

#[cfg(target_os = "linux")]
fn backend() -> Result<fr_native::EncodeBackend, Box<dyn std::error::Error>> {
    use fr_native::EncodeBackend;
    Ok(match std::env::args().nth(1).as_deref() {
        Some("--software-explicit") => EncodeBackend::SoftwareExplicit,
        Some("--nvenc") => EncodeBackend::Nvenc,
        Some("--vaapi") => EncodeBackend::Vaapi,
        _ => return Err("select --nvenc, --vaapi, or --software-explicit".into()),
    })
}

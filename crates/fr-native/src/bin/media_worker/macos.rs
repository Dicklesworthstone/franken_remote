//! The macOS capture role (fr-p1-host-macos-vif): one display through
//! `ScreenCaptureKit` into `VideoToolbox` HEVC, behind the same FRW0 records
//! as the Linux worker. The host discovers monitors, configures one, then
//! requests units. Encodes are synchronous, so a capture request is answered
//! with its unit (or unchanged evidence) and never with `NeedInput`.
use fr_core::{ids::DisplayGeometryGeneration, limits::ProtocolLimits};
use fr_media::worker::{
    self, Backend, Configuration, Error, Kind, Record, Role, Sequence, cursor::Observation,
};
use fr_native::{
    NativeError,
    macos_capture::{self, Captured, MacCapture, MacDisplay},
};
use fr_wire::display::{Catalog, Display};
use std::{fs::File, io, io::Write as _, os::fd::AsFd};

fn native(e: NativeError) -> Error {
    match e {
        NativeError::GeometryChanged => Error::GeometryChanged,
        NativeError::Unavailable | NativeError::DisplayUnavailable => Error::Unsupported,
        NativeError::Allocation => Error::Allocation,
        _ => Error::NativeFailure,
    }
}

const fn gcd(a: u32, b: u32) -> u32 {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// The catalog entry for one display: pixels are the encoded size (the
/// display's own when they fit the protocol), logical size its points.
fn entry(d: &MacDisplay, limits: &ProtocolLimits) -> Option<Display> {
    // The input executor maps a grant back to this display with the same
    // `desktop_bounds`, so the published origin and size come only from it.
    let bounds = d.desktop_bounds(limits)?;
    let (w, h) = (bounds.width(), bounds.height());
    let divisor = gcd(w, d.point_width).max(1);
    let display = Display {
        handle: u128::from(d.id),
        geometry: DisplayGeometryGeneration::INITIAL,
        x: bounds.origin().x,
        y: bounds.origin().y,
        pixel_width: w,
        pixel_height: h,
        logical_width: d.point_width,
        logical_height: d.point_height,
        scale_numerator: w / divisor,
        scale_denominator: d.point_width / divisor,
        rotation: 0,
    };
    display.validate(limits).ok()?;
    Some(display)
}

/// Capturable displays and their catalog. A display whose size cannot be
/// represented is left out rather than offered at a size it cannot have.
fn inventory(limits: &ProtocolLimits) -> Result<(Vec<MacDisplay>, Catalog), Error> {
    let displays: Vec<MacDisplay> = macos_capture::displays()
        .map_err(native)?
        .into_iter()
        .filter(|d| entry(d, limits).is_some())
        .collect();
    let entries: Vec<Display> = displays.iter().filter_map(|d| entry(d, limits)).collect();
    if entries.is_empty() {
        return Err(Error::Unsupported);
    }
    let catalog = Catalog::new(1, &entries, limits).map_err(|_| Error::ResourceLimit)?;
    Ok((displays, catalog))
}

fn check_all(displays: &[MacDisplay]) -> Result<(), Error> {
    displays.iter().try_for_each(|d| d.check().map_err(native))
}

struct Share {
    capture: MacCapture,
    display: MacDisplay,
}
impl Drop for Share {
    fn drop(&mut self) {
        if let Ok(stats) = self.capture.statistics() {
            // One content-free line per native owner. Stdout remains FRW0.
            // Counters describe work, not measured copies or GPU time.
            let _ = writeln!(
                io::stderr().lock(),
                "capture_verification_requests={} capture_verification_encoded={} verification_wait_ms={}",
                stats.verification_requests,
                stats.verification_encoded,
                stats.verification_wait_ms
            );
        }
    }
}
impl Share {
    fn handle(
        &mut self,
        request: &Record,
        limits: &ProtocolLimits,
    ) -> Result<(Kind, Vec<u8>), Error> {
        match request.header.kind {
            Kind::CheckMonitor => {
                self.display.check().map_err(native)?;
                self.capture.check().map_err(native)?;
                Ok((Kind::MonitorValid, Vec::new()))
            }
            // The pointer is part of the captured picture; no separate image.
            Kind::ReadCursor => Ok((
                Kind::CursorSnapshot,
                worker::cursor::encode_observation(Observation::Unsupported, limits)?,
            )),
            Kind::Capture | Kind::CaptureIfChanged => {
                let (frame, observed, force_idr) = worker::parse_capture(request.body())?;
                let only_if_changed = request.header.kind == Kind::CaptureIfChanged;
                match self.capture(frame, observed, force_idr, only_if_changed)? {
                    Captured::Unit(unit) => Ok((Kind::Unit, worker::unit_payload(&unit)?)),
                    Captured::Unchanged(proof) => Ok((Kind::Unchanged, proof.encode()?)),
                }
            }
            Kind::CaptureAtRate => {
                let c = worker::rate::Capture::decode(request.body())?;
                if c.bitrate != self.capture.bitrate() {
                    self.capture.set_bitrate(c.bitrate).map_err(native)?;
                }
                let bitrate = self.capture.bitrate();
                match self.capture(c.frame, c.observed_micros, c.force_idr, c.only_if_changed)? {
                    Captured::Unit(unit) => {
                        Ok((Kind::RateUnit, worker::rate::unit_payload(&unit, bitrate)?))
                    }
                    Captured::Unchanged(proof) => Ok((
                        Kind::RateUnchanged,
                        worker::rate::unchanged_payload(proof, bitrate)?,
                    )),
                }
            }
            _ => Err(Error::WrongState),
        }
    }

    fn capture(
        &mut self,
        frame: fr_media::access_unit::FrameId,
        observed: u64,
        force_idr: bool,
        only_if_changed: bool,
    ) -> Result<Captured, Error> {
        // A changed display would be scaled into the old size: refuse instead.
        // The native capture checks this display's mode and placement on
        // entry, after selecting a picture and before publishing it, against
        // the description taken at open (equal to `self.display`: Ready
        // follows a check of it). Those checks keep the documented 100 ms
        // reuse; repeating an uncached WindowServer query here cost ~3 ms per
        // picture on a busy host.
        self.capture
            .capture(frame, observed, force_idr, only_if_changed)
            .map_err(native)
    }
}

/// Monitor discovery and selection; `None` after a Stop before configuring.
fn discover(
    sequence: &mut Sequence,
    identity: &mut worker::Identity,
    input: &mut impl io::Read,
    output: &mut impl io::Write,
) -> Result<Option<(Configuration, Share, Vec<u8>)>, Error> {
    let limits = ProtocolLimits::ABSOLUTE;
    let (displays, catalog) = inventory(&limits)?;
    Record::new(
        Kind::CaptureMonitors,
        *identity,
        worker::capture::monitors::encode_catalog(catalog, &limits)?,
        &limits,
    )?
    .write(output, &limits)?;
    loop {
        let request = Record::read(input, &limits)?.ok_or(Error::Io)?;
        sequence.accept(request.header)?;
        *identity = request.header.identity;
        match request.header.kind {
            Kind::CheckMonitor => {
                check_all(&displays)?;
                Record::new(Kind::MonitorValid, *identity, Vec::new(), &limits)?
                    .write(output, &limits)?;
            }
            Kind::Stop => {
                Record::new(Kind::Stopped, *identity, Vec::new(), &limits)?
                    .write(output, &limits)?;
                return Ok(None);
            }
            Kind::ConfigureMonitor => {
                let (c, selected) =
                    worker::capture::monitors::decode_configuration(request.body(), catalog)?;
                if c.backend != Backend::VideoToolbox {
                    return Err(Error::Unsupported);
                }
                let display = *displays
                    .iter()
                    .find(|d| u128::from(d.id) == selected.handle)
                    .ok_or(Error::GeometryChanged)?;
                display.check().map_err(native)?;
                let capture = MacCapture::open(
                    &display,
                    c.codec()?,
                    c.limits()?,
                    u32::from(c.fps),
                    c.bitrate,
                )
                .map_err(native)?;
                // Opening can block; check the display again before Ready.
                display.check().map_err(native)?;
                return Ok(Some((c, Share { capture, display }, request.into_body())));
            }
            _ => return Err(Error::WrongState),
        }
    }
}

fn refuse(
    identity: worker::Identity,
    error: Error,
    output: &mut impl io::Write,
    limits: &ProtocolLimits,
) -> Result<(), Error> {
    Record::new(
        Kind::Refused,
        identity,
        (error as u16).to_be_bytes().to_vec(),
        limits,
    )?
    .write(output, limits)
}

/// `--probe-encoder videotoolbox`: one forced IDR of the main display at the
/// size a share would use, through SEI stripping and admission.
fn probe_encoder(
    backend: &str,
) -> Result<(u32, u32, usize, macos_capture::CaptureStatistics), Error> {
    if backend != "videotoolbox" {
        return Err(Error::Malformed);
    }
    let limits = ProtocolLimits::ABSOLUTE;
    let (displays, catalog) = inventory(&limits)?;
    // The main display is the one at the desktop origin.
    let main = displays
        .iter()
        .find(|d| d.x == 0 && d.y == 0)
        .or(displays.first())
        .ok_or(Error::Unsupported)?;
    let entry = catalog
        .displays()
        .iter()
        .find(|e| e.handle == u128::from(main.id))
        .ok_or(Error::Unsupported)?;
    let configuration = Configuration {
        width: entry.pixel_width,
        height: entry.pixel_height,
        fps: 60,
        backend: Backend::VideoToolbox,
        bitrate: 20_000_000,
        max_access_unit_bytes: limits.max_encoded_access_unit_bytes(),
        generation: fr_core::ids::CodecConfigurationGeneration::INITIAL,
    };
    let mut capture = MacCapture::open(
        main,
        configuration.codec()?,
        configuration.limits()?,
        60,
        20_000_000,
    )
    .map_err(native)?;
    match capture
        .capture(fr_media::access_unit::FrameId::from_raw(1), 1, true, false)
        .map_err(native)?
    {
        Captured::Unit(unit) => Ok((
            entry.pixel_width,
            entry.pixel_height,
            unit.bytes().len(),
            capture.statistics().map_err(native)?,
        )),
        Captured::Unchanged(_) => Err(Error::WrongState),
    }
}

fn parent(args: &mut impl Iterator<Item = String>) -> Result<(), Error> {
    if args.next().as_deref() != Some("--parent-pid") {
        return Err(Error::WrongState);
    }
    let parent = args
        .next()
        .ok_or(Error::WrongState)?
        .parse::<u32>()
        .map_err(|_| Error::WrongState)?;
    macos_capture::bind_parent(parent).map_err(native)?;
    if args.next().is_some() {
        return Err(Error::Malformed);
    }
    Ok(())
}

pub fn run() -> Result<(), Error> {
    let mut args = std::env::args().skip(1);
    let role = match args.next().as_deref() {
        Some("--probe-encoder") => {
            let backend = args.next().ok_or(Error::Malformed)?;
            parent(&mut args)?;
            // frd's startup probe is where a missing grant is asked for and
            // named, never a share: under launchd the prompt names frd.
            if !macos_capture::request_access() {
                println!("encoder_probe=refused reason=screen_recording_not_granted");
                return Err(Error::Unsupported);
            }
            return match probe_encoder(&backend) {
                Ok((width, height, bytes, stats)) => {
                    println!(
                        "encoder_probe=admitted width={width} height={height} idr_bytes={bytes} verification_requests={} verification_encoded={} verification_wait_ms={}",
                        stats.verification_requests,
                        stats.verification_encoded,
                        stats.verification_wait_ms
                    );
                    Ok(())
                }
                Err(error) => {
                    println!("encoder_probe=refused reason={error}");
                    Err(error)
                }
            };
        }
        Some("--capture") => Role::Capture,
        // Presentation and audio are not macOS worker roles (yet).
        _ => return Err(Error::WrongRole),
    };
    parent(&mut args)?;
    debug_assert_eq!(role, Role::Capture);
    // One unbuffered descriptor from the first record on, as on Linux.
    let mut input = File::from(
        io::stdin()
            .as_fd()
            .try_clone_to_owned()
            .map_err(|_| Error::Io)?,
    );
    let mut output = io::stdout().lock();
    let absolute = ProtocolLimits::ABSOLUTE;
    let first = Record::read(&mut input, &absolute)?.ok_or(Error::Io)?;
    let mut identity = first.header.identity;
    let mut sequence = Sequence::new(identity.epoch)?;
    sequence.accept(first.header)?;
    if first.header.kind != Kind::DiscoverMonitors {
        refuse(identity, Error::Unsupported, &mut output, &absolute)?;
        return Err(Error::Unsupported);
    }
    let (configuration, mut share, body) =
        match discover(&mut sequence, &mut identity, &mut input, &mut output) {
            Ok(Some(value)) => value,
            Ok(None) => return Ok(()),
            Err(error) => {
                refuse(identity, error, &mut output, &absolute)?;
                return Err(error);
            }
        };
    let limits = configuration.limits()?;
    Record::new(Kind::MonitorReady, identity, body, &limits)?.write(&mut output, &limits)?;
    // Stdin EOF (parent gone or pipe closed) ends the loop and frees the share.
    while let Some(request) = Record::read(&mut input, &limits)? {
        let identity = request.header.identity;
        sequence.accept(request.header)?;
        if request.header.kind == Kind::Stop {
            drop(share);
            return Record::new(Kind::Stopped, identity, Vec::new(), &limits)?
                .write(&mut output, &limits);
        }
        match share.handle(&request, &limits) {
            Ok((kind, body)) => {
                Record::new(kind, identity, body, &limits)?.write(&mut output, &limits)?;
            }
            Err(error) => {
                refuse(identity, error, &mut output, &limits)?;
                return Err(error);
            }
        }
    }
    Ok(())
}

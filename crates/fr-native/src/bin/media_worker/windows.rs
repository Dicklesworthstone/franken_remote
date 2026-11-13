//! The Windows capture role (fr-p2-host-windows-rt1): one display through
//! DXGI Desktop Duplication into the adapter's hardware Media Foundation
//! HEVC encoder (`fr_native::windows_capture`), behind the same FRW0 records
//! as the Linux and macOS workers. The host discovers monitors, configures
//! one, then requests units. Encodes are synchronous, so a capture request is
//! answered with its unit (or unchanged evidence) and never with `NeedInput`.
use fr_core::{ids::DisplayGeometryGeneration, limits::ProtocolLimits};
use fr_media::worker::{
    self, Backend, Configuration, Error, Kind, Record, Role, Sequence, cursor::Observation,
};
use fr_native::{
    NativeError,
    windows_capture::{self, Captured, WinCapture, WinDisplay},
};
use fr_wire::display::{Catalog, Display};
use std::{fs::File, io, io::Write as _, os::windows::io::AsHandle};

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

/// The catalog entry for one display: pixels are the desktop's own (Desktop
/// Duplication captures them 1:1), logical size the pixels at 96 DPI.
fn entry(d: &WinDisplay, limits: &ProtocolLimits) -> Option<Display> {
    // Rotated outputs are refused by the capture for now.
    if d.rotation != 0 || d.dpi == 0 {
        return None;
    }
    let logical = |pixels: u32| {
        u32::try_from((u64::from(pixels) * 96 + u64::from(d.dpi) / 2) / u64::from(d.dpi)).ok()
    };
    let divisor = gcd(d.dpi, 96).max(1);
    let display = Display {
        handle: d.handle(),
        geometry: DisplayGeometryGeneration::INITIAL,
        x: d.left,
        y: d.top,
        pixel_width: d.width,
        pixel_height: d.height,
        logical_width: logical(d.width)?,
        logical_height: logical(d.height)?,
        scale_numerator: d.dpi / divisor,
        scale_denominator: 96 / divisor,
        rotation: 0,
    };
    display.validate(limits).ok()?;
    Some(display)
}

/// Capturable displays and their catalog. A display whose size cannot be
/// represented is left out rather than offered at a size it cannot have.
fn inventory(limits: &ProtocolLimits) -> Result<(Vec<WinDisplay>, Catalog), Error> {
    let displays: Vec<WinDisplay> = windows_capture::displays()
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

fn check_all(displays: &[WinDisplay]) -> Result<(), Error> {
    displays.iter().try_for_each(|d| d.check().map_err(native))
}

struct Share {
    capture: WinCapture,
    display: WinDisplay,
}
impl Drop for Share {
    fn drop(&mut self) {
        let stats = self.capture.statistics();
        // One content-free line per native owner. Stdout remains FRW0.
        let _ = writeln!(
            io::stderr().lock(),
            "capture_images={} capture_recreated={} capture_refused_desktops={} capture_encoded={} capture_idr={} capture_unchanged={} capture_refusal={}",
            stats.images,
            stats.recreated,
            stats.refused_desktops,
            stats.encoded,
            stats.idr,
            stats.unchanged,
            self.capture.refusal().unwrap_or("none")
        );
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
                match self
                    .capture
                    .capture(frame, observed, force_idr, only_if_changed)
                    .map_err(native)?
                {
                    Captured::Unit(unit) => Ok((Kind::Unit, worker::unit_payload(&unit)?)),
                    Captured::Unchanged(proof) => Ok((Kind::Unchanged, proof.encode()?)),
                }
            }
            _ => Err(Error::WrongState),
        }
    }
}

fn open(display: &WinDisplay, c: Configuration) -> Result<WinCapture, Error> {
    WinCapture::open(
        display,
        c.codec()?,
        c.limits()?,
        u32::from(c.fps),
        c.bitrate,
    )
    .map_err(native)
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
                if c.backend != Backend::MediaFoundation {
                    return Err(Error::Unsupported);
                }
                let display = *displays
                    .iter()
                    .find(|d| d.handle() == selected.handle)
                    .ok_or(Error::GeometryChanged)?;
                display.check().map_err(native)?;
                let capture = open(&display, c)?;
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

/// `--probe-encoder mediafoundation`: one forced IDR of the main display at
/// the size a share would use, through SEI stripping and admission.
fn probe_encoder(backend: &str) -> Result<(u32, u32, usize), Error> {
    if backend != "mediafoundation" {
        return Err(Error::Malformed);
    }
    let limits = ProtocolLimits::ABSOLUTE;
    let (displays, catalog) = inventory(&limits)?;
    // The main display is the one at the desktop origin.
    let main = displays
        .iter()
        .find(|d| d.left == 0 && d.top == 0)
        .or(displays.first())
        .ok_or(Error::Unsupported)?;
    let entry = catalog
        .displays()
        .iter()
        .find(|e| e.handle == main.handle())
        .ok_or(Error::Unsupported)?;
    let configuration = Configuration {
        width: entry.pixel_width,
        height: entry.pixel_height,
        fps: 60,
        backend: Backend::MediaFoundation,
        bitrate: 20_000_000,
        max_access_unit_bytes: limits.max_encoded_access_unit_bytes(),
        generation: fr_core::ids::CodecConfigurationGeneration::INITIAL,
    };
    let mut capture = open(main, configuration)?;
    match capture
        .capture(fr_media::access_unit::FrameId::from_raw(1), 1, true, false)
        .map_err(native)?
    {
        Captured::Unit(unit) => Ok((entry.pixel_width, entry.pixel_height, unit.bytes().len())),
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
    windows_capture::bind_parent(parent).map_err(native)?;
    if args.next().is_some() {
        return Err(Error::Malformed);
    }
    Ok(())
}

pub fn run() -> Result<(), Error> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("--probe-encoder") => {
            let backend = args.next().ok_or(Error::Malformed)?;
            parent(&mut args)?;
            return match probe_encoder(&backend) {
                Ok((width, height, bytes)) => {
                    println!(
                        "encoder_probe=admitted width={width} height={height} idr_bytes={bytes}"
                    );
                    Ok(())
                }
                Err(error) => {
                    println!("encoder_probe=refused reason={error}");
                    Err(error)
                }
            };
        }
        Some("--capture") => {}
        // Presentation belongs to the native client; no audio role yet.
        _ => return Err(Error::WrongRole),
    }
    let role = Role::Capture;
    parent(&mut args)?;
    // One unbuffered handle from the first record on, as on Linux.
    let mut input = File::from(
        io::stdin()
            .as_handle()
            .try_clone_to_owned()
            .map_err(|_| Error::Io)?,
    );
    let mut output = io::stdout().lock();
    let absolute = ProtocolLimits::ABSOLUTE;
    let first = Record::read(&mut input, &absolute)?.ok_or(Error::Io)?;
    let mut identity = first.header.identity;
    let mut sequence = Sequence::new(identity.epoch)?;
    sequence.accept(first.header)?;
    if role != Role::Capture || first.header.kind != Kind::DiscoverMonitors {
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

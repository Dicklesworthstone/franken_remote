#![forbid(unsafe_code)]
//! Private media process, launched through parent-owned anonymous pipes.
//! There is deliberately no socket, tailnet API, certificate or input handling.
#[cfg(target_os = "linux")]
mod linux {
    use fr_core::limits::ProtocolLimits;
    use fr_media::worker::{self, Backend, Configuration, Error, Kind, Record, Role, Sequence};
    use fr_native::{
        EncodeBackend, HevcDecoder, HevcEncoder, NativeError, X11Screens, X11Surface,
        bind_worker_parent,
        capture::{CaptureOutput, ChangeAwareCapture},
    };
    use std::io;

    // Fixed bounded native inventory stays inline in the single worker owner.
    #[allow(clippy::large_enum_variant)]
    enum Media {
        Capture(ChangeAwareCapture),
        Present {
            surface: X11Surface,
            codec: HevcDecoder,
            display_next: Option<bool>,
        },
    }
    fn native(e: NativeError) -> Error {
        match e {
            NativeError::GeometryChanged => Error::GeometryChanged,
            NativeError::Unavailable | NativeError::DisplayUnavailable => Error::Unsupported,
            NativeError::Allocation => Error::Allocation,
            _ => Error::NativeFailure,
        }
    }
    fn capture_media(surface: X11Surface, configuration: Configuration) -> Result<Media, Error> {
        if surface.width() != configuration.width || surface.height() != configuration.height {
            return Err(Error::GeometryChanged);
        }
        let backend = match configuration.backend {
            Backend::Nvenc => EncodeBackend::Nvenc,
            Backend::Vaapi => EncodeBackend::Vaapi,
            Backend::SoftwareExplicit => EncodeBackend::SoftwareExplicit,
        };
        let codec = HevcEncoder::new(
            configuration.codec()?,
            configuration.limits()?,
            backend,
            u32::from(configuration.fps),
            configuration.bitrate,
        )
        .map_err(native)?;
        Ok(Media::Capture(ChangeAwareCapture::new(surface, codec)))
    }
    fn open(
        role: Role,
        configuration: Configuration,
        record: Option<&[u8]>,
    ) -> Result<Media, Error> {
        let limits = configuration.limits()?;
        let config = configuration.codec()?;
        Ok(match role {
            Role::Capture => {
                let surface = X11Surface::capture(None, limits).map_err(native)?;
                capture_media(surface, configuration)?
            }
            Role::Present => Media::Present {
                surface: X11Surface::presenter(
                    None,
                    configuration.width,
                    configuration.height,
                    limits,
                )
                .map_err(native)?,
                codec: HevcDecoder::new(config, limits, record.ok_or(Error::WrongState)?)
                    .map_err(native)?,
                display_next: None,
            },
        })
    }
    impl Media {
        fn poll(&mut self, verify: bool) -> Result<(Kind, Vec<u8>), Error> {
            match self {
                Self::Capture(capture) => match capture.poll_output() {
                    Ok(unit) => Ok((Kind::Unit, worker::unit_payload(&unit)?)),
                    Err(NativeError::NeedInput) => Ok((Kind::NeedInput, Vec::new())),
                    Err(e) => Err(native(e)),
                },
                Self::Present {
                    surface,
                    codec,
                    display_next,
                } => match codec.poll_output() {
                    Ok((frame, pixels)) => {
                        let display = display_next.take().ok_or(Error::WrongState)?;
                        if !display {
                            return Ok((Kind::Decoded, frame.as_raw().to_be_bytes().to_vec()));
                        }
                        surface.present(&pixels).map_err(native)?;
                        // Explicit local verification mode only, never a remote peer option.
                        // Production does not read every presented frame back from the GPU/X server.
                        if verify && surface.snapshot().map_err(native)?.pixels() != pixels.pixels()
                        {
                            return Err(Error::NativeFailure);
                        }
                        // This acknowledges X11 submission/synchronization, not optical visibility.
                        Ok((Kind::Presented, frame.as_raw().to_be_bytes().to_vec()))
                    }
                    Err(NativeError::NeedInput) => Ok((Kind::NeedInput, Vec::new())),
                    Err(e) => Err(native(e)),
                },
            }
        }
        fn handle(
            &mut self,
            request: Record,
            limits: &ProtocolLimits,
            verify: bool,
        ) -> Result<(Kind, Vec<u8>), Error> {
            match request.header.kind {
                Kind::CheckMonitor => {
                    let Self::Capture(capture) = self else {
                        return Err(Error::WrongRole);
                    };
                    capture.check_display().map_err(native)?;
                    Ok((Kind::MonitorValid, Vec::new()))
                }
                Kind::Capture | Kind::CaptureIfChanged => {
                    let Self::Capture(capture) = self else {
                        return Err(Error::WrongRole);
                    };
                    let (id, capture_lower_bound, force_idr) =
                        worker::parse_capture(request.body())?;
                    match capture.capture(
                        id,
                        capture_lower_bound,
                        force_idr,
                        request.header.kind == Kind::CaptureIfChanged,
                    ) {
                        Ok(CaptureOutput::Submitted) => self.poll(verify),
                        Ok(CaptureOutput::Unchanged(evidence)) => {
                            Ok((Kind::Unchanged, evidence.encode()?))
                        }
                        Err(NativeError::NeedDrain) => Ok((Kind::NeedDrain, Vec::new())),
                        Err(e) => Err(native(e)),
                    }
                }
                Kind::Present | Kind::Decode => {
                    let Self::Present {
                        codec,
                        display_next,
                        ..
                    } = self
                    else {
                        return Err(Error::WrongRole);
                    };
                    if display_next.is_some() {
                        return Ok((Kind::NeedDrain, Vec::new()));
                    }
                    let display = request.header.kind == Kind::Present;
                    let unit = worker::parse_unit(request.into_body(), limits)?;
                    match codec.submit(&unit) {
                        Ok(()) => {
                            *display_next = Some(display);
                            self.poll(verify)
                        }
                        Err(NativeError::NeedDrain) => Ok((Kind::NeedDrain, Vec::new())),
                        Err(e) => Err(native(e)),
                    }
                }
                Kind::Poll => self.poll(verify),
                _ => Err(Error::WrongState),
            }
        }
    }
    type Initialized = (Configuration, Media, Kind, Vec<u8>);
    fn initialize(
        first: Record,
        role: Role,
        sequence: &mut Sequence,
        identity: &mut worker::Identity,
        input: &mut impl io::Read,
        output: &mut impl io::Write,
    ) -> Result<Option<Initialized>, Error> {
        if first.header.kind == Kind::DiscoverCapture {
            if role != Role::Capture {
                return Err(Error::WrongRole);
            }
            return discover_screens(sequence, identity, input, output);
        }
        if first.header.kind == Kind::DiscoverMonitors {
            if role != Role::Capture {
                return Err(Error::WrongRole);
            }
            return discover(sequence, identity, input, output);
        }
        let (configuration, media, ready) = match (role, first.header.kind) {
            (Role::Capture, Kind::Configure) => {
                let c = Configuration::decode(first.body())?;
                (c, open(role, c, None)?, Kind::Ready)
            }
            (Role::Present, Kind::ConfigureDecoder) => {
                let (c, record) = Configuration::decode_decoder(first.body())?;
                (c, open(role, c, Some(record))?, Kind::DecoderReady)
            }
            _ => return Err(Error::WrongState),
        };
        Ok(Some((configuration, media, ready, first.into_body())))
    }
    fn discover_screens(
        sequence: &mut Sequence,
        identity: &mut worker::Identity,
        input: &mut impl io::Read,
        output: &mut impl io::Write,
    ) -> Result<Option<Initialized>, Error> {
        let limits = ProtocolLimits::ABSOLUTE;
        let screens = X11Screens::open(None, limits).map_err(native)?;
        Record::new(
            Kind::CaptureScreens,
            *identity,
            screens.catalog().encode(),
            &limits,
        )?
        .write(output, &limits)?;
        let request = Record::read(input, &limits)?.ok_or(Error::Io)?;
        sequence.accept(request.header)?;
        *identity = request.header.identity;
        match request.header.kind {
            Kind::Stop => {
                drop(screens);
                Record::new(Kind::Stopped, *identity, Vec::new(), &limits)?
                    .write(output, &limits)?;
                Ok(None)
            }
            Kind::ConfigureCapture => {
                let (configuration, screen) = worker::capture::parse_configuration(request.body())?;
                let surface = screens.select(screen).map_err(native)?;
                let media = capture_media(surface, configuration)?;
                Ok(Some((
                    configuration,
                    media,
                    Kind::CaptureReady,
                    request.into_body(),
                )))
            }
            _ => Err(Error::WrongState),
        }
    }
    #[cfg(not(feature = "linux-displays"))]
    fn discover(
        _: &mut Sequence,
        _: &mut worker::Identity,
        _: &mut impl io::Read,
        _: &mut impl io::Write,
    ) -> Result<Option<Initialized>, Error> {
        Err(Error::Unsupported)
    }
    #[cfg(feature = "linux-displays")]
    fn discover(
        sequence: &mut Sequence,
        identity: &mut worker::Identity,
        input: &mut impl io::Read,
        output: &mut impl io::Write,
    ) -> Result<Option<Initialized>, Error> {
        let limits = ProtocolLimits::ABSOLUTE;
        let selector = std::env::var("DISPLAY").map_err(|_| Error::Unsupported)?;
        let mut inventory =
            fr_native::displays::X11Inventory::open(&selector, limits).map_err(native)?;
        let catalog = inventory.catalog().map_err(native)?;
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
                    inventory.revalidate().map_err(native)?;
                    Record::new(Kind::MonitorValid, *identity, Vec::new(), &limits)?
                        .write(output, &limits)?;
                }
                Kind::Stop => {
                    drop(inventory);
                    Record::new(Kind::Stopped, *identity, Vec::new(), &limits)?
                        .write(output, &limits)?;
                    return Ok(None);
                }
                Kind::ConfigureMonitor => {
                    let (c, selected) =
                        worker::capture::monitors::decode_configuration(request.body(), catalog)?;
                    let surface = inventory.select(selected.handle).map_err(native)?;
                    let backend = match c.backend {
                        Backend::Nvenc => EncodeBackend::Nvenc,
                        Backend::Vaapi => EncodeBackend::Vaapi,
                        Backend::SoftwareExplicit => EncodeBackend::SoftwareExplicit,
                    };
                    let codec = HevcEncoder::new(
                        c.codec()?,
                        c.limits()?,
                        backend,
                        u32::from(c.fps),
                        c.bitrate,
                    )
                    .map_err(native)?;
                    let mut capture = ChangeAwareCapture::selected(surface, codec);
                    // Configuration work can block; check topology again before Ready.
                    capture.check_display().map_err(native)?;
                    return Ok(Some((
                        c,
                        Media::Capture(capture),
                        Kind::MonitorReady,
                        request.into_body(),
                    )));
                }
                _ => return Err(Error::WrongState),
            }
        }
    }
    pub fn run() -> Result<(), Error> {
        let mut args = std::env::args().skip(1);
        let role = match args.next().as_deref() {
            Some("--capture") => Role::Capture,
            Some("--present") => Role::Present,
            _ => return Err(Error::WrongRole),
        };
        if args.next().as_deref() != Some("--parent-pid") {
            return Err(Error::WrongState);
        }
        let parent = args
            .next()
            .ok_or(Error::WrongState)?
            .parse::<u32>()
            .map_err(|_| Error::WrongState)?;
        bind_worker_parent(parent).map_err(native)?;
        let verify = match args.next().as_deref() {
            None => false,
            Some("--verify-readback") if role == Role::Present => true,
            _ => return Err(Error::Malformed),
        };
        if args.next().is_some() {
            return Err(Error::Malformed);
        }
        let (stdin, stdout) = (io::stdin(), io::stdout());
        let (mut input, mut output) = (stdin.lock(), stdout.lock());
        let absolute = ProtocolLimits::ABSOLUTE;
        let first = Record::read(&mut input, &absolute)?.ok_or(Error::Io)?;
        let mut identity = first.header.identity;
        let mut sequence = Sequence::new(identity.epoch)?;
        sequence.accept(first.header)?;
        let initialized = initialize(
            first,
            role,
            &mut sequence,
            &mut identity,
            &mut input,
            &mut output,
        );
        let Some((configuration, mut media, ready, body)) = (match initialized {
            Ok(value) => value,
            Err(error) => {
                Record::new(
                    Kind::Refused,
                    identity,
                    (error as u16).to_be_bytes().to_vec(),
                    &absolute,
                )?
                .write(&mut output, &absolute)?;
                return Err(error);
            }
        }) else {
            return Ok(());
        };
        let limits = configuration.limits()?;
        Record::new(ready, identity, body, &limits)?.write(&mut output, &limits)?;
        while let Some(request) = Record::read(&mut input, &limits)? {
            let identity = request.header.identity;
            sequence.accept(request.header)?;
            if request.header.kind == Kind::Stop {
                // Destroy foreign contexts before acknowledging a completed stop.
                drop(media);
                return Record::new(Kind::Stopped, identity, Vec::new(), &limits)?
                    .write(&mut output, &limits);
            }
            match media.handle(request, &limits, verify) {
                Ok((kind, body)) => {
                    Record::new(kind, identity, body, &limits)?.write(&mut output, &limits)?;
                }
                Err(error) => {
                    Record::new(
                        Kind::Refused,
                        identity,
                        (error as u16).to_be_bytes().to_vec(),
                        &limits,
                    )?
                    .write(&mut output, &limits)?;
                    return Err(error);
                }
            }
        }
        // Parent death/pipe closure drops all native owners, even without Stop.
        Ok(())
    }
}
fn main() {
    #[cfg(target_os = "linux")]
    if let Err(error) = linux::run() {
        eprintln!("media_worker_refused={error}");
        std::process::exit(2);
    }
    #[cfg(not(target_os = "linux"))]
    {
        eprintln!("media_worker_refused=UnsupportedPlatform");
        std::process::exit(2);
    }
}

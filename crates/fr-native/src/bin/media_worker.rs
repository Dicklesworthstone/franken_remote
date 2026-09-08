#![forbid(unsafe_code)]
//! Private media process, launched through parent-owned anonymous pipes.
//! There is deliberately no socket, tailnet API, certificate or input handling.
#[cfg(target_os = "linux")]
mod linux {
    use fr_core::limits::ProtocolLimits;
    use fr_media::worker::{self, Backend, Configuration, Error, Kind, Record, Role, Sequence};
    use fr_native::{
        EncodeBackend, HevcDecoder, HevcEncoder, NativeError, X11Surface, bind_worker_parent,
    };
    use std::io;

    enum Media {
        Capture {
            surface: X11Surface,
            codec: HevcEncoder,
        },
        Present {
            surface: X11Surface,
            codec: HevcDecoder,
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
    fn open(role: Role, configuration: Configuration) -> Result<Media, Error> {
        let limits = configuration.limits()?;
        let config = configuration.codec()?;
        Ok(match role {
            Role::Capture => {
                let surface = X11Surface::capture(None, limits).map_err(native)?;
                if surface.width() != configuration.width
                    || surface.height() != configuration.height
                {
                    return Err(Error::GeometryChanged);
                }
                let backend = match configuration.backend {
                    Backend::Nvenc => EncodeBackend::Nvenc,
                    Backend::Vaapi => EncodeBackend::Vaapi,
                    Backend::SoftwareExplicit => EncodeBackend::SoftwareExplicit,
                };
                let codec = HevcEncoder::new(
                    config,
                    limits,
                    backend,
                    u32::from(configuration.fps),
                    configuration.bitrate,
                )
                .map_err(native)?;
                Media::Capture { surface, codec }
            }
            Role::Present => Media::Present {
                surface: X11Surface::presenter(
                    None,
                    configuration.width,
                    configuration.height,
                    limits,
                )
                .map_err(native)?,
                codec: HevcDecoder::new(config, limits).map_err(native)?,
            },
        })
    }
    impl Media {
        fn poll(&mut self, verify: bool) -> Result<(Kind, Vec<u8>), Error> {
            match self {
                Self::Capture { codec, .. } => match codec.poll_output() {
                    Ok(unit) => Ok((Kind::Unit, worker::unit_payload(&unit)?)),
                    Err(NativeError::NeedInput) => Ok((Kind::NeedInput, Vec::new())),
                    Err(e) => Err(native(e)),
                },
                Self::Present { surface, codec } => match codec.poll_output() {
                    Ok((frame, pixels)) => {
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
                Kind::Capture => {
                    let Self::Capture { surface, codec } = self else {
                        return Err(Error::WrongRole);
                    };
                    let (id, capture_lower_bound, force_idr) =
                        worker::parse_capture(request.body())?;
                    let pixels = surface.snapshot().map_err(native)?;
                    match codec.submit(&pixels, id, capture_lower_bound, force_idr) {
                        Ok(()) => self.poll(verify),
                        Err(NativeError::NeedDrain) => Ok((Kind::NeedDrain, Vec::new())),
                        Err(e) => Err(native(e)),
                    }
                }
                Kind::Present => {
                    let Self::Present { codec, .. } = self else {
                        return Err(Error::WrongRole);
                    };
                    let unit = worker::parse_unit(request.into_body(), limits)?;
                    match codec.submit(&unit) {
                        Ok(()) => self.poll(verify),
                        Err(NativeError::NeedDrain) => Ok((Kind::NeedDrain, Vec::new())),
                        Err(e) => Err(native(e)),
                    }
                }
                Kind::Poll => self.poll(verify),
                _ => Err(Error::WrongState),
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
        let first_identity = first.header.identity;
        let mut sequence = Sequence::new(first_identity.epoch)?;
        sequence.accept(first.header)?;
        if first.header.kind != Kind::Configure {
            return Err(Error::WrongState);
        }
        let initialized =
            Configuration::decode(first.body()).and_then(|c| open(role, c).map(|m| (c, m)));
        let (configuration, mut media) = match initialized {
            Ok(value) => value,
            Err(error) => {
                Record::new(
                    Kind::Refused,
                    first_identity,
                    (error as u16).to_be_bytes().to_vec(),
                    &absolute,
                )?
                .write(&mut output, &absolute)?;
                return Err(error);
            }
        };
        let limits = configuration.limits()?;
        Record::new(
            Kind::Ready,
            first_identity,
            configuration.encode()?,
            &limits,
        )?
        .write(&mut output, &limits)?;
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

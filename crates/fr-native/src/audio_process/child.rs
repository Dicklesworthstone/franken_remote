//! Private child's entry: one explicit native source, no device aliases,
//! transport, authority endpoint, default source or native fallback.
use super::{
    Error,
    protocol::{self, CONFIGURE, Header, RENEW},
};
use crate::pulse::microphone::MicrophoneSource;
use std::{
    io::{Read, Write},
    os::{fd::AsFd, unix::net::UnixStream},
    time::{Duration, Instant},
};

/// The original parent holds the sole other end. A byte, EOF, socket error,
/// unknown clock or elapsed installed deadline all permanently withdraw capture.
/// Only an in-sequence renewal accepted before that deadline can replace it.
struct Guard {
    channel: UnixStream,
    authorization_ns: u64,
    denied: bool,
}
impl Guard {
    fn permitted(&mut self, expires_ns: u64) -> bool {
        if self.denied
            || crate::clock::monotonic_ns()
                .is_none_or(|now| now >= self.authorization_ns.min(expires_ns))
        {
            self.denied = true;
            return false;
        }
        let mut byte = [0];
        if !matches!(self.channel.read(&mut byte), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
        {
            self.denied = true;
        }
        !self.denied
    }
}

/// Called only inside the installed standalone `fr-audio-worker`. Parent death
/// binding is installed before any native library/device initialization. This
/// is process isolation, not a kernel sandbox for a same-user audio worker.
pub fn run(parent: u32) -> Result<(), Error> {
    crate::bind_parent(parent).map_err(|_| Error::Spawn)?;
    let guard = std::io::stderr()
        .as_fd()
        .try_clone_to_owned()
        .map_err(|_| Error::Protocol)?;
    let channel = UnixStream::from(guard);
    channel.peer_addr().map_err(|_| Error::Protocol)?;
    channel.set_nonblocking(true).map_err(|_| Error::Protocol)?;
    serve(
        &mut std::io::stdin().lock(),
        &mut std::io::stdout().lock(),
        channel,
    )
}

fn refuse(output: &mut impl Write, mut request: Header, error: Error) -> Result<(), Error> {
    request.error = Some(error);
    request.bytes = 0;
    request.write(output, true, &[])?;
    Err(error)
}

fn serve(input: &mut impl Read, output: &mut impl Write, channel: UnixStream) -> Result<(), Error> {
    let first = Header::read(input, false)?;
    let body = protocol::body(input, first)?;
    let options = protocol::read_configuration(first, &body)?;
    let mut guard = Guard {
        channel,
        authorization_ns: first.authorization_ns,
        denied: false,
    };
    if first.kind != CONFIGURE || !guard.permitted(first.expires_ns) {
        return refuse(output, first, Error::Denied);
    }
    let initialized = (|| {
        let mut source = MicrophoneSource::connect(
            &options.server,
            &options.source,
            options.configuration,
            options.bitrate,
            options.max_packet_bytes,
            || guard.permitted(first.expires_ns),
        )
        .map_err(Error::from)?;
        while !source
            .poll_ready(|| guard.permitted(first.expires_ns))
            .map_err(Error::from)?
        {
            std::thread::sleep(Duration::from_millis(1));
        }
        if !guard.permitted(first.expires_ns) {
            return Err(Error::Denied);
        }
        Ok(source)
    })();
    let mut source = match initialized {
        Ok(source) => source,
        Err(error) => return refuse(output, first, error),
    };
    first.write(output, true, &body)?;
    output.flush().map_err(|_| Error::Native)?;
    let mut serial = 1_u64;
    loop {
        let request = Header::read(input, false)?;
        serial = serial.checked_add(1).ok_or(Error::Protocol)?;
        if !request.follows(serial, first.generation, guard.authorization_ns) {
            source.close();
            return refuse(output, request, Error::Protocol);
        }
        if request.kind == RENEW {
            // No native call or PCM mutation occurs in this exchange. A partial
            // frame keeps the original pull's expiry across the new future cap.
            if !guard.permitted(request.expires_ns) {
                source.close();
                return refuse(output, request, Error::Denied);
            }
            guard.authorization_ns = request.authorization_ns;
            if !guard.permitted(request.expires_ns) {
                source.close();
                return refuse(output, request, Error::Denied);
            }
            request.write(output, true, &[])?;
            output.flush().map_err(|_| Error::Native)?;
            continue;
        }
        let result = (|| {
            if !guard.permitted(request.expires_ns) {
                return Err(Error::Denied);
            }
            let expires = crate::clock::local_instant(request.expires_ns).ok_or(Error::Clock)?;
            if Instant::now() >= expires {
                return Err(Error::Expired);
            }
            let batch = source
                .pull(expires, || guard.permitted(request.expires_ns))
                .map_err(Error::from)?;
            let original = batch.expires();
            let counters = batch.counters();
            let units = batch
                .consume(|| guard.permitted(request.expires_ns))
                .map_err(Error::from)?;
            let body = protocol::packets(&units, counters, &options)?;
            let mut header = request;
            header.expires_ns = crate::clock::stamp_ns(original)
                .ok_or(Error::Clock)?
                .min(request.expires_ns);
            header.bytes = u32::try_from(body.len()).map_err(|_| Error::Protocol)?;
            if !guard.permitted(header.expires_ns) {
                return Err(Error::Denied);
            }
            Ok((header, body))
        })();
        match result {
            Ok((header, body)) => {
                header.write(output, true, &body)?;
                output.flush().map_err(|_| Error::Native)?;
            }
            Err(error) => {
                source.close();
                return refuse(output, request, error);
            }
        }
    }
}

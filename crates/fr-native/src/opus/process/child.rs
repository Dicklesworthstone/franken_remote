//! Standalone child's entry. No device, network, application callback or grant.
use super::{
    AudioMediaError as Error,
    protocol::{self, CONFIG, Header, PACKET, PLC},
};
use crate::opus::Decoder;
use fr_media::audio::AudioDecoder;
use std::io::{Read, Write};
#[link(name = "fropussandbox", kind = "static")]
unsafe extern "C" {
    fn fr_opus_sandbox_enter() -> std::ffi::c_int;
}
/// Permanently restrict this SINGLE-THREAD private child before reading any
/// command or entering libopus. It must have its parent's connected socket on
/// stdin/stdout. All other descriptors close. This changes process-wide state;
/// never call inside the client, daemon, test runner or a reusable worker.
pub fn confine() -> Result<(), Error> {
    // SAFETY: no pointers, borrowed buffers or callbacks. Kernel-only process
    // restrictions; exact success required, no unrestricted fallback.
    if unsafe { fr_opus_sandbox_enter() } == 1 {
        Ok(())
    } else {
        Err(Error::Fatal)
    }
}
pub fn run(parent: u32) -> Result<(), Error> {
    crate::bind_parent(parent).map_err(|_| Error::Fatal)?;
    confine()?;
    serve(&mut std::io::stdin().lock(), &mut std::io::stdout().lock())
}
fn serve(input: &mut impl Read, output: &mut impl Write) -> Result<(), Error> {
    let header = Header::read(input, false)?;
    let (config, limits) = protocol::read_config(header, input)?;
    let mut decoder = Decoder::with_limits(limits);
    decoder.configure(config)?;
    protocol::write(output, header.config_reply(), true, &[])?;
    output.flush().map_err(|_| Error::Fatal)?;
    let mut serial = 1u64;
    let mut next = None;
    loop {
        let h = Header::read(input, false)?;
        serial = serial.checked_add(1).ok_or(Error::BufferOverflow)?;
        if h.serial != serial || !h.matches(config) || h.kind == CONFIG {
            return Err(Error::InvalidPayload);
        }
        let mut pcm = match h.kind {
            PACKET => {
                let p = protocol::read_packet(h, limits, input)?;
                decoder.submit_packet(&p)?;
                decoder.poll_pcm()?.ok_or(Error::Fatal)?
            }
            PLC if h.bytes == 0 && next == Some((h.sequence, h.at)) => {
                decoder.decode_plc(h.samples)?
            }
            _ => return Err(Error::InvalidPayload),
        };
        next = Some((
            h.sequence.checked_add(1).ok_or(Error::BufferOverflow)?,
            h.at.checked_add(u64::from(h.samples))
                .ok_or(Error::BufferOverflow)?,
        ));
        let result = protocol::write_pcm(output, h, &pcm);
        pcm.samples_mut().fill(0);
        result?;
        output.flush().map_err(|_| Error::Fatal)?;
    }
}

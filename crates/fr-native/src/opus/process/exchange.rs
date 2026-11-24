//! One private protocol for all supervised native Opus children.
//!
//! The endpoint owns its platform's bounded, stop-aware I/O. A new deadline is
//! installed only for a new mailbox request, never for a partial read/write.
#![forbid(unsafe_code)]
use super::{
    AudioMediaError as Error, AudioStreamConfig, CodecLimits, Instant, Job, Reply, Shared,
    protocol::{self, CONFIG, Header, PACKET, PLC},
};
use std::io::{Read, Write};

pub(super) trait Endpoint: Read + Write {
    fn set_deadline(&mut self, until: Instant);
}

pub(super) fn serve(
    c: AudioStreamConfig,
    l: CodecLimits,
    shared: &Shared,
    exchange: &mut impl Endpoint,
) -> Result<(), Error> {
    let mut h = Header::new(c, 1, CONFIG)?;
    h.bytes = 8;
    protocol::write(exchange, h, false, &protocol::config_body(c, l)?)?;
    if Header::read(exchange, true)? != h.config_reply() {
        return Err(Error::InvalidPayload);
    }
    shared.reply(Ok(Reply::Ready));
    let mut serial = 1u64;
    let mut next = None;
    loop {
        let job = {
            let mut mail = shared.mail.lock().map_err(|_| Error::Fatal)?;
            while mail.job.is_none() && !mail.stop {
                mail = shared.wake.wait(mail).map_err(|_| Error::Fatal)?;
            }
            if mail.stop {
                return Ok(());
            }
            mail.job.take().ok_or(Error::Fatal)?
        };
        serial = serial.checked_add(1).ok_or(Error::BufferOverflow)?;
        exchange.set_deadline(job.1);
        let mut h = Header::new(c, serial, PACKET)?;
        match job.0 {
            Job::Packet(p) => {
                h.sequence = p.sequence();
                h.at = p.timestamp_samples();
                h.bytes = u32::try_from(p.payload().len()).map_err(|_| Error::BufferOverflow)?;
                protocol::write(exchange, h, false, p.payload())?;
            }
            Job::Plc(samples) => {
                h.kind = PLC;
                h.samples = samples;
                let (seq, at) = next.ok_or(Error::NeedMoreInput)?;
                h.sequence = seq;
                h.at = at;
                protocol::write(exchange, h, false, &[])?;
            }
        }
        let pcm = protocol::read_pcm(exchange, h)?;
        next = Some((
            h.sequence.checked_add(1).ok_or(Error::BufferOverflow)?,
            h.at.checked_add(u64::from(h.samples))
                .ok_or(Error::BufferOverflow)?,
        ));
        shared.reply(Ok(Reply::Pcm(Box::new(pcm))));
    }
}

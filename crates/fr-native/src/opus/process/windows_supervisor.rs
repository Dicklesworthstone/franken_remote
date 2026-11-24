//! One admitted owner performs all Windows process, pipe and cleanup work.
//! The client observes only the original mailbox and actual retirement receipt.
use super::{
    Arc, AudioMediaError as Error, AudioStreamConfig, CodecLimits, Instant, Ordering, Path,
    PathBuf, Shared, Slot, exchange,
    windows::{LaunchError, Pipe, Process},
    windows_policy::Deadline,
};
use std::io::{self, Read, Write};

struct Owner<'a> {
    process: Option<Process>,
    shared: &'a Shared,
    slot: Option<Slot>,
    uncertain: bool,
}
impl Drop for Owner<'_> {
    fn drop(&mut self) {
        let complete = match self.process.take() {
            Some(process) => {
                let complete = process.stop_and_wait();
                // Close the original job, process handle and image pins before
                // publishing retirement. No handle from a replacement is used.
                drop(process);
                complete
            }
            None => !self.uncertain,
        };
        if complete {
            self.shared.pid.store(0, Ordering::Release);
            self.shared.retired.store(true, Ordering::Release);
        } else if let Some(slot) = self.slot.take() {
            // A stop request/closed job is not proof of exit. Uncertain cleanup
            // permanently consumes one of the four slots, with no reaper queue.
            std::mem::forget(slot);
        }
    }
}

struct Exchange<'a> {
    pipe: Pipe,
    deadline: Deadline<'a>,
}
impl Read for Exchange<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        loop {
            self.deadline.live()?;
            let result = self.pipe.read(bytes);
            self.deadline.live()?;
            match result {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => self.deadline.wait()?,
                result => return result,
            }
        }
    }
}
impl Write for Exchange<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        loop {
            self.deadline.live()?;
            let result = self.pipe.write(bytes);
            self.deadline.live()?;
            match result {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => self.deadline.wait()?,
                result => return result,
            }
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        self.deadline.live().map(|_| ())
    }
}
impl exchange::Endpoint for Exchange<'_> {
    fn set_deadline(&mut self, until: Instant) {
        self.deadline.set(until);
    }
}

// This common platform entry transfers the configured path into its admitted
// owner thread. Its borrowed implementation below cannot outlive that owner.
#[allow(clippy::needless_pass_by_value)]
pub(super) fn run(
    image: PathBuf,
    config: AudioStreamConfig,
    limits: CodecLimits,
    until: Instant,
    shared: &Arc<Shared>,
    slot: Slot,
) {
    let result = serve(&image, config, limits, until, shared, slot);
    if let Err(error) = result {
        shared.reply(Err(error));
    }
}
fn serve(
    image: &Path,
    config: AudioStreamConfig,
    limits: CodecLimits,
    until: Instant,
    shared: &Shared,
    slot: Slot,
) -> Result<(), Error> {
    let mut owner = Owner {
        process: None,
        shared,
        slot: Some(slot),
        uncertain: false,
    };
    let deadline = Deadline::new(shared, until);
    let (process, pipe) = match Process::spawn(image, &|| deadline.live().map(|_| ())) {
        Ok(child) => child,
        Err(LaunchError { error, quiescent }) => {
            owner.uncertain = !quiescent;
            return Err(match error.kind() {
                io::ErrorKind::InvalidInput | io::ErrorKind::PermissionDenied => {
                    Error::UnsupportedFormat
                }
                _ => Error::Fatal,
            });
        }
    };
    shared.pid.store(process.id(), Ordering::Release);
    owner.process = Some(process);
    let mut exchange = Exchange { pipe, deadline };
    // A stop or expired setup while CreateProcessW was running cannot publish
    // readiness or use a newly refreshed deadline. Owner still retires its child.
    exchange.deadline.live().map_err(|_| Error::Fatal)?;
    exchange::serve(config, limits, shared, &mut exchange)
}

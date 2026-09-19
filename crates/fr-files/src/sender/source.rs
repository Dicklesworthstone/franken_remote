//! One selected file descriptor, disk-only hashing/reads, one outstanding result.
use super::{Error, Policy};
use crate::receive::MAX_CHUNK_BYTES;
use asupersync::{
    cx::Cx,
    net::atp::{
        transport_common::{StagedEntryReceive, flat_merkle_root_from_digests, hex_encode},
        transport_tcp::{ManifestEntry, TransferManifest},
    },
    time::TimerDriverHandle,
};
use std::{
    fs::{File, Metadata},
    io::{Read, Seek},
    os::unix::fs::MetadataExt,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

pub(super) enum Event {
    Prepared(Box<TransferManifest>),
    Chunk { offset: u64, bytes: Vec<u8> },
    End,
}
struct Gate {
    stop: Arc<AtomicBool>,
    cx: Cx,
    clock: TimerDriverHandle,
    deadline: u64,
}
impl Gate {
    fn check(&self) -> Result<(), Error> {
        if self.stop.load(Ordering::Acquire) || self.cx.is_cancel_requested() {
            return Err(Error::Cancelled);
        }
        if self.clock.now().as_nanos() / 1000 >= self.deadline {
            return Err(Error::Expired);
        }
        Ok(())
    }
}

pub(super) struct Source {
    command: SyncSender<usize>,
    result: Receiver<Result<Event, Error>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    pending: bool,
}
impl Source {
    pub(super) fn spawn(
        cx: Cx,
        file: File,
        name: String,
        policy: Policy,
        deadline: u64,
    ) -> Result<Self, Error> {
        let clock = cx.timer_driver().ok_or(Error::Clock)?;
        let (send, command) = mpsc::sync_channel(1);
        let (reply, result) = mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let gate = Gate {
            stop: stop.clone(),
            cx,
            clock,
            deadline,
        };
        let thread = thread::Builder::new()
            .name("fr-file-source".into())
            .spawn(move || {
                if let Err(error) = run(file, name, policy, &gate, &command, &reply) {
                    // There cannot be a second unread result: request admission is
                    // exclusive until the previous result is taken. Never block on
                    // the abandoned network owner, including during cancellation.
                    let _ = reply.try_send(Err(error));
                }
            })
            .map_err(|_| Error::Spawn)?;
        Ok(Self {
            command: send,
            result,
            stop,
            thread: Some(thread),
            pending: true,
        })
    }
    pub(super) fn poll(&mut self) -> Result<Option<Event>, Error> {
        if !self.pending {
            return Ok(None);
        }
        match self.result.try_recv() {
            Ok(event) => {
                self.pending = false;
                event.map(Some)
            }
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(Error::Worker),
        }
    }
    pub(super) fn request(&mut self, maximum: usize) -> Result<(), Error> {
        if self.pending {
            return Err(Error::Busy);
        }
        if !(1..=MAX_CHUNK_BYTES).contains(&maximum) {
            return Err(Error::Limits);
        }
        self.command.try_send(maximum).map_err(|_| Error::Worker)?;
        self.pending = true;
        Ok(())
    }
    pub(super) fn pending(&self) -> bool {
        self.pending
    }
    pub(super) fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }
    pub(super) fn finished(&self) -> bool {
        self.thread.as_ref().is_none_or(JoinHandle::is_finished)
    }
    pub(super) fn reap(&mut self) -> Option<Result<(), Error>> {
        if !self.finished() {
            return None;
        }
        self.thread
            .take()
            .map(|thread| thread.join().map_err(|_| Error::Worker))
    }
}
impl Drop for Source {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(PartialEq, Eq)]
struct Identity(u64, u64, u64, i64, i64, i64, i64);
fn identity(m: &Metadata) -> Identity {
    Identity(
        m.dev(),
        m.ino(),
        m.len(),
        m.mtime(),
        m.mtime_nsec(),
        m.ctime(),
        m.ctime_nsec(),
    )
}
fn unchanged(file: &File, original: &Identity) -> Result<(), Error> {
    if identity(&file.metadata().map_err(Error::from)?) != *original {
        return Err(Error::SourceChanged);
    }
    Ok(())
}
// Some proc-style regular files report zero length while returning data. Never
// silently send an empty/truncated file based only on the metadata size.
fn eof(file: &mut File) -> Result<(), Error> {
    if file.read(&mut [0_u8; 1]).map_err(Error::from)? != 0 {
        return Err(Error::SourceChanged);
    }
    Ok(())
}
fn run(
    mut file: File,
    name: String,
    policy: Policy,
    gate: &Gate,
    command: &Receiver<usize>,
    reply: &SyncSender<Result<Event, Error>>,
) -> Result<(), Error> {
    gate.check()?;
    let metadata = file.metadata().map_err(Error::from)?;
    if !metadata.is_file() || metadata.len() > policy.max_file_bytes {
        return Err(Error::Source);
    }
    let original = identity(&metadata);
    let size = metadata.len();
    file.rewind().map_err(Error::from)?;
    let mut hash = StagedEntryReceive::new(PathBuf::new());
    let mut buffer = vec![0_u8; MAX_CHUNK_BYTES];
    let mut offset = 0;
    while offset < size {
        gate.check()?;
        let n = usize::try_from((size - offset).min(MAX_CHUNK_BYTES as u64))
            .map_err(|_| Error::Limits)?;
        file.read_exact(&mut buffer[..n]).map_err(Error::from)?;
        hash.update_with_chunk(&buffer[..n]);
        offset += n as u64;
    }
    buffer.fill(0);
    eof(&mut file)?;
    unchanged(&file, &original)?;
    gate.check()?;
    let (digest, _, _) = hash.finalize(name.clone());
    let expected = digest.content_sha256;
    let root = flat_merkle_root_from_digests(std::slice::from_ref(&digest));
    let mut random = [0; 16];
    getrandom::fill(&mut random).map_err(|_| Error::Source)?;
    let manifest = TransferManifest {
        transfer_id: hex_encode(&random),
        root_name: name.clone(),
        is_directory: false,
        total_bytes: size,
        merkle_root_hex: root,
        metadata_root_hex: None,
        directory_metadata: None,
        delta_manifest: None,
        entries: vec![ManifestEntry {
            index: 0,
            rel_path: name.clone(),
            size,
            sha256_hex: hex_encode(&expected),
            metadata: None,
            members: Vec::new(),
        }],
    };
    file.rewind().map_err(Error::from)?;
    reply
        .try_send(Ok(Event::Prepared(Box::new(manifest))))
        .map_err(|_| Error::Worker)?;
    offset = 0;
    let mut verification = StagedEntryReceive::new(PathBuf::new());
    loop {
        gate.check()?;
        let maximum = match command.recv_timeout(Duration::from_millis(10)) {
            Ok(maximum) => maximum,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        };
        gate.check()?;
        unchanged(&file, &original)?;
        if offset == size {
            eof(&mut file)?;
            let (actual, _, _) = verification.finalize(name);
            if actual.content_sha256 != expected {
                return Err(Error::SourceChanged);
            }
            gate.check()?;
            reply.try_send(Ok(Event::End)).map_err(|_| Error::Worker)?;
            return Ok(());
        }
        let n = usize::try_from((size - offset).min(maximum as u64)).map_err(|_| Error::Limits)?;
        let mut bytes = vec![0; n];
        file.read_exact(&mut bytes).map_err(Error::from)?;
        verification.update_with_chunk(&bytes);
        gate.check()?;
        unchanged(&file, &original)?;
        reply
            .try_send(Ok(Event::Chunk { offset, bytes }))
            .map_err(|_| Error::Worker)?;
        offset += n as u64;
    }
}

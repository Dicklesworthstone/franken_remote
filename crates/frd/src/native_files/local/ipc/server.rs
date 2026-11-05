//! One socket supervisor, one selection worker and one fixed cleanup reaper.
//! No per-peer threads and no blocking join on the caller/authority thread.
use super::{
    Arc, Error, HANDSHAKE, Handler, Info, MAX_PEERS, Scope, Status, TICK, check_peer, check_time,
    name, wire,
};
use std::{
    io::{self, Read, Write},
    os::{
        linux::net::SocketAddrExt,
        unix::net::{SocketAddr, UnixListener, UnixStream},
    },
    panic::{AssertUnwindSafe, catch_unwind},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError},
    },
    thread,
    time::Instant,
};

struct Shared {
    handler: Arc<dyn Handler>,
    stopped: AtomicBool,
    selecting: AtomicBool,
}
impl Shared {
    fn stop(&self) {
        if !self.stopped.swap(true, Ordering::AcqRel) {
            self.handler.close();
        }
    }
}
struct Finished {
    shared: Arc<Shared>,
}
impl Drop for Finished {
    fn drop(&mut self) {
        self.shared.stop();
    }
}

struct Reaper {
    joined: Arc<AtomicBool>,
    // This helper owns no intake, paths, socket or application callback. Its
    // receipt is published only after BOTH original worker joins have returned.
    _thread: thread::JoinHandle<()>,
}
impl Reaper {
    fn start() -> Result<(Self, SyncSender<thread::JoinHandle<()>>), Error> {
        let (send, receive) = mpsc::sync_channel::<thread::JoinHandle<()>>(2);
        let joined = Arc::new(AtomicBool::new(false));
        let complete = Arc::clone(&joined);
        let thread = thread::Builder::new()
            .name("fr-local-file-cleanup".into())
            .spawn(move || {
                for original in receive {
                    let _ = original.join();
                }
                complete.store(true, Ordering::Release);
            })
            .map_err(|_| Error::Unavailable)?;
        Ok((
            Self {
                joined,
                _thread: thread,
            },
            send,
        ))
    }
}

pub(in super::super) struct Owner {
    shared: Arc<Shared>,
    target: String,
    instance: [u8; 16],
    io: thread::Thread,
    cleanup: Reaper,
}
impl Owner {
    pub(in super::super) fn bind(target: &str, handler: Arc<dyn Handler>) -> Result<Self, Error> {
        Self::bind_scoped(Scope::Host, target, handler)
    }
    pub(in super::super) fn bind_upload(
        connection: &str,
        handler: Arc<dyn Handler>,
    ) -> Result<Self, Error> {
        Self::bind_scoped(Scope::Upload, connection, handler)
    }
    fn bind_scoped(scope: Scope, target: &str, handler: Arc<dyn Handler>) -> Result<Self, Error> {
        scope.check_target(target)?;
        let uid = rustix::process::geteuid().as_raw();
        let address = SocketAddr::from_abstract_name(name(scope, uid, target))
            .map_err(|_| Error::InvalidTarget)?;
        let listener = match UnixListener::bind_addr(&address) {
            Ok(listener) => listener,
            Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
                // Public abstract-name occupancy is not proof of a trusted owner.
                let existing = super::connect(scope, uid, target)?;
                check_peer(&existing, uid)?;
                return Err(Error::AlreadyRunning);
            }
            Err(_) => return Err(Error::Unavailable),
        };
        listener
            .set_nonblocking(true)
            .map_err(|_| Error::Unavailable)?;
        rustix::net::listen(
            &listener,
            i32::try_from(MAX_PEERS).expect("fixed peer bound"),
        )
        .map_err(|_| Error::Unavailable)?;
        let mut instance = [0; 16];
        getrandom::fill(&mut instance).map_err(|_| Error::Unavailable)?;
        let shared = Arc::new(Shared {
            handler,
            stopped: AtomicBool::new(false),
            selecting: AtomicBool::new(false),
        });
        // Start the cleanup owner before either worker. Startup refusal can
        // therefore fence work and leave its actual handle with this reaper.
        let (cleanup, originals) = Reaper::start().inspect_err(|_| shared.stop())?;
        let (submit, jobs) = mpsc::sync_channel(1);
        let worker_shared = Arc::clone(&shared);
        let selection = thread::Builder::new()
            .name("fr-local-file-selection".into())
            .spawn(move || {
                let _finished = Finished {
                    shared: Arc::clone(&worker_shared),
                };
                selection_loop(&worker_shared, &jobs);
            })
            .map_err(|_| {
                shared.stop();
                Error::Unavailable
            })?;
        originals.send(selection).map_err(|_| {
            shared.stop();
            Error::Unavailable
        })?;
        let io_shared = Arc::clone(&shared);
        let io_target = target.to_owned();
        let io = thread::Builder::new()
            .name("fr-local-file-control".into())
            .spawn(move || {
                let _finished = Finished {
                    shared: Arc::clone(&io_shared),
                };
                socket_loop(&io_shared, &listener, &submit, uid, &io_target, instance);
            })
            .map_err(|_| {
                shared.stop();
                Error::Unavailable
            })?;
        let io_thread = io.thread().clone();
        originals.send(io).map_err(|_| {
            shared.stop();
            Error::Unavailable
        })?;
        drop(originals);
        Ok(Self {
            shared,
            target: target.to_owned(),
            instance,
            io: io_thread,
            cleanup,
        })
    }
    pub(in super::super) fn info(&self) -> Result<Info, Error> {
        Ok(Info {
            target: self.target.clone(),
            instance: wire::hex(&self.instance),
            status: self.shared.handler.status(None)?,
        })
    }
    pub(in super::super) fn stop(&self) {
        self.shared.stop();
        self.io.unpark();
    }
    pub(in super::super) fn is_complete(&self) -> bool {
        self.cleanup.joined.load(Ordering::Acquire)
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Job {
    paths: Vec<PathBuf>,
    until: Instant,
    response: SyncSender<Result<Status, Error>>,
}
fn selection_loop(shared: &Shared, jobs: &Receiver<Job>) {
    while !shared.stopped.load(Ordering::Acquire) {
        let job = match jobs.recv_timeout(TICK) {
            Ok(job) => job,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        };
        let result = if shared.stopped.load(Ordering::Acquire) {
            Err(Error::Closed)
        } else if let Ok(result) = catch_unwind(AssertUnwindSafe(|| {
            check_time(job.until)?;
            shared
                .handler
                .select(&job.paths, job.until, &shared.stopped)
        })) {
            result
        } else {
            shared.stop();
            Err(Error::OutcomeUnknown)
        };
        let _ = job.response.try_send(result);
        shared.selecting.store(false, Ordering::Release);
    }
}

enum Phase {
    Identify,
    Identity,
    Header,
    Payload(wire::Command),
    End(wire::Command),
    Working(Receiver<Result<Status, Error>>),
    Reply,
}
struct Peer {
    stream: UnixStream,
    until: Instant,
    phase: Phase,
    bytes: Vec<u8>,
    used: usize,
}
impl Peer {
    fn new(stream: UnixStream) -> Result<Self, Error> {
        stream
            .set_nonblocking(true)
            .map_err(|_| Error::Unavailable)?;
        Ok(Self {
            stream,
            until: Instant::now()
                .checked_add(HANDSHAKE)
                .ok_or(Error::Unavailable)?,
            phase: Phase::Identify,
            bytes: vec![0; wire::IDENTIFY.len()],
            used: 0,
        })
    }
    fn receive(&mut self) -> Result<bool, Error> {
        if self.used == self.bytes.len() {
            return Ok(true);
        }
        match self.stream.read(&mut self.bytes[self.used..]) {
            Ok(0) => Err(Error::Protocol),
            Ok(count) => {
                self.used += count;
                Ok(self.used == self.bytes.len())
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                Ok(false)
            }
            Err(_) => Err(Error::Unavailable),
        }
    }
    fn transmit(&mut self) -> Result<bool, Error> {
        match self.stream.write(&self.bytes[self.used..]) {
            Ok(0) => Err(Error::Unavailable),
            Ok(count) => {
                self.used += count;
                Ok(self.used == self.bytes.len())
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                Ok(false)
            }
            Err(_) => Err(Error::Unavailable),
        }
    }
    fn reply(&mut self, instance: [u8; 16], result: Result<(Status, bool), Error>) {
        self.bytes = wire::result_record(instance, result);
        self.used = 0;
        self.phase = Phase::Reply;
    }
    fn dispatch(
        &mut self,
        command: wire::Command,
        shared: &Shared,
        submit: &SyncSender<Job>,
        instance: [u8; 16],
    ) -> Result<(), Error> {
        check_time(self.until)?;
        match command {
            wire::Command::Status(sequence) => {
                self.reply(
                    instance,
                    shared
                        .handler
                        .status(sequence)
                        .map(|status| (status, false)),
                );
            }
            wire::Command::Cancel(sequence) => {
                self.reply(instance, shared.handler.cancel(sequence));
            }
            wire::Command::Send(count) => {
                let paths = wire::decode_paths(&self.bytes, count)?;
                if shared
                    .selecting
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
                {
                    self.reply(instance, Err(Error::Busy));
                    return Ok(());
                }
                let (response, receive) = mpsc::sync_channel(1);
                if submit
                    .try_send(Job {
                        paths,
                        until: self.until,
                        response,
                    })
                    .is_err()
                {
                    shared.selecting.store(false, Ordering::Release);
                    self.reply(instance, Err(Error::Busy));
                } else {
                    self.bytes.clear();
                    self.phase = Phase::Working(receive);
                }
            }
        }
        Ok(())
    }
    fn advance(
        &mut self,
        shared: &Shared,
        submit: &SyncSender<Job>,
        target: &str,
        instance: [u8; 16],
    ) -> Result<bool, Error> {
        check_time(self.until)?;
        match &self.phase {
            Phase::Identify => {
                if !self.receive()? {
                    return Ok(false);
                }
                if self.bytes != wire::IDENTIFY {
                    return Err(Error::Protocol);
                }
                self.bytes = wire::identity_record(target, instance, shared.handler.status(None)?);
                self.used = 0;
                self.phase = Phase::Identity;
            }
            Phase::Identity => {
                if self.transmit()? {
                    self.bytes = vec![0; wire::COMMAND_BYTES];
                    self.used = 0;
                    self.phase = Phase::Header;
                }
            }
            Phase::Header => {
                if !self.receive()? {
                    return Ok(false);
                }
                let (command, expected, length) = wire::header(
                    self.bytes
                        .as_slice()
                        .try_into()
                        .map_err(|_| Error::Protocol)?,
                )?;
                if expected != instance {
                    self.reply(instance, Err(Error::WrongInstance));
                    return Ok(false);
                }
                self.bytes = vec![0; length];
                self.used = 0;
                self.phase = Phase::Payload(command);
            }
            Phase::Payload(command) => {
                let command = *command;
                if self.receive()? {
                    self.phase = Phase::End(command);
                }
            }
            Phase::End(command) => {
                let command = *command;
                let mut trailing = [0];
                match self.stream.read(&mut trailing) {
                    Ok(0) => {}
                    Ok(_) => {
                        self.reply(instance, Err(Error::Protocol));
                        return Ok(false);
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) =>
                    {
                        return Ok(false);
                    }
                    Err(_) => return Err(Error::Unavailable),
                }
                self.dispatch(command, shared, submit, instance)?;
            }
            Phase::Working(receive) => match receive.try_recv() {
                Ok(result) => self.reply(instance, result.map(|status| (status, false))),
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => self.reply(instance, Err(Error::OutcomeUnknown)),
            },
            Phase::Reply => return self.transmit(),
        }
        Ok(false)
    }
}

fn socket_loop(
    shared: &Shared,
    listener: &UnixListener,
    submit: &SyncSender<Job>,
    uid: u32,
    target: &str,
    instance: [u8; 16],
) {
    let mut peers = Vec::with_capacity(MAX_PEERS);
    while !shared.stopped.load(Ordering::Acquire) {
        for _ in 0..MAX_PEERS {
            match listener.accept() {
                Ok((stream, _)) => {
                    if peers.len() < MAX_PEERS
                        && check_peer(&stream, uid).is_ok()
                        && let Ok(peer) = Peer::new(stream)
                    {
                        peers.push(peer);
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) =>
                {
                    break;
                }
                Err(_) => return,
            }
        }
        let mut index = 0;
        while index < peers.len() {
            if matches!(
                peers[index].advance(shared, submit, target, instance),
                Ok(false)
            ) {
                index += 1;
            } else {
                peers.swap_remove(index);
            }
        }
        thread::park_timeout(TICK);
    }
}

//! The root side. One single-task loop accepts connections and drives at most
//! `MAX_CONNECTIONS` handlers; nothing is spawned. Each handler serves one
//! request at a time and owns at most one rule, removed when it ends.
use super::{
    super::{
        Leaf, SocketAddr, address_assigned, command, delete_table, install_script, interface_index,
        interface_valid, net_admin, new_table, observe, protected, protected_executable, read_back,
        table_exists, validate_rule,
    },
    Install, MAX_REQUEST, Refusal, Request, Response, TABLE_PREFIX, decode_request,
    encode_response, owned_table,
};
use crate::{Error as IdentityError, local::bounded, local::now};
use asupersync::{
    cx::Cx,
    io::{AsyncReadExt, AsyncWriteExt},
    net::unix::{UnixListener, UnixStream},
};
use serde::Deserialize;
use std::{
    fs::{self, File},
    future::{Future, poll_fn},
    net::IpAddr,
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    pin::{Pin, pin},
    sync::atomic::{AtomicU64, Ordering},
    task::Poll,
    time::Duration,
};

/// Concurrent broker connections (each holds at most one rule).
pub const MAX_CONNECTIONS: usize = 8;
const MAX_SETTINGS: usize = 4096;
const MAX_ALLOWED_UIDS: usize = 16;
/// Once a frame starts, all of it must arrive within this deadline.
const FRAME_DEADLINE: Duration = Duration::from_secs(1);
const WRITE_DEADLINE: Duration = Duration::from_secs(1);
/// Brokers renew every 500 ms; after closing their connections, wait longer
/// than that before reclaiming so they fence before protection is removed.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
/// Per connection: a burst of 4 requests, then one per 250 ms (renewal is 2/s).
const REQUEST_BURST: u32 = 4;
const REQUEST_REFILL_US: u64 = 250_000;
/// Admitted-uid connections: a burst of 8, then one per 250 ms.
const ACCEPT_BURST: u32 = 8;
const ACCEPT_REFILL_US: u64 = 250_000;
/// Refusal log lines: a burst of 16, then one per second; the rest are counted.
const LOG_BURST: u32 = 16;
const LOG_REFILL_US: u64 = 1_000_000;
/// Accepts handled per loop turn before yielding to connection handlers.
const ACCEPTS_PER_TURN: usize = 16;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsFile {
    interface: String,
    allowed_uids: Vec<u32>,
    #[serde(default)]
    socket: Option<PathBuf>,
    #[serde(default)]
    nft: Option<PathBuf>,
    #[serde(default)]
    ip: Option<PathBuf>,
}
/// Root administrator choices. Nothing here is supplied by a connecting broker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    interface: String,
    allowed: Vec<u32>,
    socket: PathBuf,
    nft: PathBuf,
    ip: PathBuf,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SettingsError {
    /// The file or an ancestor is not root-owned, or is group/world-writable.
    Unprotected,
    Unreadable,
    TooLarge,
    Malformed,
    InvalidInterface,
    InvalidAllowedUids,
    InvalidSocket,
    UntrustedExecutable,
}
impl Settings {
    /// Load the ROOT-OWNED configuration file; the tools must be root-owned too.
    pub fn load(path: &Path) -> Result<Self, SettingsError> {
        use std::io::Read as _;
        let path = protected(path, Leaf::File).ok_or(SettingsError::Unprotected)?;
        let mut bytes = Vec::new();
        File::open(path)
            .and_then(|file| {
                file.take(u64::try_from(MAX_SETTINGS + 1).unwrap_or(u64::MAX))
                    .read_to_end(&mut bytes)
            })
            .map_err(|_| SettingsError::Unreadable)?;
        if bytes.len() > MAX_SETTINGS {
            return Err(SettingsError::TooLarge);
        }
        let mut settings = Self::parse(&bytes)?;
        settings.nft =
            protected_executable(&settings.nft).map_err(|_| SettingsError::UntrustedExecutable)?;
        settings.ip =
            protected_executable(&settings.ip).map_err(|_| SettingsError::UntrustedExecutable)?;
        Ok(settings)
    }
    /// Strict parse: unknown or duplicate fields, bad names and empty or
    /// oversized uid lists refuse rather than fall back to a default.
    pub fn parse(bytes: &[u8]) -> Result<Self, SettingsError> {
        if bytes.len() > MAX_SETTINGS {
            return Err(SettingsError::TooLarge);
        }
        let file: SettingsFile =
            serde_json::from_slice(bytes).map_err(|_| SettingsError::Malformed)?;
        if !interface_valid(&file.interface) {
            return Err(SettingsError::InvalidInterface);
        }
        let mut sorted = file.allowed_uids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        if sorted.is_empty()
            || sorted.len() != file.allowed_uids.len()
            || sorted.len() > MAX_ALLOWED_UIDS
        {
            return Err(SettingsError::InvalidAllowedUids);
        }
        let socket = file
            .socket
            .unwrap_or_else(|| PathBuf::from(super::DEFAULT_SOCKET));
        if !super::socket_path_valid(&socket) || socket.parent().is_none() {
            return Err(SettingsError::InvalidSocket);
        }
        Ok(Self {
            interface: file.interface,
            allowed: sorted,
            socket,
            nft: file.nft.unwrap_or_else(|| PathBuf::from("/usr/sbin/nft")),
            ip: file.ip.unwrap_or_else(|| PathBuf::from("/usr/sbin/ip")),
        })
    }
    pub fn socket(&self) -> &Path {
        &self.socket
    }
    pub fn interface(&self) -> &str {
        &self.interface
    }
    pub fn allows(&self, uid: u32) -> bool {
        self.allowed.binary_search(&uid).is_ok()
    }
}

/// Operator log events: uids, generations and table names only; never
/// addresses, ports or command output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Reclaimed {
        tables: usize,
    },
    Listening,
    Refused {
        uid: u32,
        reason: Refusal,
    },
    /// Refusal lines dropped by the log budget since the last one reported.
    Suppressed {
        count: u64,
    },
    Installed {
        uid: u32,
        generation: u64,
        table: String,
    },
    /// First successful renewal of a generation (later ones are not logged).
    Renewed {
        generation: u64,
        table: String,
    },
    Removed {
        generation: u64,
        table: String,
        disconnect: bool,
    },
    CleanupFailed {
        table: String,
    },
    Stopping,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ServeError {
    /// No effective `CAP_NET_ADMIN`: this role cannot administer nftables.
    NotPrivileged,
    /// The running executable (or a parent directory) is not root-only.
    UntrustedExecutable,
    UnprotectedSocketDirectory,
    /// The socket path exists and is not a stale root-owned socket.
    SocketInUse,
    Bind,
    Reclaim,
    Accept,
    Cancelled,
}
impl ServeError {
    pub const fn code(self) -> &'static str {
        match self {
            Self::NotPrivileged => "ingress_helper_unprivileged",
            Self::UntrustedExecutable => "ingress_helper_untrusted_executable",
            Self::UnprotectedSocketDirectory | Self::SocketInUse | Self::Bind => {
                "ingress_helper_socket_unavailable"
            }
            Self::Reclaim => "ingress_helper_reclaim_failed",
            Self::Accept | Self::Cancelled => "ingress_helper_stopped",
        }
    }
}

struct Bucket {
    tokens: u32,
    burst: u32,
    refill_us: u64,
    last_us: u64,
}
impl Bucket {
    const fn new(burst: u32, refill_us: u64, now_us: u64) -> Self {
        Self {
            tokens: burst,
            burst,
            refill_us,
            last_us: now_us,
        }
    }
    fn take(&mut self, now_us: u64) -> bool {
        let earned = now_us.saturating_sub(self.last_us) / self.refill_us;
        if earned > 0 {
            self.tokens = u32::try_from(earned)
                .unwrap_or(u32::MAX)
                .saturating_add(self.tokens)
                .min(self.burst);
            self.last_us = if self.tokens == self.burst {
                now_us
            } else {
                self.last_us + earned * self.refill_us
            };
        }
        if self.tokens == 0 {
            return false;
        }
        self.tokens -= 1;
        true
    }
}

pub(super) struct Shared<'a> {
    cx: Cx,
    settings: &'a Settings,
    generation: AtomicU64,
    report: &'a (dyn Fn(Event) + Sync),
    log: std::sync::Mutex<(Bucket, u64)>,
}
impl<'a> Shared<'a> {
    pub(super) fn new(cx: &Cx, settings: &'a Settings, report: &'a (dyn Fn(Event) + Sync)) -> Self {
        Self {
            cx: cx.clone(),
            settings,
            generation: AtomicU64::new(1),
            report,
            log: std::sync::Mutex::new((
                Bucket::new(LOG_BURST, LOG_REFILL_US, now(cx).unwrap_or(0)),
                0,
            )),
        }
    }
}
impl Shared<'_> {
    fn now(&self) -> u64 {
        now(&self.cx).unwrap_or(0)
    }
    fn refused(&self, uid: u32, reason: Refusal) {
        let now_us = self.now();
        let Ok(mut log) = self.log.lock() else {
            return;
        };
        let (bucket, suppressed) = &mut *log;
        if bucket.take(now_us) {
            if *suppressed > 0 {
                (self.report)(Event::Suppressed { count: *suppressed });
                *suppressed = 0;
            }
            (self.report)(Event::Refused { uid, reason });
        } else {
            *suppressed = suppressed.saturating_add(1);
        }
    }
}

/// The kernel's own report for the CONFIGURED interface (never a caller name).
pub(super) struct Observed {
    pub(super) index: u32,
    pub(super) report: Vec<u8>,
}
/// Static admission of an install request against the root configuration.
pub(super) fn check_request(settings: &Settings, install: &Install) -> Result<(), Refusal> {
    if install.interface != settings.interface {
        return Err(Refusal::InterfaceNotConfigured);
    }
    if install.port == 0 {
        return Err(Refusal::InvalidPort);
    }
    let ip = install.address;
    if ip.is_unspecified()
        || ip.is_multicast()
        || ip.is_loopback()
        || matches!(ip, IpAddr::V6(v6) if v6.to_ipv4_mapped().is_some())
    {
        return Err(Refusal::InvalidAddress);
    }
    Ok(())
}
/// The requested address must currently be assigned (not tentative, DAD
/// passed) to the configured interface in the kernel's report.
pub(super) fn check_assigned(
    settings: &Settings,
    address: IpAddr,
    observed: &Observed,
) -> Result<u32, Refusal> {
    match address_assigned(
        &observed.report,
        observed.index,
        &settings.interface,
        address,
    ) {
        Ok(true) => Ok(observed.index),
        Ok(false) => Err(Refusal::AddressNotAssigned),
        Err(_) => Err(Refusal::InterfaceUnqualified),
    }
}

/// One rule owned by one connection.
pub(super) struct Owned {
    pub(super) generation: u64,
    pub(super) table: String,
    index: u32,
    address: SocketAddr,
    protocols: super::Protocols,
    renewed: bool,
}
/// Per-connection rule lifecycle. Generations fence renew/remove: only the
/// currently installed generation of THIS connection is accepted.
#[derive(Default)]
pub(super) struct Lifecycle {
    pub(super) active: Option<Owned>,
    /// A table whose install or removal was uncertain; retried before any
    /// new install and on disconnect, so residue cannot accumulate.
    residue: Option<String>,
}
impl Lifecycle {
    pub(super) fn current(&mut self, generation: u64) -> Result<&mut Owned, Refusal> {
        match &mut self.active {
            None => Err(Refusal::NotInstalled),
            Some(owned) if owned.generation != generation => Err(Refusal::StaleGeneration),
            Some(owned) => Ok(owned),
        }
    }
    pub(super) fn admit_install(&self) -> Result<(), Refusal> {
        if self.active.is_some() {
            Err(Refusal::AlreadyInstalled)
        } else {
            Ok(())
        }
    }
    #[cfg(test)]
    pub(super) fn installed(&mut self, generation: u64, table: &str) {
        self.active = Some(Owned {
            generation,
            table: table.to_owned(),
            index: 1,
            address: SocketAddr::from(([100, 64, 0, 1], 1)),
            protocols: super::Protocols::UDP,
            renewed: false,
        });
    }
    pub(super) fn removed(&mut self, generation: u64) -> Result<Owned, Refusal> {
        self.current(generation)?;
        self.active.take().ok_or(Refusal::NotInstalled)
    }

    async fn handle(&mut self, shared: &Shared<'_>, uid: u32, request: Request) -> Response {
        let result = match request {
            Request::Install(install) => self.install(shared, uid, &install).await,
            Request::Renew { generation } => self.renew(shared, generation).await,
            Request::Remove { generation } => self.remove(shared, generation).await,
        };
        result.unwrap_or_else(|reason| {
            shared.refused(uid, reason);
            Response::Refused(reason)
        })
    }
    async fn observe(shared: &Shared<'_>) -> Result<Observed, Refusal> {
        let settings = shared.settings;
        let (index, report) = observe(&shared.cx, &settings.ip, &settings.interface)
            .await
            .map_err(|_| Refusal::InterfaceUnqualified)?;
        Ok(Observed { index, report })
    }
    async fn install(
        &mut self,
        shared: &Shared<'_>,
        uid: u32,
        install: &Install,
    ) -> Result<Response, Refusal> {
        let (cx, settings) = (&shared.cx, shared.settings);
        self.admit_install()?;
        check_request(settings, install)?;
        let observed = Self::observe(shared).await?;
        let index = check_assigned(settings, install.address, &observed)?;
        self.clear_residue(shared).await?;
        let table = new_table(TABLE_PREFIX).map_err(|_| Refusal::FirewallFailed)?;
        let address = SocketAddr::new(install.address, install.port);
        let script = install_script(&table, address, install.protocols, index);
        if command(cx, &settings.nft, &["-f", "-"], script.as_bytes())
            .await
            .is_err()
        {
            self.forget(shared, table).await;
            return Err(Refusal::FirewallFailed);
        }
        let readback = read_back(cx, &settings.nft, &table)
            .await
            .ok()
            .filter(|bytes| {
                validate_rule(
                    bytes,
                    &table,
                    address,
                    install.protocols,
                    index,
                    &settings.interface,
                )
                .is_ok()
            });
        let Some(readback) = readback.filter(|_| interface_index(&settings.interface) == Ok(index))
        else {
            self.forget(shared, table).await;
            return Err(Refusal::FirewallMismatch);
        };
        let generation = shared.generation.fetch_add(1, Ordering::Relaxed);
        (shared.report)(Event::Installed {
            uid,
            generation,
            table: table.clone(),
        });
        self.active = Some(Owned {
            generation,
            table: table.clone(),
            index,
            address,
            protocols: install.protocols,
            renewed: false,
        });
        Ok(Response::Installed {
            generation,
            index,
            table,
            readback,
        })
    }
    async fn renew(&mut self, shared: &Shared<'_>, generation: u64) -> Result<Response, Refusal> {
        let (cx, settings) = (&shared.cx, shared.settings);
        let address = self.current(generation)?.address;
        let observed = Self::observe(shared).await?;
        let index = check_assigned(settings, address.ip(), &observed)?;
        let owned = self.current(generation)?;
        if index != owned.index {
            return Err(Refusal::InterfaceChanged);
        }
        let readback = read_back(cx, &settings.nft, &owned.table)
            .await
            .map_err(|_| Refusal::FirewallFailed)?;
        validate_rule(
            &readback,
            &owned.table,
            owned.address,
            owned.protocols,
            owned.index,
            &settings.interface,
        )
        .map_err(|_| Refusal::FirewallMismatch)?;
        if !owned.renewed {
            owned.renewed = true;
            (shared.report)(Event::Renewed {
                generation,
                table: owned.table.clone(),
            });
        }
        Ok(Response::Renewed {
            generation,
            readback,
        })
    }
    async fn remove(&mut self, shared: &Shared<'_>, generation: u64) -> Result<Response, Refusal> {
        let table = self.current(generation)?.table.clone();
        owned_table(&table)?;
        // On failure the rule stays owned: a retry or the disconnect removes it.
        delete_table(&shared.cx, &shared.settings.nft, &table)
            .await
            .map_err(|_| Refusal::FirewallFailed)?;
        self.removed(generation)?;
        (shared.report)(Event::Removed {
            generation,
            table,
            disconnect: false,
        });
        Ok(Response::Removed { generation })
    }
    async fn forget(&mut self, shared: &Shared<'_>, table: String) {
        if delete_table(&shared.cx, &shared.settings.nft, &table)
            .await
            .is_err()
        {
            (shared.report)(Event::CleanupFailed {
                table: table.clone(),
            });
            self.residue = Some(table);
        }
    }
    async fn clear_residue(&mut self, shared: &Shared<'_>) -> Result<(), Refusal> {
        if let Some(table) = &self.residue {
            delete_table(&shared.cx, &shared.settings.nft, table)
                .await
                .map_err(|_| Refusal::FirewallFailed)?;
            self.residue = None;
        }
        Ok(())
    }
    /// The connection ended (close, crash, violation): remove what it owned.
    async fn release(&mut self, shared: &Shared<'_>) {
        let active = self.active.take();
        let generation = active.as_ref().map_or(0, |owned| owned.generation);
        let tables = active.map(|owned| owned.table).into_iter();
        for table in tables.chain(self.residue.take()) {
            if owned_table(&table).is_err() {
                continue;
            }
            match delete_table(&shared.cx, &shared.settings.nft, &table).await {
                Ok(()) => (shared.report)(Event::Removed {
                    generation,
                    table,
                    disconnect: true,
                }),
                Err(_) => (shared.report)(Event::CleanupFailed { table }),
            }
        }
    }
}

enum Frame {
    Body(Vec<u8>),
    Violation(Refusal),
    Closed,
}
async fn next_frame(cx: &Cx, stream: &mut UnixStream) -> Frame {
    // Idle between requests is legitimate: the connection IS the lease.
    let mut first = [0_u8; 1];
    match stream.read(&mut first).await {
        Ok(1) => {}
        _ => return Frame::Closed,
    }
    let rest = Box::pin(bounded(cx, FRAME_DEADLINE, async {
        let mut second = [0_u8; 1];
        stream
            .read_exact(&mut second)
            .await
            .map_err(|_| IdentityError::LocalApiUnavailable)?;
        let length = usize::from(u16::from_be_bytes([first[0], second[0]]));
        if length == 0 {
            return Ok(Frame::Violation(Refusal::Malformed));
        }
        if length > MAX_REQUEST {
            return Ok(Frame::Violation(Refusal::Oversized));
        }
        let mut body = vec![0_u8; length];
        stream
            .read_exact(&mut body)
            .await
            .map_err(|_| IdentityError::LocalApiUnavailable)?;
        Ok(Frame::Body(body))
    }))
    .await;
    rest.unwrap_or(Frame::Closed)
}
async fn send(cx: &Cx, stream: &mut UnixStream, response: &Response) -> Result<(), IdentityError> {
    let frame = encode_response(response);
    Box::pin(bounded(cx, WRITE_DEADLINE, async {
        stream
            .write_all(&frame)
            .await
            .map_err(|_| IdentityError::LocalApiUnavailable)
    }))
    .await
}
/// One request at a time; a codec violation is answered and closes the
/// connection. Whatever ended the loop, the connection's rule is removed.
pub(super) async fn connection(shared: &Shared<'_>, mut stream: UnixStream, uid: u32) {
    let mut rule = Lifecycle::default();
    let mut budget = Bucket::new(REQUEST_BURST, REQUEST_REFILL_US, shared.now());
    loop {
        let body = match next_frame(&shared.cx, &mut stream).await {
            Frame::Body(body) => body,
            Frame::Violation(reason) => {
                shared.refused(uid, reason);
                let _ = send(&shared.cx, &mut stream, &Response::Refused(reason)).await;
                break;
            }
            Frame::Closed => break,
        };
        let response = match decode_request(&body) {
            Err(reason) => {
                shared.refused(uid, reason);
                let _ = send(&shared.cx, &mut stream, &Response::Refused(reason)).await;
                break;
            }
            Ok(_) if !budget.take(shared.now()) => {
                shared.refused(uid, Refusal::RateLimited);
                Response::Refused(Refusal::RateLimited)
            }
            Ok(request) => rule.handle(shared, uid, request).await,
        };
        if send(&shared.cx, &mut stream, &response).await.is_err() {
            break;
        }
    }
    rule.release(shared).await;
}
/// Best-effort nonblocking refusal before closing a connection never admitted.
fn refuse_now(stream: &UnixStream, reason: Refusal) {
    use std::io::Write as _;
    let mut socket = stream.as_std();
    let _ = socket.write(&encode_response(&Response::Refused(reason)));
}

/// Remove every helper-owned table (a dead helper's residue). Foreign tables,
/// including the direct path's `frd_` ones, are never touched.
async fn reclaim(cx: &Cx, nft: &Path) -> Result<usize, super::super::Error> {
    let bytes = command(cx, nft, &["-j", "list", "tables", "inet"], b"").await?;
    // Refuse a malformed inventory rather than reading absence into it.
    table_exists(&bytes, TABLE_PREFIX)?;
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| super::super::Error::FirewallMismatch)?;
    let owned: Vec<String> = value["nftables"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| row["table"]["name"].as_str())
        .filter(|name| owned_table(name).is_ok())
        .map(str::to_owned)
        .collect();
    if !owned.is_empty() {
        let script = owned.iter().fold(String::new(), |mut script, table| {
            script.push_str("delete table inet ");
            script.push_str(table);
            script.push('\n');
            script
        });
        command(cx, nft, &["-f", "-"], script.as_bytes()).await?;
    }
    Ok(owned.len())
}

/// Run the helper until `shutdown` completes. Refuses without `CAP_NET_ADMIN`,
/// from a non-root-only executable, or with an unprotected socket directory.
/// Stale helper tables are reclaimed before listening and again at shutdown,
/// after closing every connection and waiting past one broker renewal period.
pub async fn serve(
    cx: &Cx,
    settings: &Settings,
    shutdown: impl Future<Output = ()>,
    report: &(dyn Fn(Event) + Sync),
) -> Result<(), ServeError> {
    if !net_admin() {
        return Err(ServeError::NotPrivileged);
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| protected(&exe, Leaf::Executable))
        .ok_or(ServeError::UntrustedExecutable)?;
    settings
        .socket
        .parent()
        .and_then(|dir| protected(dir, Leaf::Directory))
        .ok_or(ServeError::UnprotectedSocketDirectory)?;
    let tables = reclaim(cx, &settings.nft)
        .await
        .map_err(|_| ServeError::Reclaim)?;
    report(Event::Reclaimed { tables });
    match fs::symlink_metadata(&settings.socket) {
        Ok(m) if m.file_type().is_socket() && m.uid() == 0 => {
            fs::remove_file(&settings.socket).map_err(|_| ServeError::SocketInUse)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        _ => return Err(ServeError::SocketInUse),
    }
    let listener = UnixListener::bind(&settings.socket)
        .await
        .map_err(|_| ServeError::Bind)?;
    // Any local account may connect; SO_PEERCRED, not the mode, admits it.
    fs::set_permissions(&settings.socket, fs::Permissions::from_mode(0o666))
        .map_err(|_| ServeError::Bind)?;
    report(Event::Listening);
    let shared = Shared::new(cx, settings, report);
    let result = accept(&shared, &listener, shutdown).await;
    report(Event::Stopping);
    drop(listener);
    asupersync::time::sleep(cx.now(), SHUTDOWN_GRACE).await;
    let tables = reclaim(cx, &settings.nft).await;
    if let Ok(tables) = tables {
        report(Event::Reclaimed { tables });
    }
    result.and(tables.map(|_| ()).map_err(|_| ServeError::Reclaim))
}

type Handler<'a> = Pin<Box<dyn Future<Output = ()> + 'a>>;
async fn accept<'a>(
    shared: &'a Shared<'a>,
    listener: &UnixListener,
    shutdown: impl Future<Output = ()>,
) -> Result<(), ServeError> {
    let mut shutdown = pin!(shutdown);
    let mut handlers: Vec<Handler<'a>> = Vec::with_capacity(MAX_CONNECTIONS);
    let mut admission = Bucket::new(ACCEPT_BURST, ACCEPT_REFILL_US, shared.now());
    // Dropping `handlers` on return closes every connection (brokers fence).
    poll_fn(|task| {
        if shutdown.as_mut().poll(task).is_ready() {
            return Poll::Ready(Ok(()));
        }
        if shared.cx.checkpoint().is_err() {
            return Poll::Ready(Err(ServeError::Cancelled));
        }
        for turn in 0.. {
            if turn == ACCEPTS_PER_TURN {
                task.waker().wake_by_ref();
                break;
            }
            let stream = match listener.poll_accept(task) {
                Poll::Ready(Ok((stream, _))) => stream,
                Poll::Ready(Err(_)) => return Poll::Ready(Err(ServeError::Accept)),
                Poll::Pending => break,
            };
            // SO_PEERCRED on EVERY connection before a single byte is read.
            let Ok(uid) = stream.peer_cred().map(|credentials| credentials.uid) else {
                continue;
            };
            let refusal = if !shared.settings.allows(uid) {
                Some(Refusal::PeerNotAllowed)
            } else if !admission.take(shared.now()) {
                Some(Refusal::RateLimited)
            } else if handlers.len() >= MAX_CONNECTIONS {
                Some(Refusal::CapacityExhausted)
            } else {
                None
            };
            if let Some(reason) = refusal {
                shared.refused(uid, reason);
                refuse_now(&stream, reason);
                continue;
            }
            handlers.push(Box::pin(connection(shared, stream, uid)));
        }
        handlers.retain_mut(|handler| handler.as_mut().poll(task).is_pending());
        Poll::Pending
    })
    .await
}

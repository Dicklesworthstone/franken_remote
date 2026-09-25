//! Linux kernel-TUN ingress for one exact destination (plan section 6.1).
//! Install a drop-only nftables transaction BEFORE binding. Address ownership is
//! independently read from the installed `LocalAPI`; a tailnet-looking prefix is
//! neither interface evidence nor peer authorization. No accept rule is added.
//!
//! The privileged host OS and selected local TUN are trusted. This profile does
//! not support userspace-networking Tailscale. Firewall/TUN/address identity is
//! periodically revalidated; per-I/O leases expire if that service stops. A
//! retired lease never becomes live again. Dropping leaves a restrictive rule;
//! explicit cleanup cannot remove it while any connection still holds a lease.
//!
//! A broker without `CAP_NET_ADMIN` asks the root [`helper`] instead; its rule
//! lives exactly as long as the helper connection, which every lease retains.
use super::{LocalApi, NodeIdentity, bounded};
use crate::Error as IdentityError;
use asupersync::{
    cx::Cx,
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Command, Stdio},
};
use std::{
    fmt,
    fs::{self, File},
    future::{Future, poll_fn},
    io::Read,
    net::{IpAddr, SocketAddr},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    pin::pin,
    sync::{Arc, Mutex, atomic::AtomicBool},
    task::Poll,
    time::Duration,
};
pub mod helper;
const COMMAND_LIMIT: usize = 16 * 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(1);
const REFRESH_INTERVAL: Duration = Duration::from_millis(500);

/// No command output, private names, addresses, or packet content in errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    Identity(IdentityError),
    InvalidConfiguration,
    UntrustedExecutable,
    UnqualifiedInterface,
    InterfaceChanged,
    CommandFailed,
    CommandOutputLimit,
    CommandUncertain,
    FirewallMismatch,
    ResidualRuleLimit,
    InUse,
    Closed,
    /// The root ingress helper is absent, unprotected, not root, or its
    /// connection (and therefore its rule) was lost.
    HelperUnavailable,
    /// The helper answered with this typed refusal; no rule is held for it.
    HelperRefused(helper::Refusal),
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "linux-ingress: {self:?}")
    }
}
impl std::error::Error for Error {}
impl From<IdentityError> for Error {
    fn from(error: IdentityError) -> Self {
        Self::Identity(error)
    }
}
impl Error {
    /// Stable operator refusal code; the Debug form carries the specific reason.
    pub const fn refusal_code(self) -> &'static str {
        match self {
            Self::HelperUnavailable => "ingress_helper_unavailable",
            _ => "ingress_unenforced",
        }
    }
}
/// Transport protocols one exact-destination drop rule covers: a nonempty
/// subset of {udp, tcp}. QUIC needs UDP; HTTPS/WSS ingress will need TCP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Protocols(u8);
impl Protocols {
    pub const UDP: Self = Self(1);
    pub const TCP: Self = Self(2);
    pub const UDP_TCP: Self = Self(3);
    const NAMES: [(u8, &'static str); 2] = [(1, "udp"), (2, "tcp")];
    pub const fn from_bits(bits: u8) -> Option<Self> {
        match bits {
            1..=3 => Some(Self(bits)),
            _ => None,
        }
    }
    pub const fn bits(self) -> u8 {
        self.0
    }
    fn names(self) -> impl Iterator<Item = &'static str> {
        Self::NAMES
            .into_iter()
            .filter(move |(bit, _)| self.0 & bit != 0)
            .map(|(_, name)| name)
    }
}
/// Who administers the rule. Neither choice is peer-selectable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Enforcement {
    /// This process runs the root-owned nft/ip tools itself (root or
    /// `CAP_NET_ADMIN`).
    Direct,
    /// The root `frd ingress-helper` at this protected socket owns the rule for
    /// exactly the lifetime of the connection this process keeps open.
    Helper(PathBuf),
}
impl Enforcement {
    /// Direct when this process holds effective `CAP_NET_ADMIN`; otherwise the
    /// least-privilege helper. Unknown capability state selects the helper.
    pub fn detect(helper: &Path) -> Self {
        if net_admin() {
            Self::Direct
        } else {
            Self::Helper(helper.to_path_buf())
        }
    }
}
/// Effective `CAP_NET_ADMIN` (bit 12) of this process, from the kernel's own
/// status file. Nothing is inferred from the user id.
pub fn net_admin() -> bool {
    fs::read_to_string("/proc/self/status").is_ok_and(|status| effective_net_admin(&status))
}
fn effective_net_admin(status: &str) -> bool {
    status
        .lines()
        .find_map(|line| line.strip_prefix("CapEff:"))
        .and_then(|value| u64::from_str_radix(value.trim(), 16).ok())
        .is_some_and(|caps| caps & (1 << 12) != 0)
}
/// Local administrator choices only. A peer cannot select an interface, port,
/// executable, or firewall script. Installed tools must be root-write-only.
#[derive(Clone)]
pub struct Configuration {
    nft: PathBuf,
    ip: PathBuf,
    interface: String,
    address: SocketAddr,
    protocols: Protocols,
    enforcement: Enforcement,
}
impl fmt::Debug for Configuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("IngressConfiguration([local])")
    }
}
impl Configuration {
    pub fn new(address: SocketAddr, interface: &str) -> Result<Self, Error> {
        if address.port() == 0
            || !interface_valid(interface)
            || !address_valid(address.ip())
            || matches!(address, SocketAddr::V6(v) if v.scope_id() != 0 || v.flowinfo() != 0)
        {
            return Err(Error::InvalidConfiguration);
        }
        Ok(Self {
            nft: PathBuf::from("/usr/sbin/nft"),
            ip: PathBuf::from("/usr/sbin/ip"),
            interface: interface.to_owned(),
            address,
            protocols: Protocols::UDP,
            enforcement: Enforcement::Direct,
        })
    }
    pub fn executables(mut self, nft: &Path, ip: &Path) -> Result<Self, Error> {
        self.nft = protected_executable(nft)?;
        self.ip = protected_executable(ip)?;
        Ok(self)
    }
    /// Protocols the rule must cover (default: UDP, the QUIC listener).
    #[must_use]
    pub const fn protocols(mut self, protocols: Protocols) -> Self {
        self.protocols = protocols;
        self
    }
    /// Select direct administration or the helper socket (default: direct).
    pub fn enforcement(mut self, enforcement: Enforcement) -> Result<Self, Error> {
        if let Enforcement::Helper(path) = &enforcement {
            helper::socket_path_valid(path)
                .then_some(())
                .ok_or(Error::InvalidConfiguration)?;
        }
        self.enforcement = enforcement;
        Ok(self)
    }
    pub const fn address(&self) -> SocketAddr {
        self.address
    }
    pub const fn enforcement_mode(&self) -> &Enforcement {
        &self.enforcement
    }
}
fn interface_valid(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 15
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
fn address_valid(ip: IpAddr) -> bool {
    !ip.is_unspecified()
        && !ip.is_multicast()
        && !ip.is_loopback()
        && !matches!(ip, IpAddr::V6(v) if v.to_ipv4_mapped().is_some())
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Leaf {
    Executable,
    File,
    Directory,
    /// A socket's own mode is not its access control (`SO_PEERCRED` is).
    Socket,
}
/// After resolving symlinks, every ancestor is a root-owned directory that only
/// root can write, and the leaf is root-owned and of the expected type.
fn protected(path: &Path, leaf: Leaf) -> Option<PathBuf> {
    use std::os::unix::fs::FileTypeExt;
    if !path.is_absolute() {
        return None;
    }
    let path = path.canonicalize().ok()?;
    for (index, part) in path.ancestors().enumerate() {
        let m = fs::symlink_metadata(part).ok()?;
        let kind = if index > 0 {
            m.is_dir()
        } else {
            match leaf {
                Leaf::Executable => m.is_file() && m.mode() & 0o111 != 0,
                Leaf::File => m.is_file(),
                Leaf::Directory => m.is_dir(),
                Leaf::Socket => m.file_type().is_socket(),
            }
        };
        let writable = m.mode() & 0o022 != 0 && !(index == 0 && leaf == Leaf::Socket);
        if m.uid() != 0 || !kind || writable {
            return None;
        }
    }
    Some(path)
}
fn protected_executable(path: &Path) -> Result<PathBuf, Error> {
    protected(path, Leaf::Executable).ok_or(Error::UntrustedExecutable)
}
fn sys_value(interface: &str, field: &str) -> Result<u32, Error> {
    let path = Path::new("/sys/class/net").join(interface).join(field);
    let mut b = [0_u8; 65];
    let n = File::open(path)
        .and_then(|mut f| f.read(&mut b))
        .map_err(|_| Error::UnqualifiedInterface)?;
    if n == b.len() {
        return Err(Error::UnqualifiedInterface);
    }
    let s = std::str::from_utf8(&b[..n])
        .map_err(|_| Error::UnqualifiedInterface)?
        .trim();
    s.strip_prefix("0x")
        .map_or_else(|| s.parse(), |s| u32::from_str_radix(s, 16))
        .map_err(|_| Error::UnqualifiedInterface)
}
fn interface_index(name: &str) -> Result<u32, Error> {
    let index = sys_value(name, "ifindex")?;
    let flags = sys_value(name, "flags")?;
    let tun = sys_value(name, "tun_flags")?;
    // IFF_UP and IFF_TUN, explicitly not IFF_TAP. An absent userspace-only
    // Tailscale interface refuses this Linux kernel-TUN profile.
    if index == 0 || flags & 1 == 0 || tun & 3 != 1 {
        return Err(Error::UnqualifiedInterface);
    }
    Ok(index)
}

async fn command(cx: &Cx, image: &Path, args: &[&str], input: &[u8]) -> Result<Vec<u8>, Error> {
    let image = protected_executable(image)?;
    let mut child = Command::new(image)
        .args(args)
        .env_clear()
        .env("LANG", "C")
        .current_dir("/")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| Error::CommandFailed)?;
    let mut stdin = child.stdin().ok_or(Error::CommandFailed)?;
    let stdout = child.stdout().ok_or(Error::CommandFailed)?;
    let operation = async {
        stdin
            .write_all(input)
            .await
            .map_err(|_| IdentityError::LocalApiUnavailable)?;
        drop(stdin);
        let mut bytes = Vec::new();
        let mut reader = stdout;
        let mut buf = [0_u8; 1024];
        loop {
            let n = reader
                .read(&mut buf)
                .await
                .map_err(|_| IdentityError::LocalApiUnavailable)?;
            if n == 0 {
                break;
            }
            if n > COMMAND_LIMIT.saturating_sub(bytes.len()) {
                return Err(IdentityError::MalformedMetadata);
            }
            bytes.extend_from_slice(&buf[..n]);
        }
        let status = child
            .wait_async(cx)
            .await
            .map_err(|_| IdentityError::LocalApiUnavailable)?;
        if !status.success() {
            return Err(IdentityError::LocalApiDenied);
        }
        Ok(bytes)
    };
    Box::pin(bounded(cx, COMMAND_TIMEOUT, operation))
        .await
        .map_err(|e| match e {
            IdentityError::MalformedMetadata => Error::CommandOutputLimit,
            IdentityError::LocalApiDenied => Error::CommandFailed,
            _ => Error::CommandUncertain,
        })
}
/// Kernel view of one interface: its qualified TUN index and the unprivileged
/// `ip -j address show` report. Neither value comes from a caller.
async fn observe(cx: &Cx, ip: &Path, interface: &str) -> Result<(u32, Vec<u8>), Error> {
    let index = interface_index(interface)?;
    let bytes = command(cx, ip, &["-j", "address", "show", "dev", interface], b"").await?;
    Ok((index, bytes))
}
/// `Ok(false)`: the report is exactly this interface but lacks a usable
/// (non-tentative, DAD-passed) `address`. `Err`: not that interface's report.
fn address_assigned(
    bytes: &[u8],
    index: u32,
    interface: &str,
    address: IpAddr,
) -> Result<bool, Error> {
    let v: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| Error::UnqualifiedInterface)?;
    let rows = v.as_array().ok_or(Error::UnqualifiedInterface)?;
    if rows.len() != 1
        || rows[0]["ifindex"].as_u64() != Some(u64::from(index))
        || rows[0]["ifname"].as_str() != Some(interface)
    {
        return Err(Error::UnqualifiedInterface);
    }
    let addresses = rows[0]["addr_info"]
        .as_array()
        .ok_or(Error::UnqualifiedInterface)?;
    if addresses.len() > 64 {
        return Err(Error::UnqualifiedInterface);
    }
    Ok(addresses.iter().any(|a| {
        a["local"].as_str().and_then(|v| v.parse::<IpAddr>().ok()) == Some(address)
            && a["tentative"].as_bool() != Some(true)
            && a["dadfailed"].as_bool() != Some(true)
    }))
}
async fn qualified_interface(cx: &Cx, config: &Configuration) -> Result<u32, Error> {
    let (index, bytes) = observe(cx, &config.ip, &config.interface).await?;
    if !address_assigned(&bytes, index, &config.interface, config.address.ip())?
        || interface_index(&config.interface)? != index
    {
        return Err(Error::UnqualifiedInterface);
    }
    Ok(index)
}
/// One exact drop rule per requested protocol, in one atomic transaction.
fn install_script(table: &str, addr: SocketAddr, protocols: Protocols, index: u32) -> String {
    use std::fmt::Write as _;
    let family = if addr.is_ipv4() { "ip" } else { "ip6" };
    let mut script = format!(
        "create table inet {table}\nadd chain inet {table} input {{ type filter hook input priority -310; policy accept; }}\n"
    );
    for protocol in protocols.names() {
        let _ = writeln!(
            script,
            "add rule inet {table} input {family} daddr {} {protocol} dport {} meta iif != {index} drop",
            addr.ip(),
            addr.port()
        );
    }
    script
}
/// nftables stores `meta iif` as an index but prints it back by the interface's
/// current name (1.1.6 does so even with `-n`), or as the number when it has no
/// name. Either must identify the interface already qualified by that index.
/// Exactly one rule per requested protocol and nothing else is accepted.
fn validate_rule(
    bytes: &[u8],
    table: &str,
    addr: SocketAddr,
    protocols: Protocols,
    index: u32,
    interface: &str,
) -> Result<(), Error> {
    use serde_json::json;
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| Error::FirewallMismatch)?;
    let rows = value["nftables"]
        .as_array()
        .ok_or(Error::FirewallMismatch)?;
    let (mut tables, mut chains, mut seen) = (0, 0, 0_u8);
    let family = if addr.is_ipv4() { "ip" } else { "ip6" };
    let expected = |protocol: &str, iif: serde_json::Value| {
        json!([
            {"match":{"op":"==","left":{"payload":{"protocol":family,"field":"daddr"}},"right":addr.ip().to_string()}},
            {"match":{"op":"==","left":{"payload":{"protocol":protocol,"field":"dport"}},"right":addr.port()}},
            {"match":{"op":"!=","left":{"meta":{"key":"iif"}},"right":iif}},
            {"drop":null}
        ])
    };
    let wanted: Vec<_> = Protocols::NAMES
        .into_iter()
        .filter(|(bit, _)| protocols.0 & bit != 0)
        .map(|(bit, name)| {
            (
                bit,
                expected(name, json!(index)),
                expected(name, json!(interface)),
            )
        })
        .collect();
    for row in rows {
        if let Some(t) = row.get("table") {
            tables += 1;
            if t["family"] != "inet"
                || t["name"] != table
                || t.get("flags")
                    .is_some_and(|v| v.as_array().is_none_or(|a| !a.is_empty()))
            {
                return Err(Error::FirewallMismatch);
            }
        } else if let Some(c) = row.get("chain") {
            chains += 1;
            if c["family"] != "inet"
                || c["table"] != table
                || c["name"] != "input"
                || c["type"] != "filter"
                || c["hook"] != "input"
                || c["prio"] != -310
                || c["policy"] != "accept"
            {
                return Err(Error::FirewallMismatch);
            }
        } else if let Some(r) = row.get("rule") {
            // Each requested protocol exactly once; duplicates and extras fail.
            let bit = wanted
                .iter()
                .find(|(_, by_index, by_name)| r["expr"] == *by_index || r["expr"] == *by_name)
                .map(|(bit, _, _)| *bit);
            if r["family"] != "inet"
                || r["table"] != table
                || r["chain"] != "input"
                || bit.is_none_or(|bit| seen & bit != 0)
            {
                return Err(Error::FirewallMismatch);
            }
            seen |= bit.unwrap_or(0);
        } else if row.get("metainfo").is_none() {
            return Err(Error::FirewallMismatch);
        }
    }
    if (tables, chains, seen) != (1, 1, protocols.0) {
        return Err(Error::FirewallMismatch);
    }
    Ok(())
}
async fn read_back(cx: &Cx, nft: &Path, table: &str) -> Result<Vec<u8>, Error> {
    command(cx, nft, &["-j", "-n", "list", "table", "inet", table], b"").await
}
async fn read_rule(cx: &Cx, config: &Configuration, table: &str, index: u32) -> Result<(), Error> {
    let bytes = read_back(cx, &config.nft, table).await?;
    validate_rule(
        &bytes,
        table,
        config.address,
        config.protocols,
        index,
        &config.interface,
    )
}
async fn residue_budget(cx: &Cx, config: &Configuration) -> Result<(), Error> {
    let bytes = command(cx, &config.nft, &["-j", "list", "tables", "inet"], b"").await?;
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| Error::FirewallMismatch)?;
    let rows = value["nftables"]
        .as_array()
        .ok_or(Error::FirewallMismatch)?;
    if rows
        .iter()
        .filter(|r| {
            r["table"]["name"]
                .as_str()
                .is_some_and(|n| n.starts_with("frd_"))
        })
        .count()
        >= 8
    {
        return Err(Error::ResidualRuleLimit);
    }
    Ok(())
}
fn new_table(prefix: &str) -> Result<String, Error> {
    let mut bytes = [0_u8; 16];
    File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|_| Error::CommandFailed)?;
    Ok(format!("{prefix}{:032x}", u128::from_ne_bytes(bytes)))
}

struct State {
    node: Arc<NodeIdentity>,
    active: bool,
    /// A duplicate of the helper connection: the helper keeps the rule until
    /// the LAST lease and the owner are gone, never while a transport lives.
    keepalive: Option<std::os::unix::net::UnixStream>,
}
enum Backend {
    Direct,
    Helper(helper::Link),
}
/// Cloneable liveness evidence for a rule already installed by Boundary. This
/// is not peer identity or permission. Retain it in the ORIGINAL transport's I/O
/// checks through every handoff, until that transport has actually been dropped.
#[derive(Clone)]
pub struct Lease {
    api: LocalApi,
    cx: Cx,
    address: SocketAddr,
    state: Arc<Mutex<State>>,
}
impl fmt::Debug for Lease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("IngressLease([local lifetime])")
    }
}
impl Lease {
    fn retire(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.active = false;
        }
    }
    pub fn check(&self, address: SocketAddr) -> Result<(), Error> {
        let mut state = self.state.lock().map_err(|_| Error::Closed)?;
        if !state.active {
            return Err(Error::Closed);
        }
        // Wrong destinations never borrow another listener's protection. They
        // also cannot revoke that otherwise healthy listener by probing it.
        if address != self.address {
            return Err(Error::InvalidConfiguration);
        }
        if let Err(error) = self.api.check_node(&self.cx, &state.node) {
            state.active = false;
            return Err(error.into());
        }
        Ok(())
    }
}
/// Owns one exact-address/port drop rule. This type deliberately owns no socket:
/// the native host binds only after install succeeds and holds a Lease across
/// its canonical TLS/session owner. Cleanup refuses while those leases exist.
/// No async work, hidden retry, or rule deletion occurs in Drop. (A helper-held
/// rule is released by the helper only once every lease has also been dropped.)
pub struct Boundary {
    lease: Lease,
    config: Configuration,
    index: u32,
    table: String,
    backend: Backend,
    cleaned: bool,
}
impl fmt::Debug for Boundary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IngressBoundary")
            .field("cleanup_complete", &self.cleaned)
            .finish_non_exhaustive()
    }
}
impl Boundary {
    /// Install and read back a single atomic drop-only transaction. The caller
    /// obtains the `NodeIdentity` from THIS `LocalApi` before calling. Uncertain
    /// installation may leave a restrictive frd_* table but never binds a socket.
    /// With [`Enforcement::Helper`] the root helper installs it; its read-back
    /// is validated here with the same `validate_rule` as the direct path.
    pub async fn install(
        cx: &Cx,
        api: LocalApi,
        node: NodeIdentity,
        config: Configuration,
    ) -> Result<Self, Error> {
        api.check_node(cx, &node)?;
        if !node.addresses().contains(&config.address.ip()) {
            return Err(Error::InvalidConfiguration);
        }
        // A listener has one bounded metadata operation independent of per-peer
        // lookups. Preserve the adapter's provenance and credential ownership.
        let api = LocalApi {
            busy: Arc::new(AtomicBool::new(false)),
            ..api
        };
        let index = qualified_interface(cx, &config).await?;
        api.check_node(cx, &node)?;
        let (table, backend, keepalive) = match &config.enforcement {
            Enforcement::Direct => {
                residue_budget(cx, &config).await?;
                let table = new_table("frd_")?;
                command(
                    cx,
                    &config.nft,
                    &["-f", "-"],
                    install_script(&table, config.address, config.protocols, index).as_bytes(),
                )
                .await?;
                (table, Backend::Direct, None)
            }
            Enforcement::Helper(socket) => {
                let (link, keepalive) = helper::Link::install(cx, socket, &config, index).await?;
                (
                    link.table().to_owned(),
                    Backend::Helper(link),
                    Some(keepalive),
                )
            }
        };
        let mut owner = Self {
            lease: Lease {
                api,
                cx: cx.clone(),
                address: config.address,
                state: Arc::new(Mutex::new(State {
                    node: Arc::new(node),
                    active: true,
                    keepalive,
                })),
            },
            config,
            index,
            table,
            backend,
            cleaned: false,
        };
        // Recheck both sides of the transaction before callers can obtain a
        // lease. No delayed command gets a replacement validity deadline.
        owner.revalidate().await?;
        Ok(owner)
    }
    pub const fn address(&self) -> SocketAddr {
        self.config.address
    }
    /// Operational residue identifier, not a credential. Only this owned table
    /// may be removed; there is no general firewall/command administration API.
    pub fn cleanup_table(&self) -> &str {
        &self.table
    }
    pub fn lease(&self) -> Result<Lease, Error> {
        self.lease.check(self.address())?;
        Ok(self.lease.clone())
    }
    pub fn close(&self) {
        if let Ok(mut state) = self.lease.state.lock() {
            state.active = false;
        }
    }
    pub fn is_closed(&self) -> bool {
        self.lease.check(self.address()).is_err()
    }
    /// Same node, daemon, address and numeric TUN index only. Failure or dropped
    /// work retires the original lifetime. Old validity must still hold after
    /// every await; a late result cannot resurrect protection.
    pub fn revalidate(&mut self) -> impl Future<Output = Result<(), Error>> + '_ {
        let attempt = Attempt {
            owner: self,
            complete: false,
        };
        async move {
            let mut attempt = attempt;
            let owner = &mut *attempt.owner;
            owner.lease.check(owner.address())?;
            let old = owner
                .lease
                .state
                .lock()
                .map_err(|_| Error::Closed)?
                .node
                .clone();
            let node = owner
                .lease
                .api
                .revalidate_node(&owner.lease.cx, &old)
                .await?;
            let index = qualified_interface(&owner.lease.cx, &owner.config).await?;
            if index != owner.index {
                return Err(Error::InterfaceChanged);
            }
            match &mut owner.backend {
                Backend::Direct => {
                    read_rule(&owner.lease.cx, &owner.config, &owner.table, index).await?;
                }
                // The helper re-qualifies the interface/address and reads the
                // kernel ruleset as root; its read-back must pass the same check.
                Backend::Helper(link) => {
                    let bytes = link.renew(&owner.lease.cx).await?;
                    validate_rule(
                        &bytes,
                        &owner.table,
                        owner.config.address,
                        owner.config.protocols,
                        index,
                        &owner.config.interface,
                    )?;
                }
            }
            owner.lease.check(owner.address())?;
            owner.lease.api.check_node(&owner.lease.cx, &node)?;
            let mut state = owner.lease.state.lock().map_err(|_| Error::Closed)?;
            if !state.active {
                return Err(Error::Closed);
            }
            state.node = Arc::new(node);
            drop(state);
            attempt.complete = true;
            Ok(())
        }
    }
    /// Drive a connection AND protection renewal without spawning a task. The
    /// application future is pinned once and is never restarted after a refresh.
    /// The retained lease is checked before/after every poll, including while a
    /// refresh is awaiting a tool. Returning/dropping this future retires it.
    pub fn supervise<'a, F: Future + 'a>(
        &'a mut self,
        operation: F,
    ) -> impl Future<Output = Result<F::Output, Error>> + 'a {
        let guard = Attempt {
            owner: self,
            complete: false,
        };
        async move {
            let mut operation = pin!(operation);
            let guard = guard;
            loop {
                let lease = guard.owner.lease()?;
                let cx = lease.cx.clone();
                let mut refresh = pin!(async {
                    asupersync::time::sleep(cx.now(), REFRESH_INTERVAL).await;
                    guard.owner.revalidate().await
                });
                let output = poll_fn(|task| {
                    let mut panic_fence = PanicFence {
                        lease: &lease,
                        complete: false,
                    };
                    let result = (|| {
                        lease.check(lease.address)?;
                        // Revocation/expiry wins over another application poll.
                        let renewed = refresh.as_mut().poll(task);
                        if let Poll::Ready(Err(error)) = renewed {
                            return Poll::Ready(Err(error));
                        }
                        lease.check(lease.address)?;
                        let output = operation.as_mut().poll(task);
                        lease.check(lease.address)?;
                        match output {
                            Poll::Ready(value) => Poll::Ready(Ok(Some(value))),
                            Poll::Pending if renewed.is_ready() => Poll::Ready(Ok(None)),
                            Poll::Pending => Poll::Pending,
                        }
                    })();
                    panic_fence.complete = true;
                    result
                })
                .await?;
                if let Some(output) = output {
                    return Ok(output);
                }
            }
        }
    }
    /// Fence at CALL time, then remove only this table after every transport
    /// lease is dropped. InUse/timeout retains a retryable cleanup owner. Even a
    /// successful application result is not proof that it released its sockets.
    pub fn stop<'a>(&'a mut self, cleanup: &'a Cx) -> impl Future<Output = Result<(), Error>> + 'a {
        self.close();
        async move {
            if self.cleaned {
                return Ok(());
            }
            if Arc::strong_count(&self.lease.state) != 1 {
                return Err(Error::InUse);
            }
            match &mut self.backend {
                Backend::Direct => delete_table(cleanup, &self.config.nft, &self.table).await?,
                // Acknowledged removal by generation; a lost connection is not
                // reported as verified cleanup (the helper reclaims on its own).
                Backend::Helper(link) => {
                    link.remove(cleanup).await?;
                    if let Ok(mut state) = self.lease.state.lock() {
                        state.keepalive = None;
                    }
                }
            }
            self.cleaned = true;
            Ok(())
        }
    }
}
/// Remove one owned table if present. Existence is read first so a prior
/// uncertain deletion is retryable; absence needs a well-formed inventory.
async fn delete_table(cx: &Cx, nft: &Path, table: &str) -> Result<(), Error> {
    let bytes = command(cx, nft, &["-j", "list", "tables", "inet"], b"").await?;
    if table_exists(&bytes, table)? {
        command(
            cx,
            nft,
            &["-f", "-"],
            format!("delete table inet {table}\n").as_bytes(),
        )
        .await?;
    }
    Ok(())
}
impl Drop for Boundary {
    fn drop(&mut self) {
        self.close();
    }
}
struct PanicFence<'a> {
    lease: &'a Lease,
    complete: bool,
}
impl Drop for PanicFence<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.lease.retire();
        }
    }
}
struct Attempt<'a> {
    owner: &'a mut Boundary,
    complete: bool,
}
impl Drop for Attempt<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.owner.close();
        }
    }
}
fn table_exists(bytes: &[u8], table: &str) -> Result<bool, Error> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| Error::FirewallMismatch)?;
    let rows = value["nftables"]
        .as_array()
        .ok_or(Error::FirewallMismatch)?;
    for row in rows {
        if let Some(t) = row.get("table") {
            if t["family"] != "inet" || t["name"].as_str().is_none() {
                return Err(Error::FirewallMismatch);
            }
        } else if row.get("metainfo").is_none() {
            return Err(Error::FirewallMismatch);
        }
    }
    Ok(rows
        .iter()
        .any(|row| row["table"]["name"].as_str() == Some(table)))
}
#[cfg(test)]
mod tests;

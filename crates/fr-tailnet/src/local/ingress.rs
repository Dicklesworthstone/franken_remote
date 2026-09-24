//! Linux kernel-TUN ingress for one exact UDP destination (plan section 6.1).
//! Install a drop-only nftables transaction BEFORE binding. Address ownership is
//! independently read from the installed `LocalAPI`; a tailnet-looking prefix is
//! neither interface evidence nor peer authorization. No accept rule is added.
//!
//! The privileged host OS and selected local TUN are trusted. This profile does
//! not support userspace-networking Tailscale. Firewall/TUN/address identity is
//! periodically revalidated; per-I/O leases expire if that service stops. A
//! retired lease never becomes live again. Dropping leaves a restrictive rule;
//! explicit cleanup cannot remove it while any connection still holds a lease.
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
/// Local administrator choices only. A peer cannot select an interface, port,
/// executable, or firewall script. Installed tools must be root-write-only.
#[derive(Clone)]
pub struct Configuration {
    nft: PathBuf,
    ip: PathBuf,
    interface: String,
    address: SocketAddr,
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
        })
    }
    pub fn executables(mut self, nft: &Path, ip: &Path) -> Result<Self, Error> {
        self.nft = protected_executable(nft)?;
        self.ip = protected_executable(ip)?;
        Ok(self)
    }
    pub const fn address(&self) -> SocketAddr {
        self.address
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
fn protected_executable(path: &Path) -> Result<PathBuf, Error> {
    if !path.is_absolute() {
        return Err(Error::UntrustedExecutable);
    }
    let path = path
        .canonicalize()
        .map_err(|_| Error::UntrustedExecutable)?;
    for (index, part) in path.ancestors().enumerate() {
        let m = fs::symlink_metadata(part).map_err(|_| Error::UntrustedExecutable)?;
        if m.uid() != 0
            || m.mode() & 0o022 != 0
            || if index == 0 {
                !m.is_file() || m.mode() & 0o111 == 0
            } else {
                !m.is_dir()
            }
        {
            return Err(Error::UntrustedExecutable);
        }
    }
    Ok(path)
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
async fn qualified_interface(cx: &Cx, config: &Configuration) -> Result<u32, Error> {
    let index = interface_index(&config.interface)?;
    let bytes = command(
        cx,
        &config.ip,
        &["-j", "address", "show", "dev", &config.interface],
        b"",
    )
    .await?;
    let v: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| Error::UnqualifiedInterface)?;
    let rows = v.as_array().ok_or(Error::UnqualifiedInterface)?;
    if rows.len() != 1
        || rows[0]["ifindex"].as_u64() != Some(u64::from(index))
        || rows[0]["ifname"].as_str() != Some(&config.interface)
    {
        return Err(Error::UnqualifiedInterface);
    }
    let addresses = rows[0]["addr_info"]
        .as_array()
        .ok_or(Error::UnqualifiedInterface)?;
    if addresses.len() > 64
        || !addresses.iter().any(|a| {
            a["local"].as_str().and_then(|v| v.parse::<IpAddr>().ok()) == Some(config.address.ip())
                && a["tentative"].as_bool() != Some(true)
                && a["dadfailed"].as_bool() != Some(true)
        })
        || interface_index(&config.interface)? != index
    {
        return Err(Error::UnqualifiedInterface);
    }
    Ok(index)
}
fn install_script(table: &str, addr: SocketAddr, index: u32) -> String {
    let family = if addr.is_ipv4() { "ip" } else { "ip6" };
    format!(
        "create table inet {table}\nadd chain inet {table} input {{ type filter hook input priority -310; policy accept; }}\nadd rule inet {table} input {family} daddr {} udp dport {} meta iif != {index} drop\n",
        addr.ip(),
        addr.port()
    )
}
/// nftables stores `meta iif` as an index but prints it back by the interface's
/// current name (1.1.6 does so even with `-n`), or as the number when it has no
/// name. Either must identify the interface already qualified by that index.
fn validate_rule(
    bytes: &[u8],
    table: &str,
    addr: SocketAddr,
    index: u32,
    interface: &str,
) -> Result<(), Error> {
    use serde_json::json;
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| Error::FirewallMismatch)?;
    let rows = value["nftables"]
        .as_array()
        .ok_or(Error::FirewallMismatch)?;
    let (mut tables, mut chains, mut rules) = (0, 0, 0);
    let family = if addr.is_ipv4() { "ip" } else { "ip6" };
    let expected = |iif: serde_json::Value| {
        json!([
            {"match":{"op":"==","left":{"payload":{"protocol":family,"field":"daddr"}},"right":addr.ip().to_string()}},
            {"match":{"op":"==","left":{"payload":{"protocol":"udp","field":"dport"}},"right":addr.port()}},
            {"match":{"op":"!=","left":{"meta":{"key":"iif"}},"right":iif}},
            {"drop":null}
        ])
    };
    let (by_index, by_name) = (expected(json!(index)), expected(json!(interface)));
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
            rules += 1;
            if r["family"] != "inet"
                || r["table"] != table
                || r["chain"] != "input"
                || (r["expr"] != by_index && r["expr"] != by_name)
            {
                return Err(Error::FirewallMismatch);
            }
        } else if row.get("metainfo").is_none() {
            return Err(Error::FirewallMismatch);
        }
    }
    if (tables, chains, rules) != (1, 1, 1) {
        return Err(Error::FirewallMismatch);
    }
    Ok(())
}
async fn read_rule(cx: &Cx, config: &Configuration, table: &str, index: u32) -> Result<(), Error> {
    let bytes = command(
        cx,
        &config.nft,
        &["-j", "-n", "list", "table", "inet", table],
        b"",
    )
    .await?;
    validate_rule(&bytes, table, config.address, index, &config.interface)
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
fn new_table() -> Result<String, Error> {
    let mut bytes = [0_u8; 16];
    File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|_| Error::CommandFailed)?;
    Ok(format!("frd_{:032x}", u128::from_ne_bytes(bytes)))
}

struct State {
    node: Arc<NodeIdentity>,
    active: bool,
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
/// No async work, hidden retry, or rule deletion occurs in Drop.
pub struct Boundary {
    lease: Lease,
    config: Configuration,
    index: u32,
    table: String,
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
        residue_budget(cx, &config).await?;
        let table = new_table()?;
        command(
            cx,
            &config.nft,
            &["-f", "-"],
            install_script(&table, config.address, index).as_bytes(),
        )
        .await?;
        let mut owner = Self {
            lease: Lease {
                api,
                cx: cx.clone(),
                address: config.address,
                state: Arc::new(Mutex::new(State {
                    node: Arc::new(node),
                    active: true,
                })),
            },
            config,
            index,
            table,
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
            read_rule(&owner.lease.cx, &owner.config, &owner.table, index).await?;
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
            // Read existence first so a prior uncertain deletion is retryable.
            let bytes = command(
                cleanup,
                &self.config.nft,
                &["-j", "list", "tables", "inet"],
                b"",
            )
            .await?;
            let exists = table_exists(&bytes, &self.table)?;
            if exists {
                command(
                    cleanup,
                    &self.config.nft,
                    &["-f", "-"],
                    format!("delete table inet {}\n", self.table).as_bytes(),
                )
                .await?;
            }
            self.cleaned = true;
            Ok(())
        }
    }
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

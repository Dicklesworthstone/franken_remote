//! Explicit privileged qualification of the ingress helper split, never a
//! shipping listener or a live tailnet. Run ONLY via
//! `scripts/test_ingress_helper_namespace.sh`, as root in a fresh network and
//! mount namespace: `qualify_ingress_helper /absolute/path/to/frd`.
//!
//! Real: nftables in the kernel, a TUN named `tailscale0` and a veth pair to a
//! second namespace, packet delivery and drops, the `frd ingress-helper` role,
//! `SO_PEERCRED`, process death, and the broker's `Boundary` with
//! `Enforcement::Helper` running as an unprivileged uid. Synthetic: the
//! `LocalAPI` node metadata (served by root in this namespace). A namespace is
//! not a live tailnet: real `tailscaled`, its interface and a real second NIC
//! remain separately untested here.
#[cfg(target_os = "linux")]
mod linux {
    use asupersync::{cx::Cx, runtime::RuntimeBuilder, time::sleep};
    use fr_tailnet::{
        LocalApi,
        ingress::{
            Boundary, Configuration, Enforcement, Protocols,
            helper::{self, Install, Refusal, Request, Response},
        },
    };
    use serde_json::json;
    use std::{
        fs,
        io::{BufRead, BufReader, Read, Write},
        net::{IpAddr, SocketAddr, TcpListener, UdpSocket},
        os::unix::{
            fs::PermissionsExt,
            net::{UnixListener, UnixStream},
        },
        path::{Path, PathBuf},
        process::{Child, Command, ExitStatus, Stdio},
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        thread,
        time::{Duration, Instant},
    };

    const DIR: &str = "/run/fr-helper-qual";
    const TAILNET: &str = "tailscale0";
    const ADDRESS: &str = "100.64.0.1";
    const PORT: u16 = 4710;
    const UNRELATED: u16 = 4712;
    /// Admitted by the root configuration; `DENIED` is not.
    const ALLOWED: u32 = 65534;
    const DENIED: u32 = 65533;

    fn path(name: &str) -> PathBuf {
        Path::new(DIR).join(name)
    }
    fn run(program: &str, args: &[&str]) {
        let status = Command::new(program).args(args).status().unwrap();
        assert!(status.success(), "{program} {args:?}: {status}");
    }
    fn nft(script: &str) {
        let mut child = Command::new("/usr/sbin/nft")
            .args(["-f", "-"])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(script.as_bytes())
            .unwrap();
        assert!(child.wait().unwrap().success(), "{script}");
    }
    fn tables() -> Vec<String> {
        let out = Command::new("/usr/sbin/nft")
            .args(["-j", "list", "tables", "inet"])
            .output()
            .unwrap();
        assert!(out.status.success());
        let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        value["nftables"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|row| row["table"]["name"].as_str().map(str::to_owned))
            .collect()
    }
    fn helper_tables() -> Vec<String> {
        tables()
            .into_iter()
            .filter(|name| name.starts_with(helper::TABLE_PREFIX))
            .collect()
    }
    fn pass(what: &str) {
        println!("PASS: {what}");
    }
    fn until(what: &str, limit: Duration, ready: impl Fn() -> bool) -> Duration {
        let start = Instant::now();
        while !ready() {
            assert!(start.elapsed() < limit, "timed out waiting for: {what}");
            thread::sleep(Duration::from_millis(20));
        }
        start.elapsed()
    }

    fn tailnet_interface() {
        run(
            "/usr/sbin/ip",
            &["tuntap", "add", "dev", TAILNET, "mode", "tun"],
        );
        run(
            "/usr/sbin/ip",
            &["address", "add", "100.64.0.1/32", "dev", TAILNET],
        );
        run("/usr/sbin/ip", &["link", "set", TAILNET, "up"]);
        run(
            "/usr/sbin/ip",
            &["route", "add", "100.100.0.0/16", "dev", TAILNET],
        );
    }
    fn network() {
        run("/usr/sbin/ip", &["link", "set", "lo", "up"]);
        tailnet_interface();
        run("/usr/sbin/ip", &["netns", "add", "frlan"]);
        run(
            "/usr/sbin/ip",
            &[
                "link", "add", "frveth0", "type", "veth", "peer", "name", "frveth1", "netns",
                "frlan",
            ],
        );
        run(
            "/usr/sbin/ip",
            &["address", "add", "192.168.77.1/24", "dev", "frveth0"],
        );
        run("/usr/sbin/ip", &["link", "set", "frveth0", "up"]);
        for args in [
            &["link", "set", "lo", "up"][..],
            &["address", "add", "192.168.77.2/24", "dev", "frveth1"],
            &["link", "set", "frveth1", "up"],
            &["route", "add", "default", "via", "192.168.77.1"],
        ] {
            let mut all = vec!["-n", "frlan"];
            all.extend(args);
            run("/usr/sbin/ip", &all);
        }
        println!(
            "[net] {TAILNET}: TUN 100.64.0.1/32, route 100.100.0.0/16 (tailnet peers)\n\
             [net] frveth0 192.168.77.1/24 <-> netns frlan: frveth1 192.168.77.2/24 (non-tailnet interface)"
        );
    }

    /// Send from the second namespace, arriving through frveth0 (not the TUN).
    fn lan(mode: &str, port: u16) -> String {
        let code = r"
import socket, sys
mode, port = sys.argv[1], int(sys.argv[2])
if mode == 'udp':
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.sendto(b'via-frveth0', ('100.64.0.1', port))
    print('sent')
else:
    try:
        socket.create_connection(('100.64.0.1', port), timeout=1.0).close()
        print('connected')
    except socket.timeout:
        print('timeout')
    except OSError as e:
        print(type(e).__name__)
";
        let out = Command::new("/usr/sbin/ip")
            .args([
                "netns",
                "exec",
                "frlan",
                "/usr/bin/python3",
                "-c",
                code,
                mode,
            ])
            .arg(port.to_string())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }
    /// Write a real IP packet into the TUN, as tailscaled would for a peer.
    fn tun(mode: &str, port: u16) -> String {
        let code = r"
import fcntl, os, select, socket, struct, sys, time
mode, port = sys.argv[1], int(sys.argv[2])
fd = os.open('/dev/net/tun', os.O_RDWR)
fcntl.ioctl(fd, 0x400454ca, struct.pack('16sH', b'tailscale0', 0x1001))
def csum(b):
    if len(b) % 2: b += b'\0'
    n = sum(struct.unpack('!%dH' % (len(b) // 2), b)); n = (n >> 16) + (n & 0xffff); n += n >> 16
    return (~n) & 0xffff
src, dst = socket.inet_aton('100.100.1.2'), socket.inet_aton('100.64.0.1')
def ip(proto, seg):
    h = struct.pack('!BBHHHBBH4s4s', 0x45, 0, 20 + len(seg), 1, 0, 64, proto, 0, src, dst)
    return h[:10] + struct.pack('!H', csum(h)) + h[12:] + seg
def l4(proto, seg):
    return csum(src + dst + struct.pack('!BBH', 0, proto, len(seg)) + seg)
if mode == 'udp':
    data = b'via-tailscale0'
    seg = struct.pack('!HHHH', 30001, port, 8 + len(data), 0) + data
    seg = seg[:6] + struct.pack('!H', l4(17, seg) or 0xffff) + seg[8:]
    os.write(fd, ip(17, seg))
    print('sent')
else:
    seg = struct.pack('!HHIIBBHHH', 30002, port, 12345, 0, 5 << 4, 0x02, 64240, 0, 0)
    seg = seg[:16] + struct.pack('!H', l4(6, seg)) + seg[18:]
    os.write(fd, ip(6, seg))
    end, result = time.time() + 2, 'none'
    while time.time() < end:
        ready, _, _ = select.select([fd], [], [], max(0, end - time.time()))
        if not ready: break
        pkt = os.read(fd, 2048)
        if len(pkt) < 40 or pkt[0] >> 4 != 4 or pkt[9] != 6: continue
        ihl = (pkt[0] & 15) * 4
        sport, dport = struct.unpack('!HH', pkt[ihl:ihl + 4])
        if pkt[12:16] == dst and sport == port and dport == 30002:
            flags = pkt[ihl + 13]
            result = 'synack' if flags & 0x12 == 0x12 else 'flags=%#x' % flags
            break
    print(result)
os.close(fd)
";
        let out = Command::new("/usr/bin/python3")
            .args(["-c", code, mode])
            .arg(port.to_string())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }
    struct Receivers {
        udp: UdpSocket,
        unrelated: UdpSocket,
        tcp: TcpListener,
    }
    impl Receivers {
        fn bind() -> Self {
            let tcp = TcpListener::bind((ADDRESS, PORT)).unwrap();
            tcp.set_nonblocking(true).unwrap();
            Self {
                udp: UdpSocket::bind((ADDRESS, PORT)).unwrap(),
                unrelated: UdpSocket::bind((ADDRESS, UNRELATED)).unwrap(),
                tcp,
            }
        }
        fn datagram(socket: &UdpSocket, wait: Duration) -> Option<(Vec<u8>, SocketAddr)> {
            socket.set_read_timeout(Some(wait)).unwrap();
            let mut buffer = [0_u8; 256];
            socket
                .recv_from(&mut buffer)
                .ok()
                .map(|(n, from)| (buffer[..n].to_vec(), from))
        }
        fn drain_tcp(&self) {
            while self.tcp.accept().is_ok() {}
        }
    }
    fn udp_dropped_via_lan(rx: &Receivers) {
        assert_eq!(lan("udp", PORT), "sent");
        let got = Receivers::datagram(&rx.udp, Duration::from_millis(600));
        assert!(
            got.is_none(),
            "UDP via frveth0 must be DROPPED, got {got:?}"
        );
        pass("UDP to 100.64.0.1:4710 via frveth0 (non-tailnet) DROPPED");
    }
    fn udp_delivered_via_lan(rx: &Receivers, why: &str) {
        assert_eq!(lan("udp", PORT), "sent");
        let got = Receivers::datagram(&rx.udp, Duration::from_secs(2));
        assert_eq!(
            got.as_ref().map(|(data, _)| data.as_slice()),
            Some(&b"via-frveth0"[..]),
            "{why}"
        );
        pass(&format!(
            "UDP to 100.64.0.1:4710 via frveth0 delivered from {} ({why})",
            got.unwrap().1
        ));
    }

    struct Log(Arc<Mutex<Vec<String>>>);
    impl Log {
        fn capture(tag: &'static str, child: &mut Child) -> Self {
            let lines = Arc::new(Mutex::new(Vec::new()));
            let streams: Vec<Box<dyn Read + Send>> = vec![
                Box::new(child.stdout.take().unwrap()),
                Box::new(child.stderr.take().unwrap()),
            ];
            for stream in streams {
                let sink = lines.clone();
                thread::spawn(move || {
                    for line in BufReader::new(stream).lines().map_while(Result::ok) {
                        println!("  [{tag}] {line}");
                        sink.lock().unwrap().push(line);
                    }
                });
            }
            Self(lines)
        }
        fn find(&self, needle: &str) -> Option<String> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .find(|line| line.contains(needle))
                .cloned()
        }
        fn wait(&self, needle: &str, limit: Duration) -> String {
            until(needle, limit, || self.find(needle).is_some());
            self.find(needle).unwrap()
        }
    }
    fn start_helper() -> (Child, Log) {
        let mut child = Command::new(path("frd"))
            .args(["ingress-helper", "--config"])
            .arg(path("helper.json"))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let log = Log::capture("helper", &mut child);
        log.wait("frd ingress-helper: listening", Duration::from_secs(10));
        (child, log)
    }
    fn as_uid(uid: u32, args: &[&str]) -> (Child, Log) {
        let id = uid.to_string();
        let mut child = Command::new("/usr/bin/setpriv")
            .args([
                format!("--reuid={id}").as_str(),
                format!("--regid={id}").as_str(),
                "--clear-groups",
                "--inh-caps=-all",
                "--bounding-set=-all",
                "--",
            ])
            .arg(path("qualify"))
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let log = Log::capture(
            if uid == ALLOWED {
                "broker uid 65534"
            } else {
                "uid 65533"
            },
            &mut child,
        );
        (child, log)
    }
    fn exit(child: &mut Child, limit: Duration) -> ExitStatus {
        let start = Instant::now();
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                return status;
            }
            assert!(start.elapsed() < limit, "process did not exit");
            thread::sleep(Duration::from_millis(20));
        }
    }
    fn installed(log: &Log) -> String {
        let line = log.wait("installed ", Duration::from_secs(10));
        let table = line.trim_start_matches("installed ").to_owned();
        assert_eq!(helper::owned_table(&table), Ok(()));
        table
    }

    struct LocalApiFixture(Arc<AtomicBool>, Option<thread::JoinHandle<()>>);
    impl LocalApiFixture {
        fn start() -> Self {
            let listener = UnixListener::bind(path("localapi.sock")).unwrap();
            fs::set_permissions(path("localapi.sock"), fs::Permissions::from_mode(0o666)).unwrap();
            listener.set_nonblocking(true).unwrap();
            let ips = json!([ADDRESS]);
            let status = serde_json::to_vec(&json!({
                "Version":"synthetic-helper-qualification", "BackendState":"Running", "TailscaleIPs":ips,
                "CurrentTailnet":{"Name":"fixture.invalid","MagicDNSSuffix":"fixture.ts.net"},
                "Self":{"ID":"n-host","NodeID":1,"PublicKey":format!("nodekey:{}","1".repeat(64)),
                    "UserID":7,"TailscaleIPs":ips,"InNetworkMap":true,"DNSName":"host.fixture.ts.net."}
            }))
            .unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let stopped = stop.clone();
            let worker = thread::spawn(move || {
                while !stopped.load(Ordering::Acquire) {
                    let mut s = match listener.accept() {
                        Ok((s, _)) => s,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(1));
                            continue;
                        }
                        Err(e) => panic!("fixture accept: {e}"),
                    };
                    s.set_nonblocking(false).unwrap();
                    s.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
                    let mut request = Vec::new();
                    let mut byte = [0_u8];
                    while !request.ends_with(b"\r\n\r\n") && request.len() < 2048 {
                        if s.read(&mut byte).unwrap_or(0) != 1 {
                            break;
                        }
                        request.push(byte[0]);
                    }
                    if !request.starts_with(b"GET /localapi/v0/status?peers=false ") {
                        continue;
                    }
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        status.len()
                    );
                    let _ = s.write_all(head.as_bytes());
                    let _ = s.write_all(&status);
                }
            });
            Self(stop, Some(worker))
        }
    }
    impl Drop for LocalApiFixture {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
            if let Some(worker) = self.1.take() {
                worker.join().unwrap();
            }
        }
    }

    /// Unprivileged broker role: the production `Boundary` via the helper.
    fn client(hold: u64) -> i32 {
        let can_read = Command::new("/usr/sbin/nft")
            .args(["-j", "list", "tables"])
            .stderr(Stdio::null())
            .stdout(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        println!(
            "uid-check net_admin={} can_read_ruleset={can_read}",
            fr_tailnet::ingress::net_admin()
        );
        RuntimeBuilder::new()
            .worker_threads(1)
            .enable_platform_reactor(true)
            .build()
            .unwrap()
            .block_on(async {
                let cx = Cx::current().unwrap();
                let api = LocalApi::new(path("localapi.sock")).unwrap();
                let node = match api.node_identity(&cx).await {
                    Ok(node) => node,
                    Err(error) => {
                        println!("refused Identity({error:?})");
                        return 2;
                    }
                };
                let address = SocketAddr::new(ADDRESS.parse().unwrap(), PORT);
                let config = Configuration::new(address, TAILNET)
                    .unwrap()
                    .protocols(Protocols::UDP_TCP)
                    .enforcement(Enforcement::Helper(path("ingress.sock")))
                    .unwrap();
                let mut boundary = match Boundary::install(&cx, api, node, config).await {
                    Ok(boundary) => boundary,
                    Err(error) => {
                        println!("refused {error:?}");
                        return 2;
                    }
                };
                println!("installed {}", boundary.cleanup_table());
                let supervised = boundary
                    .supervise(sleep(cx.now(), Duration::from_millis(hold)))
                    .await;
                if let Err(error) = supervised {
                    println!("fenced {error:?}");
                    return 3;
                }
                match boundary.stop(&cx).await {
                    Ok(()) => {
                        println!("removed");
                        0
                    }
                    Err(error) => {
                        println!("cleanup_failed {error:?}");
                        4
                    }
                }
            })
    }

    fn connect() -> UnixStream {
        let s = UnixStream::connect(path("ingress.sock")).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(6))).unwrap();
        s
    }
    fn read(s: &mut UnixStream) -> Option<Response> {
        let mut header = [0_u8; 4];
        s.read_exact(&mut header).ok()?;
        let mut body = vec![0_u8; usize::try_from(u32::from_be_bytes(header)).unwrap()];
        s.read_exact(&mut body).ok()?;
        Some(helper::decode_response(&body).unwrap())
    }
    fn closed(s: &mut UnixStream) -> bool {
        matches!(s.read(&mut [0_u8; 1]), Ok(0))
    }
    fn install(address: &str, port: u16, interface: &str) -> Request {
        Request::Install(Install {
            interface: interface.into(),
            address: address.parse::<IpAddr>().unwrap(),
            port,
            protocols: Protocols::UDP,
        })
    }
    fn refused(response: Response) -> Refusal {
        match response {
            Response::Refused(reason) => reason,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// Raw frames over the real socket, as the admitted uid.
    fn probe() -> i32 {
        probe_codec();
        probe_generations();
        probe_capacity();
        println!("probe complete");
        0
    }
    /// Codec violations: a typed refusal, then the helper closes.
    fn probe_codec() {
        let mut bad_family = helper::encode_request(&install(ADDRESS, PORT, TAILNET));
        bad_family[7] = 5;
        for (frame, expected) in [
            (vec![0x01, 0x00], Refusal::Oversized),
            (
                vec![0x00, 0x02, helper::VERSION, 0x09],
                Refusal::UnknownOperation,
            ),
            (bad_family, Refusal::Malformed),
        ] {
            let mut s = connect();
            s.write_all(&frame).unwrap();
            let got = read(&mut s);
            assert_eq!(got, Some(Response::Refused(expected)));
            assert!(closed(&mut s), "helper must close after {expected:?}");
            println!("probe: frame {frame:02x?} -> {got:?}, then closed");
        }
    }
    /// Semantic refusals and generation fencing on one connection, paced
    /// below the per-connection request budget.
    fn probe_generations() {
        let mut s = connect();
        let mut call = |request: Request| {
            thread::sleep(Duration::from_millis(300));
            s.write_all(&helper::encode_request(&request)).unwrap();
            let got = read(&mut s).unwrap();
            println!("probe: {request:?} -> {got:?}");
            got
        };
        assert_eq!(
            refused(call(install(ADDRESS, PORT, "eth0"))),
            Refusal::InterfaceNotConfigured
        );
        assert_eq!(
            refused(call(install(ADDRESS, 0, TAILNET))),
            Refusal::InvalidPort
        );
        assert_eq!(
            refused(call(install("100.64.0.9", PORT, TAILNET))),
            Refusal::AddressNotAssigned
        );
        assert_eq!(
            refused(call(Request::Renew { generation: 1 })),
            Refusal::NotInstalled
        );
        let Response::Installed {
            generation: first, ..
        } = call(install(ADDRESS, PORT, TAILNET))
        else {
            panic!("install refused");
        };
        assert!(matches!(
            call(Request::Renew { generation: first }),
            Response::Renewed { .. }
        ));
        assert_eq!(
            refused(call(install(ADDRESS, PORT, TAILNET))),
            Refusal::AlreadyInstalled
        );
        assert_eq!(
            call(Request::Remove { generation: first }),
            Response::Removed { generation: first }
        );
        assert_eq!(
            refused(call(Request::Renew { generation: first })),
            Refusal::NotInstalled
        );
        let Response::Installed {
            generation: second, ..
        } = call(install(ADDRESS, PORT, TAILNET))
        else {
            panic!("second install refused");
        };
        assert!(second > first);
        assert_eq!(
            refused(call(Request::Renew { generation: first })),
            Refusal::StaleGeneration
        );
        assert_eq!(
            refused(call(Request::Remove { generation: first })),
            Refusal::StaleGeneration
        );
        assert!(matches!(
            call(Request::Renew { generation: second }),
            Response::Renewed { .. }
        ));
    }
    /// Connection cap: refill the accept budget, hold eight, then a ninth.
    fn probe_capacity() {
        thread::sleep(Duration::from_millis(2500));
        let held: Vec<UnixStream> = (0..8).map(|_| connect()).collect();
        thread::sleep(Duration::from_millis(400));
        let mut ninth = connect();
        let got = read(&mut ninth);
        assert_eq!(got, Some(Response::Refused(Refusal::CapacityExhausted)));
        println!("probe: ninth concurrent connection -> {got:?}");
        let mut tenth = connect();
        let got = read(&mut tenth);
        assert_eq!(got, Some(Response::Refused(Refusal::RateLimited)));
        println!("probe: immediate tenth connection -> {got:?}");
        drop(held);
    }

    fn prepare(frd: &Path) -> (LocalApiFixture, Receivers) {
        assert_ne!(
            fs::read_link("/proc/self/ns/net").unwrap(),
            fs::read_link("/proc/1/ns/net").unwrap(),
            "fresh network namespace required"
        );
        assert!(
            fr_tailnet::ingress::net_admin(),
            "root in the namespace required"
        );
        network();
        fs::create_dir(DIR).unwrap();
        fs::set_permissions(DIR, fs::Permissions::from_mode(0o755)).unwrap();
        for (from, to) in [
            (frd.to_path_buf(), "frd"),
            (std::env::current_exe().unwrap(), "qualify"),
        ] {
            fs::copy(from, path(to)).unwrap();
            fs::set_permissions(path(to), fs::Permissions::from_mode(0o755)).unwrap();
        }
        let config = json!({"interface": TAILNET, "allowed_uids": [ALLOWED], "socket": path("ingress.sock")});
        fs::write(path("helper.json"), config.to_string()).unwrap();
        fs::set_permissions(path("helper.json"), fs::Permissions::from_mode(0o644)).unwrap();
        println!(
            "[config] {} (root, 0644): {config}",
            path("helper.json").display()
        );
        let api = LocalApiFixture::start();
        let rx = Receivers::bind();
        println!("== baseline: no rule installed ==");
        udp_delivered_via_lan(&rx, "baseline routing works without any rule");
        assert_eq!(lan("tcp", PORT), "connected");
        rx.drain_tcp();
        pass("TCP to 100.64.0.1:4710 via frveth0 connects (baseline)");
        (api, rx)
    }

    fn peer_credentials(helper_log: &Log) {
        println!("== SO_PEERCRED: uid {DENIED} is not in allowed_uids ==");
        let (mut denied, denied_log) = as_uid(DENIED, &["client", "1000"]);
        let status = exit(&mut denied, Duration::from_secs(10));
        until("the uid 65533 outcome", Duration::from_secs(1), || {
            denied_log.find("refused ").is_some() || denied_log.find("removed").is_some()
        });
        let outcome = denied_log
            .find("refused ")
            .or_else(|| denied_log.find("installed "));
        assert_eq!(
            outcome.as_deref(),
            Some("refused HelperRefused(PeerNotAllowed)"),
            "a connection from a disallowed uid must be refused by SO_PEERCRED"
        );
        assert_eq!(status.code(), Some(2));
        assert_eq!(helper_tables(), Vec::<String>::new());
        helper_log.wait(
            &format!("refused uid {DENIED}: peer_not_allowed"),
            Duration::from_secs(2),
        );
        pass("uid 65533 refused by SO_PEERCRED (peer_not_allowed); no table created");
    }

    fn enforcement_and_broker_crash(rx: &Receivers, helper_log: &Log) {
        println!("== unprivileged broker (uid {ALLOWED}) installs udp+tcp via the helper ==");
        let (mut broker, broker_log) = as_uid(ALLOWED, &["client", "60000"]);
        let table = installed(&broker_log);
        assert!(
            broker_log
                .find("uid-check net_admin=false can_read_ruleset=false")
                .is_some(),
            "the broker must be unprivileged and unable to read the ruleset"
        );
        assert_eq!(helper_tables(), vec![table.clone()]);
        let listing = Command::new("/usr/sbin/nft")
            .args(["list", "table", "inet", &table])
            .output()
            .unwrap();
        println!(
            "[nft list table inet {table}]\n{}",
            String::from_utf8_lossy(&listing.stdout)
        );
        udp_dropped_via_lan(rx);
        assert_eq!(
            lan("tcp", PORT),
            "timeout",
            "TCP SYN via frveth0 must be DROPPED"
        );
        pass("TCP SYN to 100.64.0.1:4710 via frveth0 (non-tailnet) DROPPED (connect timed out)");
        assert_eq!(tun("udp", PORT), "sent");
        let got = Receivers::datagram(&rx.udp, Duration::from_secs(2));
        assert_eq!(
            got.as_ref().map(|(d, _)| d.as_slice()),
            Some(&b"via-tailscale0"[..])
        );
        pass(&format!(
            "UDP to 100.64.0.1:4710 via {TAILNET} PASSED (from {})",
            got.unwrap().1
        ));
        assert_eq!(
            tun("syn", PORT),
            "synack",
            "TCP SYN via the tailnet interface must pass"
        );
        rx.drain_tcp();
        pass(&format!(
            "TCP SYN to 100.64.0.1:4710 via {TAILNET} PASSED (SYN-ACK returned)"
        ));
        assert_eq!(lan("udp", UNRELATED), "sent");
        assert!(Receivers::datagram(&rx.unrelated, Duration::from_secs(2)).is_some());
        pass("UDP to unrelated port 4712 via frveth0 still delivered (exact-port rule)");
        thread::sleep(Duration::from_millis(1500));
        assert!(
            broker.try_wait().unwrap().is_none(),
            "broker must stay live across renewals"
        );
        helper_log.wait(&format!("renewed {table}"), Duration::from_secs(1));
        pass("renewals over the helper connection keep the broker's lease live");

        println!("== SIGKILL the broker: the helper removes its rule ==");
        run("/usr/bin/kill", &["-KILL", &broker.id().to_string()]);
        exit(&mut broker, Duration::from_secs(5));
        let elapsed = until(
            "helper removed the rule after broker SIGKILL",
            Duration::from_secs(3),
            || helper_tables().is_empty(),
        );
        helper_log.wait(&format!("removed {table}"), Duration::from_secs(1));
        assert!(helper_log.find("(connection closed)").is_some());
        pass(&format!(
            "broker SIGKILL -> helper removed {table} within {} ms",
            elapsed.as_millis()
        ));
        udp_delivered_via_lan(rx, "rule gone after the broker died");
    }

    fn clean_stop_and_raw_protocol(helper_log: &Log) {
        println!("== clean stop: acknowledged removal by generation ==");
        let (mut broker, broker_log) = as_uid(ALLOWED, &["client", "1500"]);
        let table = installed(&broker_log);
        let status = exit(&mut broker, Duration::from_secs(10));
        assert_eq!(status.code(), Some(0));
        assert!(broker_log.find("removed").is_some());
        assert_eq!(helper_tables(), Vec::<String>::new());
        helper_log.wait(&format!("removed {table}"), Duration::from_secs(1));
        pass("Boundary::stop removed the helper rule (requested) after supervised renewals");

        println!("== raw protocol over the real socket (uid {ALLOWED}) ==");
        let (mut prober, _probe_log) = as_uid(ALLOWED, &["probe"]);
        let status = exit(&mut prober, Duration::from_secs(40));
        assert_eq!(status.code(), Some(0), "probe failed");
        until("probe connections released", Duration::from_secs(3), || {
            helper_tables().is_empty()
        });
        pass(
            "codec violations, wrong interface/port/address, stale generations, connection cap and accept rate refused over the real socket",
        );
        // The probe's flood spent this uid's accept budget; let it refill.
        thread::sleep(Duration::from_millis(2500));
    }

    fn interface_loss() {
        println!("== {TAILNET} disappears under a live rule ==");
        let (mut broker, broker_log) = as_uid(ALLOWED, &["client", "60000"]);
        let table = installed(&broker_log);
        run("/usr/sbin/ip", &["link", "delete", TAILNET]);
        let status = exit(&mut broker, Duration::from_secs(5));
        assert_eq!(status.code(), Some(3));
        let fenced = broker_log.wait("fenced", Duration::from_secs(1));
        until(
            "rule removed after the fenced broker exited",
            Duration::from_secs(3),
            || helper_tables().is_empty(),
        );
        pass(&format!(
            "interface loss fenced the broker ({fenced}); {table} removed"
        ));
        tailnet_interface();
    }

    fn helper_crash(rx: &Receivers, mut helper_child: Child) -> Child {
        println!("== helper crash: residue stays restrictive, then is reclaimed at start ==");
        let (mut broker, broker_log) = as_uid(ALLOWED, &["client", "60000"]);
        let table = installed(&broker_log);
        run("/usr/bin/kill", &["-KILL", &helper_child.id().to_string()]);
        exit(&mut helper_child, Duration::from_secs(5));
        assert_eq!(helper_tables(), vec![table.clone()]);
        udp_dropped_via_lan(rx);
        pass("after helper SIGKILL the orphaned rule still enforces (restrictive residue)");
        let status = exit(&mut broker, Duration::from_secs(5));
        assert_eq!(status.code(), Some(3));
        let fenced = broker_log.wait("fenced", Duration::from_secs(1));
        pass(&format!("broker fenced on its next renewal ({fenced})"));
        let foreign = "frd_00000000000000000000000000000000";
        nft(&format!(
            "add table inet {foreign}\nadd table inet fr_sentinel\n"
        ));
        let (restarted, restarted_log) = start_helper();
        restarted_log.wait("reclaimed 1 stale helper table(s)", Duration::from_secs(1));
        assert_eq!(helper_tables(), Vec::<String>::new());
        let remaining = tables();
        assert!(
            remaining.contains(&foreign.to_owned())
                && remaining.contains(&"fr_sentinel".to_owned())
        );
        pass(&format!(
            "restart reclaimed {table}; foreign tables {foreign} and fr_sentinel untouched"
        ));
        nft(&format!(
            "delete table inet {foreign}\ndelete table inet fr_sentinel\n"
        ));
        restarted
    }

    fn graceful_stop(mut helper_child: Child) {
        println!("== SIGTERM the helper under a live rule ==");
        let (mut broker, broker_log) = as_uid(ALLOWED, &["client", "60000"]);
        installed(&broker_log);
        run("/usr/bin/kill", &["-TERM", &helper_child.id().to_string()]);
        let status = exit(&mut broker, Duration::from_secs(5));
        assert_eq!(status.code(), Some(3));
        let status = exit(&mut helper_child, Duration::from_secs(10));
        assert!(status.success(), "helper exit: {status}");
        assert_eq!(helper_tables(), Vec::<String>::new());
        pass(
            "SIGTERM: connections closed, broker fenced, tables reclaimed after the grace period, clean exit",
        );
    }

    fn orchestrate(frd: &Path) {
        let (_api, rx) = prepare(frd);
        println!("== helper start (real `frd ingress-helper`, root) ==");
        let (helper_child, helper_log) = start_helper();
        peer_credentials(&helper_log);
        enforcement_and_broker_crash(&rx, &helper_log);
        clean_stop_and_raw_protocol(&helper_log);
        interface_loss();
        let helper_child = helper_crash(&rx, helper_child);
        graceful_stop(helper_child);
        println!(
            "passed: real nftables in a private namespace; unprivileged broker via root helper; \
             udp+tcp drop via frveth0 and pass via {TAILNET}; SO_PEERCRED refusal; rule removed on \
             broker SIGKILL and on stop; generation fencing, codec, cap and rate refusals over the \
             real socket; interface loss; helper crash reclaim; graceful helper stop"
        );
    }

    pub fn main() {
        let args: Vec<String> = std::env::args().skip(1).collect();
        match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
            ["client", hold] => std::process::exit(client(hold.parse().unwrap())),
            ["probe"] => std::process::exit(probe()),
            [frd] if frd.starts_with('/') => orchestrate(Path::new(frd)),
            _ => panic!("usage: qualify_ingress_helper /absolute/path/to/frd"),
        }
    }
}
#[cfg(target_os = "linux")]
fn main() {
    linux::main();
}
#[cfg(not(target_os = "linux"))]
fn main() {
    panic!("Linux namespace qualification only");
}

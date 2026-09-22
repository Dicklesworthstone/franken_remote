//! Explicit privileged qualification, never a shipping listener or a live tailnet.
//! Run only as root inside a fresh network namespace. The `LocalAPI` responses are
//! synthetic; nftables, TUN delivery, socket ownership and timers are real.
#[cfg(target_os = "linux")]
mod linux {
    use asupersync::{
        cx::Cx,
        net::quic_native::{QuicUdpEndpoint, QuicUdpEndpointConfig},
        runtime::RuntimeBuilder,
        time::{sleep, timeout},
    };
    use fr_tailnet::{
        Error, LocalApi,
        ingress::{Boundary, Configuration, Error as IngressError},
    };
    use serde_json::json;
    use std::{
        fs,
        io::{Read, Write},
        net::UdpSocket,
        os::unix::net::UnixListener,
        path::PathBuf,
        process::{Command, Stdio},
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        thread,
        time::Duration,
    };
    fn run(program: &str, args: &[&str]) {
        assert!(Command::new(program).args(args).status().unwrap().success());
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
        assert!(child.wait().unwrap().success());
    }
    fn tun_packet(port: u16, payload: &[u8], ipv6: bool) {
        // Trusted test helper writes a real IP packet into an actual TUN. The
        // production Rust contains no unsafe code or arbitrary-command surface.
        let code = r"
import fcntl, os, socket, struct, sys
fd=os.open('/dev/net/tun',os.O_RDWR)
fcntl.ioctl(fd,0x400454ca,struct.pack('16sH',b'fr-tun',0x1001))
data=bytes.fromhex(sys.argv[2]); port=int(sys.argv[1]); v6=sys.argv[3]=='true'
def check(b):
 if len(b)%2:b+=b'\0'
 n=sum(struct.unpack('!%dH'%(len(b)//2),b));n=(n>>16)+(n&65535);n+=(n>>16)
 return (~n)&65535
udp=struct.pack('!HHHH',30001,port,len(data)+8,0)+data
if v6:
 src=socket.inet_pton(socket.AF_INET6,'fd7a:115c:a1e0::2')
 dst=socket.inet_pton(socket.AF_INET6,'fd7a:115c:a1e0::1')
 c=check(src+dst+struct.pack('!I3xB',len(udp),17)+udp) or 65535
 udp=udp[:6]+struct.pack('!H',c)+udp[8:]
 header=struct.pack('!IHBB16s16s',6<<28,len(udp),17,64,src,dst)
else:
 src=socket.inet_aton('100.64.0.2');dst=socket.inet_aton('100.64.0.1')
 header=struct.pack('!BBHHHBBH4s4s',0x45,0,len(udp)+20,1,0,64,17,0,src,dst)
 header=header[:10]+struct.pack('!H',check(header))+header[12:]
os.write(fd,header+udp)
os.close(fd)
";
        let hex = payload.iter().fold(String::new(), |mut output, byte| {
            use std::fmt::Write;
            write!(&mut output, "{byte:02x}").unwrap();
            output
        });
        run(
            "/usr/bin/python3",
            &["-c", code, &port.to_string(), &hex, &ipv6.to_string()],
        );
    }
    struct Fixture {
        api: LocalApi,
        value: Arc<Mutex<serde_json::Value>>,
        stop: Arc<AtomicBool>,
        thread: Option<thread::JoinHandle<()>>,
    }
    impl Fixture {
        fn new() -> Self {
            let path = PathBuf::from(format!("/tmp/fr-ingress-{}.sock", std::process::id()));
            let listener = UnixListener::bind(&path).unwrap();
            listener.set_nonblocking(true).unwrap();
            let ips = json!(["100.64.0.1", "fd7a:115c:a1e0::1"]);
            let value = Arc::new(Mutex::new(json!({
                "Version":"synthetic-ingress-qualification", "BackendState":"Running", "TailscaleIPs":ips,
                "CurrentTailnet":{"Name":"fixture.invalid","MagicDNSSuffix":"fixture.ts.net"},
                "Self":{"ID":"n-host","NodeID":1,"PublicKey":format!("nodekey:{}","1".repeat(64)),
                    "UserID":7,"TailscaleIPs":ips,"InNetworkMap":true,"DNSName":"host.fixture.ts.net."}
            })));
            let stop = Arc::new(AtomicBool::new(false));
            let (stopped, served) = (stop.clone(), value.clone());
            let worker = thread::spawn(move || {
                while !stopped.load(Ordering::Acquire) {
                    let (mut s, _) = match listener.accept() {
                        Ok(v) => v,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(1));
                            continue;
                        }
                        Err(e) => panic!("fixture accept: {e}"),
                    };
                    s.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
                    let mut req = Vec::new();
                    let mut b = [0_u8];
                    while !req.ends_with(b"\r\n\r\n") && req.len() < 2048 {
                        if s.read(&mut b).unwrap_or(0) != 1 {
                            break;
                        }
                        req.push(b[0]);
                    }
                    assert!(req.starts_with(b"GET /localapi/v0/status?peers=false "));
                    let bytes = serde_json::to_vec(&*served.lock().unwrap()).unwrap();
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        bytes.len()
                    );
                    s.write_all(head.as_bytes()).unwrap();
                    s.write_all(&bytes).unwrap();
                }
            });
            Self {
                api: LocalApi::new(&path).unwrap(),
                value,
                stop,
                thread: Some(worker),
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            self.thread.take().unwrap().join().unwrap();
        }
    }
    async fn bind_endpoint(
        f: &Fixture,
        cx: &Cx,
        port: u16,
        ipv6: bool,
    ) -> (Boundary, QuicUdpEndpoint) {
        let address = if ipv6 {
            format!("[fd7a:115c:a1e0::1]:{port}")
        } else {
            format!("100.64.0.1:{port}")
        }
        .parse()
        .unwrap();
        let node = f.api.node_identity(cx).await.unwrap();
        let boundary = Boundary::install(
            cx,
            f.api.clone(),
            node,
            Configuration::new(address, "fr-tun").unwrap(),
        )
        .await
        .unwrap();
        let endpoint = QuicUdpEndpoint::bind(cx, address, QuicUdpEndpointConfig::default())
            .await
            .unwrap();
        (boundary, endpoint)
    }
    async fn no_packet(cx: &Cx, endpoint: &mut QuicUdpEndpoint) {
        assert!(
            timeout(
                cx.now(),
                Duration::from_millis(100),
                endpoint.receive_batch(cx, 1)
            )
            .await
            .is_err(),
            "non-TUN packet escaped drop-only boundary"
        );
    }
    async fn receive(cx: &Cx, endpoint: &mut QuicUdpEndpoint, expected: &[u8]) {
        let packets = timeout(
            cx.now(),
            Duration::from_secs(1),
            endpoint.receive_batch(cx, 1),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].data, expected);
    }
    pub fn main() {
        assert_ne!(
            fs::read_link("/proc/self/ns/net").unwrap(),
            fs::read_link("/proc/1/ns/net").unwrap(),
            "fresh network namespace required"
        );
        run("/usr/sbin/ip", &["link", "set", "lo", "up"]);
        run(
            "/usr/sbin/ip",
            &["tuntap", "add", "dev", "fr-tun", "mode", "tun"],
        );
        run(
            "/usr/sbin/ip",
            &["address", "add", "100.64.0.1/32", "dev", "fr-tun"],
        );
        run(
            "/usr/sbin/ip",
            &[
                "-6",
                "address",
                "add",
                "fd7a:115c:a1e0::1/128",
                "dev",
                "fr-tun",
                "nodad",
            ],
        );
        run("/usr/sbin/ip", &["link", "set", "fr-tun", "up"]);
        run(
            "/usr/sbin/ip",
            &["address", "add", "100.64.0.2/32", "dev", "lo"],
        );
        run(
            "/usr/sbin/ip",
            &[
                "-6",
                "address",
                "add",
                "fd7a:115c:a1e0::2/128",
                "dev",
                "lo",
                "nodad",
            ],
        );
        // Namespace-only routing setup so the firewall, not reverse-path checks,
        // distinguishes the same numeric source arriving from lo versus TUN.
        fs::write("/proc/sys/net/ipv4/conf/all/rp_filter", b"0").unwrap();
        fs::write("/proc/sys/net/ipv4/conf/fr-tun/rp_filter", b"0").unwrap();
        fs::write("/proc/sys/net/ipv4/conf/fr-tun/accept_local", b"1").unwrap();
        nft(
            "create table inet fr_sentinel\nadd chain inet fr_sentinel input { type filter hook input priority 100; policy accept; }\n",
        );
        let fixture = Fixture::new();
        RuntimeBuilder::new().worker_threads(2).enable_platform_reactor(true).build().unwrap().block_on(async {
            let cx = Cx::current().unwrap();
            let (mut b4, mut e4) = bind_endpoint(&fixture, &cx, 4710, false).await;
            let (mut b6, mut e6) = bind_endpoint(&fixture, &cx, 4711, true).await;
            let (lease4, lease6) = (b4.lease().unwrap(), b6.lease().unwrap());
            let lan = UdpSocket::bind("100.64.0.2:30001").unwrap();
            lan.send_to(b"outside-tun", "100.64.0.1:4710").unwrap(); no_packet(&cx, &mut e4).await;
            let lan6 = UdpSocket::bind("[fd7a:115c:a1e0::2]:30001").unwrap();
            lan6.send_to(b"outside-tun-v6", "[fd7a:115c:a1e0::1]:4711").unwrap(); no_packet(&cx, &mut e6).await;
            let unrelated = UdpSocket::bind("100.64.0.1:4712").unwrap();
            unrelated.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
            lan.send_to(b"unrelated-port", "100.64.0.1:4712").unwrap();
            let mut bytes=[0;64]; let size=unrelated.recv(&mut bytes).unwrap(); assert_eq!(&bytes[..size],b"unrelated-port");
            tun_packet(4710,b"real-ipv4-tun",false); receive(&cx,&mut e4,b"real-ipv4-tun").await;
            tun_packet(4711,b"real-ipv6-tun",true); receive(&cx,&mut e6,b"real-ipv6-tun").await;
            assert_eq!(lease4.check(b4.address()),Ok(()));
            assert_eq!(lease6.check(b4.address()),Err(IngressError::InvalidConfiguration));
            nft("add rule inet fr_sentinel input ip daddr 100.64.0.1 udp dport 4710 drop\n");
            tun_packet(4710,b"other-policy-denied",false); no_packet(&cx,&mut e4).await;
            nft("delete table inet fr_sentinel\n");
            drop(b6.stop(&cx)); assert!(b6.is_closed()); assert_eq!(b6.stop(&cx).await,Err(IngressError::InUse));
            drop(e6); drop(lease6); b6.stop(&cx).await.unwrap(); b6.stop(&cx).await.unwrap();
            fixture.value.lock().unwrap()["Self"]["PublicKey"]=json!(format!("nodekey:{}","2".repeat(64)));
            assert!(matches!(b4.revalidate().await,Err(IngressError::Identity(Error::IdentityChanged))));
            assert!(lease4.check(b4.address()).is_err()); drop(e4);drop(lease4);b4.stop(&cx).await.unwrap();
            let (mut owner, endpoint) = bind_endpoint(&fixture,&cx,4713,false).await;
            let lease = owner.lease().unwrap(); let address=owner.address();
            owner.supervise(async {
                sleep(cx.now(),Duration::from_millis(3300)).await;
                assert_eq!(lease.check(address),Ok(()),"renewal must outlive initial snapshot");
            }).await.unwrap();
            assert!(lease.check(address).is_err()); assert_eq!(owner.stop(&cx).await,Err(IngressError::InUse));
            drop(endpoint);drop(lease);owner.stop(&cx).await.unwrap();
            let (mut owner, endpoint) = bind_endpoint(&fixture,&cx,4714,false).await;
            nft(&format!("flush chain inet {} input\n",owner.cleanup_table()));
            assert_eq!(owner.revalidate().await,Err(IngressError::FirewallMismatch));assert!(owner.is_closed());
            drop(endpoint);owner.stop(&cx).await.unwrap();
            println!("passed: real IPv4/IPv6 TUN ingress, wrong-interface drops, unrelated-port and other-policy preservation, unchanged/changed node identity, renewal beyond initial expiry, tampered-rule refusal and retained-lease cleanup fencing");
        });
        let rows = Command::new("/usr/sbin/nft")
            .args(["-j", "list", "tables", "inet"])
            .output()
            .unwrap();
        assert!(rows.status.success());
        let value: serde_json::Value = serde_json::from_slice(&rows.stdout).unwrap();
        assert!(value["nftables"].as_array().unwrap().iter().all(|r| {
            !r["table"]["name"]
                .as_str()
                .is_some_and(|n| n.starts_with("frd_"))
        }));
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

use asupersync::{
    cx::Cx,
    net::{
        quic_core::{ConnectionId, TransportParameters},
        quic_native::{
            NativeQuicConnectionConfig, NativeQuicUdpConnection, NativeQuicUdpConnectionError,
            QuicUdpEndpoint, QuicUdpEndpointConfig,
            handshake_driver::{QuicHandshakeDriver, client_config, server_config},
        },
    },
    runtime::{Runtime, RuntimeBuilder},
    time::timeout,
};
use fr_transport::quic::{
    ALPN, DatagramRoute, Messages, Policy, Priority, QuicRecords, StreamRoute,
};
use std::{
    future::{Future, poll_fn},
    path::{Path, PathBuf},
    pin::{Pin, pin},
    process::Command,
    sync::OnceLock,
    task::Poll,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub fn runtime() -> Runtime {
    RuntimeBuilder::new()
        .worker_threads(2)
        .enable_platform_reactor(true)
        .build()
        .unwrap()
}
pub fn clock(cx: &Cx) -> u64 {
    cx.timer_driver().unwrap().now().as_nanos() / 1000
}
pub async fn both<A: Future, B: Future>(a: A, b: B) -> (A::Output, B::Output) {
    let (mut a, mut b) = (pin!(a), pin!(b));
    let (mut ar, mut br) = (None, None);
    poll_fn(|cx| {
        if ar.is_none()
            && let Poll::Ready(v) = a.as_mut().poll(cx)
        {
            ar = Some(v);
        }
        if br.is_none()
            && let Poll::Ready(v) = b.as_mut().poll(cx)
        {
            br = Some(v);
        }
        if ar.is_some() && br.is_some() {
            Poll::Ready((ar.take().unwrap(), br.take().unwrap()))
        } else {
            Poll::Pending
        }
    })
    .await
}
fn openssl(dir: &Path, args: &[&str]) {
    let output = Command::new("openssl")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("test requires OpenSSL");
    assert!(
        output.status.success(),
        "OpenSSL failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
pub fn pki() -> &'static Path {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("fr-quic-pki-{}-{stamp}", std::process::id()));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&dir).unwrap();
        openssl(&dir, &["req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256", "-nodes", "-keyout", "ca.key", "-out", "ca.pem", "-days", "1", "-subj", "/CN=FrankenRemote ephemeral test CA", "-addext", "basicConstraints=critical,CA:TRUE"]);
        openssl(&dir, &["req", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256", "-nodes", "-keyout", "leaf.key", "-out", "leaf.csr", "-subj", "/CN=localhost"]);
        std::fs::write(dir.join("extensions"), "subjectAltName=DNS:localhost\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\n").unwrap();
        openssl(&dir, &["x509", "-req", "-in", "leaf.csr", "-CA", "ca.pem", "-CAkey", "ca.key", "-CAcreateserial", "-days", "1", "-extfile", "extensions", "-out", "leaf.pem"]);
        openssl(&dir, &["x509", "-in", "ca.pem", "-outform", "DER", "-out", "ca.der"]);
        openssl(&dir, &["x509", "-in", "leaf.pem", "-outform", "DER", "-out", "leaf.der"]);
        openssl(&dir, &["pkcs8", "-topk8", "-nocrypt", "-in", "leaf.key", "-outform", "DER", "-out", "key.der"]);
        dir
    })
}
pub type NativeResult = Result<NativeQuicUdpConnection, NativeQuicUdpConnectionError>;
pub fn native_pair<'a>(
    cx: &'a Cx,
    name: &'a str,
    alpn: &'a [u8],
) -> Pin<Box<impl Future<Output = (NativeResult, NativeResult)> + 'a>> {
    native_pair_with_windows(cx, name, alpn, 65536, 524_288)
}
pub fn native_pair_with_windows<'a>(
    cx: &'a Cx,
    name: &'a str,
    alpn: &'a [u8],
    stream_window: u64,
    connection_window: u64,
) -> Pin<Box<impl Future<Output = (NativeResult, NativeResult)> + 'a>> {
    Box::pin(async move {
        let cfg = NativeQuicConnectionConfig {
            max_local_bidi: 0,
            max_local_uni: 8,
            send_window: stream_window,
            recv_window: stream_window,
            connection_send_limit: connection_window,
            connection_recv_limit: connection_window,
            max_datagram_frame_size: 1200,
            ..Default::default()
        };
        let mut parameters = vec![];
        TransportParameters {
            initial_max_data: Some(cfg.connection_recv_limit),
            initial_max_stream_data_bidi_local: Some(cfg.recv_window),
            initial_max_stream_data_bidi_remote: Some(cfg.recv_window),
            initial_max_stream_data_uni: Some(cfg.recv_window),
            initial_max_streams_bidi: Some(0),
            initial_max_streams_uni: Some(8),
            max_datagram_frame_size: Some(1200),
            ..Default::default()
        }
        .encode(&mut parameters)
        .unwrap();
        let dir = pki();
        let client = client_config(
            vec![std::fs::read(dir.join("ca.der")).unwrap().into()],
            vec![alpn.to_vec()],
        )
        .unwrap();
        let server = server_config(
            vec![std::fs::read(dir.join("leaf.der")).unwrap().into()],
            std::fs::read(dir.join("key.der"))
                .unwrap()
                .try_into()
                .unwrap(),
            vec![alpn.to_vec()],
        )
        .unwrap();
        let cd = QuicHandshakeDriver::client(
            client,
            name.to_string().try_into().unwrap(),
            parameters.clone(),
        )
        .unwrap();
        let sd = QuicHandshakeDriver::server(server, parameters).unwrap();
        let ce = QuicUdpEndpoint::bind(
            cx,
            "127.0.0.1:0".parse().unwrap(),
            QuicUdpEndpointConfig::default(),
        )
        .await
        .unwrap();
        let se = QuicUdpEndpoint::bind(
            cx,
            "127.0.0.1:0".parse().unwrap(),
            QuicUdpEndpointConfig::default(),
        )
        .await
        .unwrap();
        let address = se.local_addr();
        let dcid = ConnectionId::new(b"fr-dcid1").unwrap();
        let c = NativeQuicUdpConnection::connect(
            cx,
            ce,
            address,
            cd,
            dcid,
            ConnectionId::new(b"fr-cli01").unwrap(),
            cfg,
            alpn,
        );
        let s = NativeQuicUdpConnection::accept(
            cx,
            se,
            sd,
            dcid,
            ConnectionId::new(b"fr-srv01").unwrap(),
            cfg,
            alpn,
        );
        // Each side's bounded handshake future can fail independently. In the
        // certificate-refusal case the silent opposite side may reach this timeout.
        let (c, s) = Box::pin(both(
            timeout(cx.now(), Duration::from_secs(3), c),
            timeout(cx.now(), Duration::from_secs(3), s),
        ))
        .await;
        (
            c.unwrap_or(Err(NativeQuicUdpConnectionError::Cancelled)),
            s.unwrap_or(Err(NativeQuicUdpConnectionError::Cancelled)),
        )
    })
}
pub struct Pair {
    pub client: QuicRecords,
    pub server: QuicRecords,
    pub host_routes: [StreamRoute; 3],
    pub video: DatagramRoute,
}
pub fn pair(cx: &Cx, policy: Policy) -> Pin<Box<impl Future<Output = Pair> + '_>> {
    Box::pin(async move {
        let (c, s) = native_pair(cx, "localhost", ALPN).await;
        let (mut c, mut s) = (c.unwrap(), s.unwrap());
        let progress = s.connection_mut().open_uni_stream(cx).unwrap();
        let recovery = s.connection_mut().open_uni_stream(cx).unwrap();
        let repair = c.connection_mut().open_uni_stream(cx).unwrap();
        let routes = [
            StreamRoute {
                stream: progress,
                binding: 3,
                messages: Messages::Exact(0x37),
                priority: Priority::Critical,
                outbound: true,
                maximum: 1150,
            },
            StreamRoute {
                stream: recovery,
                binding: 2,
                messages: Messages::Exact(0x32),
                priority: Priority::Bulk,
                outbound: true,
                maximum: 65536.min(policy.retained_send_bytes),
            },
            StreamRoute {
                stream: repair,
                binding: 4,
                messages: Messages::Exact(0x35),
                priority: Priority::Critical,
                outbound: false,
                maximum: 1150,
            },
        ];
        let incoming = routes.map(|r| StreamRoute {
            outbound: !r.outbound,
            ..r
        });
        let video = DatagramRoute {
            binding: 1,
            kind: 0x34,
            outbound: true,
        };
        let client = QuicRecords::new(
            c,
            cx,
            &incoming,
            &[DatagramRoute {
                outbound: false,
                ..video
            }],
            policy,
        )
        .unwrap();
        let server = QuicRecords::new(s, cx, &routes, &[video], policy).unwrap();
        Pair {
            client,
            server,
            host_routes: routes,
            video,
        }
    })
}
pub fn drive<'a>(cx: &'a Cx, pair: &'a mut Pair) -> Pin<Box<impl Future<Output = ()> + 'a>> {
    Box::pin(async move {
        let (c, s) = Box::pin(both(
            pair.client.drive(cx, Duration::from_millis(1), || true),
            pair.server.drive(cx, Duration::from_millis(1), || true),
        ))
        .await;
        c.unwrap();
        s.unwrap();
    })
}

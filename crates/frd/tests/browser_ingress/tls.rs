//! Actual TLS/SNI coverage with an ephemeral, explicitly trusted test CA.
//! There is no certificate-verification bypass or installed-tailnet claim.

use super::*;
use asupersync::tls::{Certificate, CertificateChain, PrivateKey, TlsAcceptor, TlsConnector};
use std::{path::PathBuf, process::Command, sync::OnceLock};

const OTHER_SNI: &str = "other.tailnet.ts.net";

fn credentials() -> &'static PathBuf {
    static DIRECTORY: OnceLock<PathBuf> = OnceLock::new();
    DIRECTORY.get_or_init(|| {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "fr-browser-tls-{}-{stamp}",
            std::process::id()
        ));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&directory).unwrap();
        let openssl = |arguments: &[&str]| {
            let output = Command::new("openssl")
                .current_dir(&directory)
                .args(arguments)
                .output()
                .expect("TLS regression requires OpenSSL");
            assert!(output.status.success(), "test certificate generation failed: {}", String::from_utf8_lossy(&output.stderr));
        };
        openssl(&["req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256", "-nodes", "-keyout", "ca.key", "-out", "ca.pem", "-days", "1", "-subj", "/CN=FrankenRemote ephemeral browser test CA", "-addext", "basicConstraints=critical,CA:TRUE"]);
        openssl(&["req", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256", "-nodes", "-keyout", "leaf.key", "-out", "leaf.csr", "-subj", "/CN=FrankenRemote browser test"]);
        std::fs::write(directory.join("extensions"), format!("subjectAltName=DNS:{EXPECTED_HOST},DNS:{OTHER_SNI}\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\n")).unwrap();
        openssl(&["x509", "-req", "-in", "leaf.csr", "-CA", "ca.pem", "-CAkey", "ca.key", "-CAcreateserial", "-days", "1", "-extfile", "extensions", "-out", "leaf.pem"]);
        directory
    })
}

fn endpoints() -> (TlsAcceptor, TlsConnector) {
    let directory = credentials();
    let chain =
        CertificateChain::from_pem(&std::fs::read(directory.join("leaf.pem")).unwrap()).unwrap();
    let key = PrivateKey::from_pem(&std::fs::read(directory.join("leaf.key")).unwrap()).unwrap();
    let roots = Certificate::from_pem(&std::fs::read(directory.join("ca.pem")).unwrap()).unwrap();
    (
        TlsAcceptor::builder(chain, key)
            .require_sni()
            .alpn_protocols(vec![b"http/1.1".to_vec()])
            .build()
            .unwrap(),
        TlsConnector::builder()
            .add_root_certificates(roots)
            .alpn_protocols(vec![b"http/1.1".to_vec()])
            .build()
            .unwrap(),
    )
}

#[test]
fn https_bootstrap_checks_real_tls_sni_and_origin() {
    let (acceptor, connector) = endpoints();
    let runtime = RuntimeBuilder::current_thread()
        .enable_platform_reactor(true)
        .build()
        .unwrap();
    runtime.block_on(async {
        let ingress = test_ingress(4096, 4);
        for (name, origin, expected) in [
            (EXPECTED_HOST, EXPECTED_ORIGIN, 200),
            (OTHER_SNI, EXPECTED_ORIGIN, 403),
            (EXPECTED_HOST, "https://attacker.example", 403),
        ] {
            let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
                .await
                .unwrap();
            let addr = listener.local_addr().unwrap();
            let ((), ()) = futures_join(
                async {
                    let tcp = TcpStream::connect(addr).await.unwrap();
                    let mut stream = connector.connect(name, tcp).await.unwrap();
                    let request = bootstrap_request("observer").replace(EXPECTED_ORIGIN, origin);
                    stream.write_all(request.as_bytes()).await.unwrap();
                    stream.flush().await.unwrap();
                    let head = read_http_head_from_stream(&mut stream).await;
                    assert!(head.starts_with(&format!("HTTP/1.1 {expected} ")));
                    let length: usize = head
                        .lines()
                        .find_map(|line| line.strip_prefix("Content-Length: "))
                        .unwrap()
                        .parse()
                        .unwrap();
                    let mut body = vec![0; length];
                    stream.read_exact(&mut body).await.unwrap();
                    assert!(
                        !body
                            .windows(b"nonce_hex".len())
                            .any(|bytes| bytes == b"nonce_hex")
                    );
                    if expected == 200 {
                        let mut adapter = BrowserConnectionAdapter::new();
                        let attempt = adapter.begin("observer", 1_000_000).unwrap();
                        assert_eq!(
                            adapter.accept_bootstrap(
                                attempt,
                                std::str::from_utf8(&body).unwrap(),
                                1_001_000
                            ),
                            Err(BrowserAdapterError::DesktopUnavailable)
                        );
                    }
                },
                async {
                    let (server, _) = listener.accept().await.unwrap();
                    ingress
                        .serve_tls_connection(&acceptor, server, PEER_IP, || {
                            HostInstant::from_micros(10_000_000)
                        })
                        .await
                        .unwrap();
                },
            )
            .await;
        }
    });
}

#[test]
fn canonical_browser_refusal_traverses_real_wss() {
    let (acceptor, connector) = endpoints();
    let runtime = RuntimeBuilder::current_thread()
        .enable_platform_reactor(true)
        .build()
        .unwrap();
    runtime.block_on(async {
        let ingress = test_ingress(4096, 4);
        let (mut adapter, attempt, hello) = client_fixture(&ingress, "observer");
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let ((), ()) = futures_join(
            async {
                let tcp = TcpStream::connect(addr).await.unwrap();
                let mut stream = connector.connect(EXPECTED_HOST, tcp).await.unwrap();
                stream
                    .write_all(upgrade_request("/ws/control").as_bytes())
                    .await
                    .unwrap();
                stream.flush().await.unwrap();
                assert!(
                    read_http_head_from_stream(&mut stream)
                        .await
                        .starts_with("HTTP/1.1 101 ")
                );
                write_ws_frame(&mut stream, 0x02, &hello, Some([1, 2, 3, 4]))
                    .await
                    .unwrap();
                let response = read_ws_frame(&mut stream, browser::MAX_RECORD, false)
                    .await
                    .unwrap();
                assert_eq!(refused(&response), Reason::HostUnavailable);
                let WsFrame::Binary(bytes) = response else {
                    unreachable!()
                };
                assert_eq!(
                    adapter.receive_control(attempt, &bytes, 1_003_000),
                    Ok("host_unavailable")
                );
                assert_eq!(
                    read_ws_frame(&mut stream, 128, false).await.unwrap(),
                    WsFrame::Close
                );
            },
            async {
                let (server, _) = listener.accept().await.unwrap();
                ingress
                    .serve_tls_connection(&acceptor, server, PEER_IP, || {
                        HostInstant::from_micros(10_001_000)
                    })
                    .await
                    .unwrap();
            },
        )
        .await;
        assert!(
            ingress
                .session_metrics(RemoteSessionId::from_raw(101))
                .unwrap()
                .is_none()
        );
    });
}

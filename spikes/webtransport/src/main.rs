mod h3;
mod pki;
mod proxy;
mod server;
mod wss;

use asupersync::cx::Cx;
use std::io::Write;
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let command = args.get(1).map(|s| s.as_str()).unwrap_or("help");

    match command {
        "pki" => {
            let pki = pki::WebTransportPki::generate()?;
            let out = serde_json::json!({
                "cert_hash_hex": pki.cert_hash_hex,
                "cert_hash_bytes": pki.cert_hash,
            });
            println!("{}", out);
        }
        "serve-wt" => {
            let expected_origin = args.get(2).map(|s| s.as_str());
            let timeout_secs = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(30);

            let pki = pki::WebTransportPki::generate()?;
            let cert_hash_hex = pki.cert_hash_hex.clone();
            let cert_hash_bytes = pki.cert_hash;
            let origin_clone = expected_origin.map(|s| s.to_string());

            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            let (report_tx, report_rx) = std::sync::mpsc::channel();

            let handle = std::thread::spawn(move || {
                let runtime = asupersync::runtime::RuntimeBuilder::new()
                    .build()
                    .expect("build runtime");
                runtime.block_on(async move {
                    let cx = Cx::for_testing();
                    match server::serve_one_session(
                        &cx,
                        &pki,
                        origin_clone.as_deref(),
                        Duration::from_secs(timeout_secs),
                        Some(ready_tx),
                    )
                    .await
                    {
                        Ok((report, client_addr)) => {
                            let _ = report_tx.send(Ok((report, client_addr)));
                        }
                        Err(e) => {
                            let _ = report_tx.send(Err(e));
                        }
                    }
                });
            });

            // Receive ready signal with server port
            let client_facing_addr = match ready_rx.recv_timeout(Duration::from_secs(5)) {
                Ok(addr) => addr,
                Err(e) => {
                    eprintln!("WT_SERVER_ERROR: failed to get listening address: {e}");
                    std::process::exit(1);
                }
            };

            let ready_info = serde_json::json!({
                "port": client_facing_addr.port(),
                "cert_hash_hex": cert_hash_hex,
                "cert_hash_bytes": cert_hash_bytes,
            });
            println!("WT_READY:{}", ready_info);
            let _ = std::io::stdout().flush();

            // Wait for report
            if let Ok(result) = report_rx.recv_timeout(Duration::from_secs(timeout_secs + 5)) {
                match result {
                    Ok((report, _addr)) => {
                        println!("SESSION_REPORT:{}", serde_json::to_string(&report)?);
                    }
                    Err(e) => {
                        eprintln!("SESSION_ERROR:{}", e);
                        std::process::exit(1);
                    }
                }
            } else {
                eprintln!("SESSION_TIMEOUT");
                std::process::exit(1);
            }
            let _ = handle.join();
        }
        "serve-wss" => {
            let timeout_secs = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(15);
            let server = wss::WssServer::spawn()?;
            let ready_json = serde_json::json!({
                "status": "ready",
                "port": server.addr.port(),
            });
            println!("WSS_READY:{}", ready_json);
            let _ = std::io::stdout().flush();

            // Wait for client to complete scenario
            std::thread::sleep(Duration::from_secs(timeout_secs));
            let report = server.report.lock().unwrap().clone();
            println!("WSS_REPORT:{}", serde_json::to_string(&report)?);
        }
        _ => {
            println!(
                "Usage: webtransport-spike [pki | serve-wt <expected_origin> <timeout_s> | serve-wss <timeout_s>]"
            );
        }
    }

    Ok(())
}

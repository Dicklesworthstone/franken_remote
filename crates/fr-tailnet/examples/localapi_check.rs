#![forbid(unsafe_code)]
//! Read-only qualification of the installed Linux `LocalAPI` adapter.
//! Supplied addresses are lookup inputs, not evidence of transport ingress.

#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    eprintln!("localapi_check requires the Linux installed-socket profile");
    std::process::ExitCode::from(2)
}

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    use asupersync::{cx::Cx, runtime::RuntimeBuilder};
    use fr_tailnet::{ConnectionAddresses, GrantPolicy, LocalApi, Scope};
    use serde_json::json;
    use std::{process::ExitCode, time::Instant};

    let mut args = std::env::args_os().skip(1);
    let endpoints = args.next().zip(args.next()).and_then(|(local, peer)| {
        Some(ConnectionAddresses {
            local: local.to_str()?.parse().ok()?,
            peer: peer.to_str()?.parse().ok()?,
        })
    });
    let scope = match args.next() {
        None => Some(Scope::OwnUser),
        Some(value) => match value.to_str() {
            Some("own-user") => Some(Scope::OwnUser),
            Some("tailnet") => Some(Scope::Tailnet),
            _ => None,
        },
    };
    let (Some(endpoints), Some(scope), None) = (endpoints, scope, args.next()) else {
        eprintln!("usage: localapi_check LOCAL_IP:PORT PEER_IP:PORT [own-user|tailnet]");
        return ExitCode::from(1);
    };
    let Ok(runtime) = RuntimeBuilder::new()
        .worker_threads(2)
        .enable_platform_reactor(true)
        .build()
    else {
        eprintln!("localapi_check: runtime unavailable");
        return ExitCode::from(1);
    };
    let started = Instant::now();
    let result = runtime.block_on(async {
        let cx = Cx::current().ok_or(fr_tailnet::Error::MissingRuntime)?;
        LocalApi::installed()
            .authorize_app_capability(
                &cx,
                endpoints,
                GrantPolicy {
                    scope,
                    ..Default::default()
                },
            )
            .await
    });
    let mut row = json!({
        "schema_version": 1,
        "profile": "linux-installed-app-capability",
        "transport_ingress_qualified": false,
        "scope": match scope { Scope::OwnUser => "own-user", Scope::Tailnet => "tailnet" },
        "elapsed_us": started.elapsed().as_micros(),
    });
    let exit = match result {
        Ok(proof) => {
            row["result"] = json!("authorized_metadata_only");
            row["observe"] = json!(proof.permissions().observe());
            row["control"] = json!(proof.permissions().control());
            ExitCode::SUCCESS
        }
        Err(reason) => {
            row["result"] = json!("refused");
            row["reason"] = json!(reason.to_string());
            ExitCode::from(2)
        }
    };
    // The proof and all metadata stay private. The process opens no listener,
    // grants no session, and prints only fixed labels, flags and elapsed time.
    println!("{row}");
    exit
}

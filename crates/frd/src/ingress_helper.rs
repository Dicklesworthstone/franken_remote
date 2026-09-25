//! `frd ingress-helper`: the root half of the Linux ingress split (plan 5.2).
//! It owns the project's nftables drop rule for an unprivileged `frd run`,
//! exposes only install/renew/remove over a `SO_PEERCRED`-checked socket, and
//! reads its interface and admitted uids from a root-owned file. Everything it
//! enforces lives in `fr_tailnet::ingress::helper`; this is the process role.
use asupersync::{
    runtime::RuntimeBuilder,
    signal::{SignalKind, signal},
    types::Budget,
};
use fr_tailnet::ingress::helper::{self, Event, Settings, SettingsError};
use std::{future::poll_fn, path::PathBuf, pin::pin, process::ExitCode, task::Poll};

fn refusal(json: bool, code: &str, detail: &str) -> ExitCode {
    if json {
        eprintln!(
            "{}",
            serde_json::json!({"outcome": "refusal", "code": code, "detail": detail})
        );
    } else {
        eprintln!("frd ingress-helper: refusal ({code}): {detail}");
    }
    ExitCode::from(1)
}

/// Uids, generations and table names only: never addresses or command output.
fn print(json: bool, event: &Event) {
    if json {
        let value = match event {
            Event::Reclaimed { tables } => {
                serde_json::json!({"event": "reclaimed", "tables": tables})
            }
            Event::Listening => serde_json::json!({"event": "listening"}),
            Event::Refused { uid, reason } => {
                serde_json::json!({"event": "refused", "uid": uid, "reason": reason.as_str()})
            }
            Event::Suppressed { count } => {
                serde_json::json!({"event": "suppressed", "count": count})
            }
            Event::Installed {
                uid,
                generation,
                table,
            } => {
                serde_json::json!({"event": "installed", "uid": uid, "generation": generation, "table": table})
            }
            Event::Renewed { generation, table } => {
                serde_json::json!({"event": "renewed", "generation": generation, "table": table})
            }
            Event::Removed {
                generation,
                table,
                disconnect,
            } => serde_json::json!({"event": "removed", "generation": generation, "table": table,
                "cause": if *disconnect { "disconnect" } else { "request" }}),
            Event::CleanupFailed { table } => {
                serde_json::json!({"event": "cleanup_failed", "table": table})
            }
            Event::Stopping => serde_json::json!({"event": "stopping"}),
        };
        println!("{value}");
        return;
    }
    match event {
        Event::Reclaimed { tables } => {
            println!("frd ingress-helper: reclaimed {tables} stale helper table(s)");
        }
        Event::Listening => println!("frd ingress-helper: listening"),
        Event::Refused { uid, reason } => {
            println!("frd ingress-helper: refused uid {uid}: {}", reason.as_str());
        }
        Event::Suppressed { count } => {
            println!("frd ingress-helper: {count} refusal line(s) suppressed");
        }
        Event::Installed {
            uid,
            generation,
            table,
        } => {
            println!("frd ingress-helper: installed {table} generation {generation} for uid {uid}");
        }
        Event::Renewed { generation, table } => {
            println!("frd ingress-helper: renewed {table} generation {generation}");
        }
        Event::Removed {
            generation,
            table,
            disconnect,
        } => println!(
            "frd ingress-helper: removed {table} generation {generation} ({})",
            if *disconnect {
                "connection closed"
            } else {
                "requested"
            }
        ),
        Event::CleanupFailed { table } => {
            println!("frd ingress-helper: cleanup failed for {table}; reclaimed at next start");
        }
        Event::Stopping => println!("frd ingress-helper: stopping"),
    }
}

/// `frd ingress-helper [--config PATH] [--json]`; nothing else is accepted.
pub fn execute(args: &[String], json: bool) -> ExitCode {
    let mut config = PathBuf::from(helper::DEFAULT_CONFIG);
    let mut seen = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--json" => {}
            "--config" if !seen => match iter.next() {
                Some(path) if path.starts_with('/') => {
                    config = PathBuf::from(path);
                    seen = true;
                }
                _ => return refusal(json, "invalid_arguments", "--config needs an absolute path"),
            },
            _ => {
                return refusal(
                    json,
                    "invalid_arguments",
                    "usage: frd ingress-helper [--config /absolute/path.json] [--json]",
                );
            }
        }
    }
    let settings = match Settings::load(&config) {
        Ok(settings) => settings,
        Err(error) => {
            let hint = match error {
                SettingsError::Unprotected => {
                    "the file and every parent directory must be root-owned and not group- or other-writable"
                }
                SettingsError::UntrustedExecutable => {
                    "nft and ip must be root-owned executables in root-only directories"
                }
                _ => {
                    "expected {\"interface\":\"tailscale0\",\"allowed_uids\":[UID]} with optional socket, nft and ip"
                }
            };
            return refusal(
                json,
                "ingress_helper_configuration",
                &format!("{error:?}: {hint}"),
            );
        }
    };
    let Ok(runtime) = RuntimeBuilder::new()
        .worker_threads(1)
        .enable_platform_reactor(true)
        .build()
    else {
        return refusal(json, "runtime_unavailable", "could not start the runtime");
    };
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    let report = move |event: Event| print(json, &event);
    let result = runtime.block_on(async {
        let mut signals = (
            signal(SignalKind::interrupt()).ok(),
            signal(SignalKind::terminate()).ok(),
        );
        let shutdown = poll_fn(move |task| {
            for s in [&mut signals.0, &mut signals.1].into_iter().flatten() {
                if pin!(s.recv()).poll(task).is_ready() {
                    return Poll::Ready(());
                }
            }
            Poll::Pending
        });
        helper::serve(&cx, &settings, shutdown, &report).await
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => refusal(json, error.code(), &format!("{error:?}")),
    }
}

#[cfg(test)]
mod tests {
    use crate::{host_run::Error, native_connection::host::LinuxError};
    use fr_tailnet::ingress::{Error as Ingress, helper::Refusal};

    /// `frd run` names a missing helper and an unconfirmed rule distinctly,
    /// whether the failure surfaced at configuration, bind or renewal.
    #[test]
    fn ingress_refusals_keep_their_own_codes_through_the_listener() {
        for (error, code) in [
            (Ingress::HelperUnavailable, "ingress_helper_unavailable"),
            (
                Ingress::HelperRefused(Refusal::AddressNotAssigned),
                "ingress_unenforced",
            ),
            (Ingress::FirewallMismatch, "ingress_unenforced"),
            (Ingress::CommandFailed, "ingress_unenforced"),
        ] {
            assert_eq!(Error::Ingress(error).code(), code);
            assert_eq!(
                Error::Listener(Box::new(LinuxError::Ingress(error))).code(),
                code
            );
        }
        assert_eq!(
            Error::Listener(Box::new(LinuxError::Spent)).code(),
            "listener_failed"
        );
        let detail = Error::Listener(Box::new(LinuxError::Ingress(Ingress::HelperRefused(
            Refusal::PeerNotAllowed,
        ))))
        .to_string();
        assert!(detail.contains("HelperRefused(PeerNotAllowed)"), "{detail}");
    }

    #[test]
    fn only_the_config_flag_is_accepted() {
        for args in [
            vec!["--socket".to_owned(), "/tmp/x".to_owned()],
            vec!["--config".to_owned()],
            vec!["--config".to_owned(), "relative.json".to_owned()],
            vec![
                "--config".to_owned(),
                "/a.json".to_owned(),
                "--config".to_owned(),
                "/b.json".to_owned(),
            ],
        ] {
            assert_eq!(super::execute(&args, true), std::process::ExitCode::from(1));
        }
    }
}

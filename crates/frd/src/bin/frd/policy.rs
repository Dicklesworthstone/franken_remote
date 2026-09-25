//! Local policy publication. A save does not acknowledge any daemon's application.
use frd::host_policy::{Error, Saved, Store, options::PolicyCommand};
use std::process::ExitCode;

pub fn refusal(error: Error, json: bool) -> ExitCode {
    if json {
        println!(
            "{}",
            serde_json::json!({
                "schema_version": "fr.policy.v1", "outcome": "refusal",
                "code": "invalid_host_policy", "reason": error.to_string(), "updated": false,
            })
        );
    } else {
        eprintln!("Host policy refused: {error}. No policy change was applied.");
    }
    ExitCode::from(2)
}
pub fn execute(args: &[String], approval: bool, json: bool) -> ExitCode {
    let result = (|| {
        let command = PolicyCommand::parse(args, approval)?;
        let store = Store::new(&command.path)?;
        if let Some(change) = command.change {
            store.update(change).map(|saved| (saved, true))
        } else {
            store.load().map(|policy| {
                (
                    Saved {
                        policy,
                        changed: false,
                        durable: true,
                    },
                    false,
                )
            })
        }
    })();
    let (saved, writing) = match result {
        Ok(result) => result,
        Err(error) => return refusal(error, json),
    };
    let mode = saved.policy.approval_mode.as_str();
    let scope = saved.policy.sharing_scope.as_str();
    if json {
        println!(
            "{}",
            serde_json::json!({
                "schema_version": "fr.policy.v1",
                "outcome": if saved.durable { "success" } else { "durability_unconfirmed" },
                "approval_mode": mode, "sharing_scope": scope,
                "revision": saved.policy.revision,
                "policy": if saved.policy.revision == 0 { "plan_defaults" } else { "local_file" },
                "updated": saved.changed, "durable": if writing { Some(saved.durable) } else { None },
                "applies_to": "matching_live_daemon_or_next_start",
                "applied_to_running_daemon": serde_json::Value::Null,
                "restart_required": serde_json::Value::Null,
                "live_application": "unconfirmed", "explicit_startup_flags_override": true,
            })
        );
    } else {
        let (label, value) = if approval {
            ("Approval Mode", mode)
        } else {
            ("Sharing Scope", scope)
        };
        if writing {
            println!(
                "{label} updated to '{value}' (saved revision {}).",
                saved.policy.revision
            );
        } else {
            println!(
                "{label}: {value} (saved revision {}).",
                saved.policy.revision
            );
        }
        println!(
            "frd run watches this policy path; observed revisions end old shares before new admission.\nExplicit startup flags override values, but never bypass revision fencing."
        );
        println!("This command confirms the saved policy only; live application is unconfirmed.");
        if !saved.durable {
            eprintln!("Policy was published, but crash durability could not be confirmed.");
        }
    }
    if saved.durable {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

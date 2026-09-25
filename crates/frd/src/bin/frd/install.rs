//! Local installation command: preview and persistence are not service activation.
use frd::service_install::{self, InstallOptions};
use std::process::ExitCode;

pub fn execute(args: &[String], json: bool) -> ExitCode {
    let options = match InstallOptions::parse_cli(args) {
        Ok(options) => options,
        Err(error) => {
            let code = match &error {
                frd::service_install::ServiceError::HostProfileUnavailable { code, .. } => code,
                _ => "invalid_service_options",
            };
            if json {
                println!(
                    "{}",
                    serde_json::json!({"outcome": "refusal", "code": code})
                );
            } else {
                eprintln!("Service installation refused: {error}");
            }
            return ExitCode::from(2);
        }
    };

    match service_install::install(&options) {
        Ok(report) => {
            // Rendered, never written: these need root and are not activation.
            let helper = report.ingress_helper.as_ref().map(|helper| {
                serde_json::json!({
                    "requires_root": true, "written": false,
                    "unit_path": helper.unit_path, "unit_content": helper.unit_content,
                    "config_path": helper.config_path, "config_content": helper.config_content,
                    "exec_path": helper.exec_path, "copy_from": helper.copy_from,
                })
            });
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "schema_version": "fr.service.v1", "outcome": "success",
                        "kind": report.kind.as_str(), "unit_path": report.unit_path,
                        "dry_run": report.dry_run, "started": false,
                        "unit_content": report.dry_run.then_some(&report.unit_content),
                        "next_steps": report.next_steps,
                        "ingress_helper": helper,
                    })
                );
            } else {
                println!(
                    "Service installation {} for {}:",
                    if report.dry_run {
                        "preview"
                    } else {
                        "succeeded"
                    },
                    report.kind.as_str()
                );
                println!("  Unit file: {}", report.unit_path.display());
                for step in &report.next_steps {
                    println!("  {step}");
                }
                if report.dry_run {
                    println!("\n{}", report.unit_content);
                }
                if let Some(helper) = &report.ingress_helper {
                    println!(
                        "\n[root] Helper configuration for {} (not written by this install):\n{}",
                        helper.config_path.display(),
                        helper.config_content
                    );
                    println!(
                        "[root] Helper unit for {} (not written by this install):\n{}",
                        helper.unit_path.display(),
                        helper.unit_content
                    );
                }
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({"outcome": "failure", "error": e.to_string()})
                );
            } else {
                eprintln!("Error installing service: {e}");
            }
            ExitCode::from(1)
        }
    }
}

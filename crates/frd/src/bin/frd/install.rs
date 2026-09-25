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
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "schema_version": "fr.service.v1", "outcome": "success",
                        "kind": report.kind.as_str(), "unit_path": report.unit_path,
                        "dry_run": report.dry_run, "started": false,
                        "unit_content": report.dry_run.then_some(&report.unit_content),
                        "next_steps": report.next_steps,
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

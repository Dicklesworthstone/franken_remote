//! Artifact capture and reproducibility bundle for end-to-end sessions.
//!
//! Stores run manifests, chronological structured event logs (JSONL), process stderr,
//! summary results, and a single-command reproduction script.

use crate::assertions::AssertionResult;
use crate::event::StructuredLogEvent;
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

/// Metadata manifest for a single scenario execution run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunManifest {
    pub run_id: String,
    pub scenario_name: String,
    pub seed: u64,
    pub git_commit: String,
    pub timestamp_utc: String,
    pub target_os: String,
    pub target_arch: String,
    pub reproduce_command: String,
}

/// Final summary of scenario assertions and execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSummary {
    pub run_id: String,
    pub scenario_name: String,
    pub passed: bool,
    pub duration_ms: u64,
    pub event_count: usize,
    pub assertions: Vec<AssertionResult>,
    pub failure_reason: Option<String>,
}

/// Manages artifact collection and persistence in a per-run directory.
#[derive(Debug)]
pub struct ArtifactBundle {
    pub run_id: String,
    pub dir: PathBuf,
    manifest: RunManifest,
    events: Vec<StructuredLogEvent>,
    host_stderr: Vec<u8>,
    client_stderr: Vec<u8>,
}

impl ArtifactBundle {
    /// Initialize a new artifact bundle for a run.
    pub fn new(base_dir: &Path, scenario_name: &str, seed: u64) -> io::Result<Self> {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let run_id = format!("run_{scenario_name}_{timestamp}_{seed}");
        let dir = base_dir.join(&run_id);
        fs::create_dir_all(&dir)?;

        let git_commit = option_env!("GIT_COMMIT_HASH")
            .unwrap_or("development_tree")
            .to_string();

        let reproduce_command = format!(
            "scripts/e2e/run.sh --scenario {} --seed {}",
            scenario_name, seed
        );

        let manifest = RunManifest {
            run_id: run_id.clone(),
            scenario_name: scenario_name.to_string(),
            seed,
            git_commit,
            timestamp_utc: timestamp.to_string(),
            target_os: std::env::consts::OS.to_string(),
            target_arch: std::env::consts::ARCH.to_string(),
            reproduce_command,
        };

        Ok(Self {
            run_id,
            dir,
            manifest,
            events: Vec::new(),
            host_stderr: Vec::new(),
            client_stderr: Vec::new(),
        })
    }

    /// Record a structured log event.
    pub fn record_event(&mut self, event: StructuredLogEvent) {
        self.events.push(event);
    }

    /// Record a batch of structured log events.
    pub fn record_events(&mut self, events: impl IntoIterator<Item = StructuredLogEvent>) {
        self.events.extend(events);
    }

    /// Append to captured host stderr.
    pub fn append_host_stderr(&mut self, bytes: &[u8]) {
        self.host_stderr.extend_from_slice(bytes);
    }

    /// Append to captured client stderr.
    pub fn append_client_stderr(&mut self, bytes: &[u8]) {
        self.client_stderr.extend_from_slice(bytes);
    }

    /// Access recorded events for assertion evaluation.
    #[must_use]
    pub fn events(&self) -> &[StructuredLogEvent] {
        &self.events
    }

    /// Finalize and flush all artifacts to disk: manifest, events.jsonl, logs, summary, reproduce.sh.
    pub fn flush_to_disk(
        &self,
        assertions: Vec<AssertionResult>,
        duration_ms: u64,
    ) -> io::Result<RunSummary> {
        let all_passed = assertions.iter().all(|a| a.passed);
        let failure_reason = assertions
            .iter()
            .find(|a| !a.passed)
            .map(|a| format!("{}: {}", a.assertion_name, a.message));

        let summary = RunSummary {
            run_id: self.run_id.clone(),
            scenario_name: self.manifest.scenario_name.clone(),
            passed: all_passed,
            duration_ms,
            event_count: self.events.len(),
            assertions,
            failure_reason,
        };

        // 1. Write manifest.json
        let manifest_path = self.dir.join("manifest.json");
        let manifest_file = File::create(manifest_path)?;
        serde_json::to_writer_pretty(manifest_file, &self.manifest)?;

        // 2. Write events.jsonl
        let events_path = self.dir.join("events.jsonl");
        let events_file = File::create(events_path)?;
        let mut events_writer = BufWriter::new(events_file);
        for event in &self.events {
            writeln!(events_writer, "{}", event.to_json_line()?)?;
        }
        events_writer.flush()?;

        // 3. Write summary.json
        let summary_path = self.dir.join("summary.json");
        let summary_file = File::create(summary_path)?;
        serde_json::to_writer_pretty(summary_file, &summary)?;

        // 4. Write host_stderr.log and client_stderr.log
        let host_log_path = self.dir.join("host_stderr.log");
        fs::write(host_log_path, &self.host_stderr)?;

        let client_log_path = self.dir.join("client_stderr.log");
        fs::write(client_log_path, &self.client_stderr)?;

        // 5. Write reproduce.sh
        let reproduce_path = self.dir.join("reproduce.sh");
        let script = format!(
            "#!/usr/bin/env bash\n# FrankenRemote E2E Reproduction Script\n# Run ID: {}\n# Scenario: {}\n# Seed: {}\nset -euo pipefail\necho \"Reproducing run {}...\"\n{}\n",
            self.run_id,
            self.manifest.scenario_name,
            self.manifest.seed,
            self.run_id,
            self.manifest.reproduce_command
        );
        fs::write(&reproduce_path, script)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&reproduce_path)?.permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&reproduce_path, perms)?;
        }

        Ok(summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{AuthorityState, EventKind, EventSource};

    #[test]
    fn artifact_bundle_creation_and_flush() {
        let temp_dir = std::env::temp_dir().join(format!("fr_test_artifacts_{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp_dir);

        let mut bundle = ArtifactBundle::new(&temp_dir, "test_scenario", 42).unwrap();
        bundle.record_event(StructuredLogEvent::new(
            1_000_000,
            EventSource::Host,
            EventKind::AuthorityTransition {
                from: AuthorityState::Idle,
                to: AuthorityState::Observing,
                generation: 1,
                reason: "test".to_string(),
            },
        ));
        bundle.append_host_stderr(b"host log line 1\n");
        bundle.append_client_stderr(b"client log line 1\n");

        let assertions = vec![AssertionResult::pass("test_assertion".to_string(), "ok".to_string())];
        let summary = bundle.flush_to_disk(assertions, 150).unwrap();

        assert!(summary.passed);
        assert_eq!(summary.event_count, 1);
        assert!(bundle.dir.join("manifest.json").exists());
        assert!(bundle.dir.join("events.jsonl").exists());
        assert!(bundle.dir.join("summary.json").exists());
        assert!(bundle.dir.join("host_stderr.log").exists());
        assert!(bundle.dir.join("client_stderr.log").exists());
        assert!(bundle.dir.join("reproduce.sh").exists());

        // Clean up
        let _ = fs::remove_dir_all(&temp_dir);
    }
}

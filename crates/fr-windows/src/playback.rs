//! Local playback preferences and bounded original-owner completion history.
//! No samples, native handles, or session permission are stored here.
#![forbid(unsafe_code)]
use crate::options::AudioOptions;
use fr_client::audio::AudioVolumeControl;
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Counts {
    pub submitted: u64,
    pub dropped_packets: u64,
    pub resets: u64,
    pub expired_generations: u64,
    pub failed_generations: u64,
    pub recovered_generations: u64,
}
impl Counts {
    fn add(self, other: Self) -> Self {
        Self {
            submitted: self.submitted.saturating_add(other.submitted),
            dropped_packets: self.dropped_packets.saturating_add(other.dropped_packets),
            resets: self.resets.saturating_add(other.resets),
            expired_generations: self
                .expired_generations
                .saturating_add(other.expired_generations),
            failed_generations: self
                .failed_generations
                .saturating_add(other.failed_generations),
            recovered_generations: self
                .recovered_generations
                .saturating_add(other.recovered_generations),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
#[allow(clippy::struct_excessive_bools, reason = "independent native receipts")]
pub(crate) struct Snapshot {
    pub counts: Counts,
    pub acknowledged: bool,
    pub active: bool,
    pub complete: bool,
    pub absence: Option<&'static str>,
    pub first_failure: Option<&'static str>,
}

pub(crate) struct State {
    preferences: AudioOptions,
    owners: u64,
    current: Option<(AudioVolumeControl, Snapshot)>,
    retired: Counts,
    acknowledged: bool,
    first_failure: Option<&'static str>,
    refused_replacement: bool,
}
impl State {
    pub fn new(preferences: AudioOptions) -> Self {
        Self {
            preferences,
            owners: 0,
            current: None,
            retired: Counts::default(),
            acknowledged: false,
            first_failure: None,
            refused_replacement: false,
        }
    }

    /// Every reconnect gets a new preference handle. The old one has no power
    /// over its replacement, and native cleanup must be positively complete.
    pub fn begin(&mut self) -> Option<(u64, AudioVolumeControl)> {
        if !self.complete() {
            self.refused_replacement = true;
            return None;
        }
        let Some(owner) = self.owners.checked_add(1) else {
            self.refused_replacement = true;
            return None;
        };
        if let Some((_, previous)) = self.current.take() {
            self.retired = self.retired.add(previous.counts);
            self.acknowledged |= previous.acknowledged;
            if self.first_failure.is_none() {
                self.first_failure = previous.first_failure;
            }
        }
        let control = AudioVolumeControl::from_parts(
            f32::from(self.preferences.volume) / 100.0,
            self.preferences.muted,
        );
        self.owners = owner;
        self.current = Some((control.clone(), Snapshot::default()));
        Some((owner, control))
    }

    /// Late callbacks from a retired native owner cannot change current state.
    pub fn update(&mut self, owner: u64, snapshot: Snapshot) {
        if owner == self.owners
            && let Some((_, current)) = &mut self.current
        {
            self.acknowledged |= snapshot.acknowledged;
            if self.first_failure.is_none() {
                self.first_failure = snapshot.first_failure;
            }
            *current = snapshot;
        }
    }

    pub fn complete(&self) -> bool {
        !self.refused_replacement && self.current.as_ref().is_none_or(|(_, s)| s.complete)
    }
    pub fn preferences(&self) -> AudioOptions {
        self.preferences
    }
    pub fn set_preferences(&mut self, preferences: AudioOptions) {
        self.preferences = AudioOptions {
            volume: preferences.volume.min(100),
            ..preferences
        };
        if let Some((control, _)) = &mut self.current {
            control.set_volume(f32::from(self.preferences.volume) / 100.0);
            control.set_muted(self.preferences.muted);
        }
    }

    pub fn document(&self) -> Value {
        let current = self
            .current
            .as_ref()
            .map_or(Snapshot::default(), |(_, s)| *s);
        let counts = self.retired.add(current.counts);
        let cleanup = if self.refused_replacement {
            "unconfirmed"
        } else if self.current.is_none() {
            "not_started"
        } else if current.complete {
            "finished"
        } else {
            "pending"
        };
        json!({
            "requested": true,
            "owners": self.owners,
            "configured": self.acknowledged || current.acknowledged,
            "active": current.active && !current.complete,
            "volume_percent": self.preferences.volume,
            "muted": self.preferences.muted,
            "submitted": counts.submitted,
            "dropped_packets": counts.dropped_packets,
            "resets": counts.resets,
            "expired_generations": counts.expired_generations,
            "failed_generations": counts.failed_generations,
            "recovered_generations": counts.recovered_generations,
            "first_failure": self.first_failure.or(current.first_failure),
            "absence": current.absence,
            "cleanup": cleanup,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacement_requires_actual_cleanup_and_failure_stays_sticky() {
        let mut state = State::new(AudioOptions::default());
        let (owner, _) = state.begin().unwrap();
        assert!(!state.complete());
        assert!(state.begin().is_none());
        state.update(
            owner,
            Snapshot {
                complete: true,
                ..Snapshot::default()
            },
        );
        assert!(!state.complete());
        assert_eq!(state.document()["cleanup"], "unconfirmed");
    }

    #[test]
    fn counts_survive_reconnect_and_old_callback_cannot_overwrite_new_owner() {
        let mut state = State::new(AudioOptions::default());
        let (first, _) = state.begin().unwrap();
        let previous = Snapshot {
            counts: Counts {
                submitted: 120,
                failed_generations: 1,
                ..Counts::default()
            },
            acknowledged: true,
            complete: true,
            first_failure: Some("device_lost"),
            ..Snapshot::default()
        };
        state.update(first, previous);
        let (second, _) = state.begin().unwrap();
        state.update(first, Snapshot::default());
        state.update(
            second,
            Snapshot {
                counts: Counts {
                    submitted: 80,
                    ..Counts::default()
                },
                complete: true,
                ..Snapshot::default()
            },
        );
        let report = state.document();
        assert_eq!(report["submitted"], 200);
        assert_eq!(report["owners"], 2);
        assert_eq!(report["failed_generations"], 1);
        assert_eq!(report["first_failure"], "device_lost");
        assert_eq!(report["configured"], true);
        assert!(state.complete());
    }

    #[test]
    fn preferences_follow_reconnect_but_old_handles_do_not() {
        let mut state = State::new(AudioOptions {
            volume: 75,
            muted: false,
        });
        let (first, mut old) = state.begin().unwrap();
        state.set_preferences(AudioOptions {
            volume: 40,
            muted: true,
        });
        assert_eq!(old.snapshot(), (0.4, true));
        state.update(
            first,
            Snapshot {
                complete: true,
                ..Snapshot::default()
            },
        );
        let (_, current) = state.begin().unwrap();
        old.set_volume(1.0);
        old.set_muted(false);
        assert_eq!(current.snapshot(), (0.4, true));
        state.set_preferences(AudioOptions {
            volume: 60,
            muted: false,
        });
        assert_eq!(current.snapshot(), (0.6, false));
        assert_eq!(old.snapshot(), (1.0, false));
    }

    #[test]
    fn completed_or_failed_generation_cannot_erase_successful_configuration() {
        let mut state = State::new(AudioOptions::default());
        let (owner, _) = state.begin().unwrap();
        state.update(
            owner,
            Snapshot {
                acknowledged: true,
                active: true,
                counts: Counts {
                    submitted: 960,
                    ..Counts::default()
                },
                ..Snapshot::default()
            },
        );
        state.update(
            owner,
            Snapshot {
                complete: true,
                absence: Some("device_lost"),
                first_failure: Some("device_lost"),
                counts: Counts {
                    submitted: 960,
                    failed_generations: 1,
                    ..Counts::default()
                },
                ..Snapshot::default()
            },
        );
        assert_eq!(state.document()["configured"], true);
        assert_eq!(state.document()["active"], false);
        assert_eq!(state.document()["submitted"], 960);
        assert_eq!(state.document()["first_failure"], "device_lost");
    }

    #[test]
    fn request_without_an_admitted_session_has_no_native_cleanup_debt() {
        let state = State::new(AudioOptions::default());
        assert!(state.complete());
        assert_eq!(state.document()["cleanup"], "not_started");
        assert_eq!(state.document()["configured"], false);
    }
}

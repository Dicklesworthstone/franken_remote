//! Optional host playback, local volume menu, and original native cleanup.
use crate::{
    options::AudioOptions,
    playback::{Counts, Snapshot, State},
};
use fr_client::audio::output::Unavailable as ViewerAudioUnavailable;
use fr_mobile_ffi::session::{Config, Playback, PlaybackFactory, Session};
use fr_native::audio_output::{Output, Report, Retirement};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::{Arc, Mutex},
};
use windows::{
    Win32::{
        Foundation::HWND,
        UI::WindowsAndMessaging::{
            AppendMenuW, CreatePopupMenu, DestroyMenu, DrawMenuBar, GetMenu, HMENU, MF_BYCOMMAND,
            MF_POPUP, MF_STRING, ModifyMenuW,
        },
    },
    core::{HSTRING, Result, w},
};

const MUTE: usize = 0x46a0;
const LOWER: usize = 0x46a1;
const HIGHER: usize = 0x46a2;

pub(super) struct Audio {
    state: Arc<Mutex<State>>,
    menu: HMENU,
}
impl Audio {
    pub fn install(hwnd: HWND, preferences: AudioOptions) -> Result<Self> {
        // SAFETY: all menu handles belong to this UI-thread window. The popup
        // remains ours until AppendMenuW transfers it to the existing menu bar.
        let menu = unsafe {
            let menu = CreatePopupMenu()?;
            let result = (|| {
                AppendMenuW(menu, MF_STRING, MUTE, w!("Mute host audio"))?;
                AppendMenuW(menu, MF_STRING, LOWER, w!("Volume down 10%"))?;
                AppendMenuW(menu, MF_STRING, HIGHER, w!("Volume up 10%"))?;
                AppendMenuW(GetMenu(hwnd), MF_POPUP, menu.0 as usize, w!("&Audio"))
            })();
            if let Err(error) = result {
                let _ = DestroyMenu(menu);
                return Err(error);
            }
            let _ = DrawMenuBar(hwnd);
            menu
        };
        let audio = Self {
            state: Arc::new(Mutex::new(State::new(preferences))),
            menu,
        };
        audio.refresh();
        Ok(audio)
    }

    pub fn refresh(&self) {
        let Some(preferences) = self.state.lock().ok().map(|s| s.preferences()) else {
            return;
        };
        let verb = if preferences.muted { "Unmute" } else { "Mute" };
        let label = HSTRING::from(format!("{verb} host audio ({}%)", preferences.volume));
        // SAFETY: this popup is owned by the live viewer window. Win32 copies
        // the label during the call; no state borrow spans its callbacks.
        let _ = unsafe {
            ModifyMenuW(
                self.menu,
                u32::try_from(MUTE).expect("bounded menu ID"),
                MF_BYCOMMAND | MF_STRING,
                MUTE,
                &label,
            )
        };
    }

    pub fn command(&self, command: usize) -> bool {
        if !matches!(command, MUTE | LOWER | HIGHER) {
            return false;
        }
        if let Ok(mut state) = self.state.lock() {
            let mut preferences = state.preferences();
            match command {
                MUTE => preferences.muted = !preferences.muted,
                LOWER => preferences.volume = preferences.volume.saturating_sub(10),
                HIGHER => preferences.volume = preferences.volume.saturating_add(10).min(100),
                _ => unreachable!(),
            }
            state.set_preferences(preferences);
        }
        self.refresh();
        true
    }

    pub fn complete(&self) -> bool {
        self.state.lock().is_ok_and(|state| state.complete())
    }

    pub fn document(&self) -> serde_json::Value {
        self.state.lock().map_or_else(
            |_| serde_json::json!({"requested": true, "cleanup": "unconfirmed", "absence": "local_state_failed"}),
            |state| state.document(),
        )
    }

    fn factory(&self) -> PlaybackFactory {
        let state = Arc::clone(&self.state);
        Box::new(move |observer| -> Box<dyn Playback> {
            let Some((id, volume)) = state.lock().ok().and_then(|mut s| s.begin()) else {
                // A prior owner's cleanup remains in the original thread.
                // Never allocate a replacement over an unconfirmed native exit.
                return Box::new(Refused);
            };
            let (gain, muted) = volume.snapshot();
            let mut report = Report::new(gain, muted);
            report.volume = volume.clone();
            let report = Rc::new(RefCell::new(report));
            let output = Output::windows(Rc::clone(&report), volume);
            let owner = Owner {
                id,
                state: Arc::clone(&state),
                report,
                retirement: output.retirement(),
                stopped: Cell::new(false),
            };
            if let Err(reason) = observer.configure_audio(Box::new(output)) {
                owner.report.borrow_mut().absent(match reason {
                    ViewerAudioUnavailable::NotSelected => "host_audio_not_selected",
                    ViewerAudioUnavailable::AlreadyConfigured => "audio_output_already_configured",
                });
                owner.stop();
            }
            owner.poll();
            Box::new(owner)
        })
    }
}

pub(super) fn start(config: Config, audio: Option<&Audio>) -> std::io::Result<Session> {
    if let Some(audio) = audio {
        Session::start_with_local_cursor_and_playback(config, audio.factory())
    } else {
        Session::start_with_local_cursor(config)
    }
}

struct Owner {
    id: u64,
    state: Arc<Mutex<State>>,
    report: Rc<RefCell<Report>>,
    retirement: Retirement,
    stopped: Cell<bool>,
}
impl Playback for Owner {
    fn stop(&self) {
        self.stopped.set(true);
        self.retirement.stop();
        self.poll();
    }
    fn is_complete(&self) -> bool {
        self.stopped.get() && self.retirement.is_complete()
    }
    fn poll(&self) {
        // Retirement collects counters into this report. Poll it before taking
        // the immutable report borrow, including during final native cleanup.
        let complete = self.is_complete();
        let report = self.report.borrow();
        let snapshot = Snapshot {
            counts: Counts {
                submitted: report.submitted,
                dropped_packets: report.dropped_packets,
                resets: report.resets,
                expired_generations: report.expired_generations,
                failed_generations: report.failed_generations,
                recovered_generations: report.recovered_generations,
            },
            acknowledged: report.acknowledged,
            active: report.active(),
            complete,
            absence: report.absence,
            first_failure: report.first_failure,
        };
        drop(report);
        if let Ok(mut state) = self.state.lock() {
            state.update(self.id, snapshot);
        }
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.stop();
    }
}
struct Refused;
impl Playback for Refused {
    fn stop(&self) {}
    fn is_complete(&self) -> bool {
        true
    }
    fn poll(&self) {}
}

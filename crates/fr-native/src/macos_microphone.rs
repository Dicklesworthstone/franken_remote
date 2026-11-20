//! Explicit macOS client microphone on the original control attempt. The
//! visible native surface, same-image OS permission helper, host configuration
//! and original controller lease all remain independent prerequisites.
use crate::{
    audio_process::{
        network::{CaptureRetirement, Options, Uplink},
        permission::Permission,
    },
    macos::session::{Microphone, MicrophoneReport},
    macos_microphone_policy::{Action, Gesture, Intent},
};
use fr_core::audio::{MicPermission, MicTalkMode};
use frd::{
    native_connection::reconnect::CallbackError,
    session_startup::{ControlledViewer, viewer_microphone as mic},
};
use std::{
    ffi::c_void, marker::PhantomData, path::PathBuf, ptr::NonNull, sync::Arc, time::Duration,
};

type Act = unsafe extern "C" fn(*mut c_void, u32, u64) -> i32;
type Release = unsafe extern "C" fn(*mut c_void);
unsafe extern "C" {
    fn fr_mac_mic_panel_open(
        context: *mut c_void,
        action: Act,
        release: Release,
        out: *mut *mut c_void,
    ) -> i32;
    fn fr_mac_mic_panel_update(
        owner: *mut c_void,
        state: u32,
        allow: i32,
        talk: i32,
        shown: *mut i32,
    ) -> i32;
    fn fr_mac_mic_panel_close(owner: *mut c_void) -> i32;
}
unsafe extern "C" fn action(context: *mut c_void, kind: u32, captured_ns: u64) -> i32 {
    // SAFETY: native owner adopts one Box<Intent> until its successful close;
    // callbacks occur synchronously on AppKit's sole main thread.
    let intent = unsafe { &*context.cast::<Intent>() };
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let action = match kind {
            0 => {
                intent.stop();
                return 1;
            }
            1 => Action::Allow,
            2 => Action::Talk,
            3 => Action::Mute,
            _ => return 0,
        };
        i32::from(intent.request_from_event(action, captured_ns, || {
            crate::macos::event_time().and_then(|now| u64::try_from(now.as_nanos()).ok())
        }))
    }))
    .unwrap_or_else(|_| {
        intent.stop();
        0
    })
}
unsafe extern "C" fn release(context: *mut c_void) {
    // SAFETY: native clears all callback pointers before returning this exact
    // adopted Box once, only after its original panel has actually closed.
    drop(unsafe { Box::from_raw(context.cast::<Intent>()) });
}

struct Panel {
    raw: Option<NonNull<c_void>>,
    intent: Intent,
    _main: PhantomData<*mut ()>,
}
impl Panel {
    fn open(intent: Intent) -> Result<Self, Self> {
        let context = Box::into_raw(Box::new(intent.clone())).cast();
        let mut raw = std::ptr::null_mut();
        // SAFETY: native adopts context iff raw is non-null, even on failure;
        // all pointers/functions remain valid until its single release call.
        let status = unsafe { fr_mac_mic_panel_open(context, action, release, &raw mut raw) };
        if raw.is_null() {
            // SAFETY: failed native construction explicitly did not adopt it.
            unsafe { release(context) };
        }
        let owner = Self {
            raw: NonNull::new(raw),
            intent,
            _main: PhantomData,
        };
        if status == 0 && owner.raw.is_some() {
            Ok(owner)
        } else {
            Err(owner)
        }
    }
    fn update(&self, state: u32, allow: bool, talk: bool) -> bool {
        let Some(raw) = self.raw else { return false };
        let mut visible = 0;
        // SAFETY: this retained owner is main-thread-only. No callback borrows
        // Local, so the synchronous Intent fence cannot reenter its RefCell.
        let status = unsafe {
            fr_mac_mic_panel_update(
                raw.as_ptr(),
                state,
                i32::from(allow),
                i32::from(talk),
                &raw mut visible,
            )
        };
        status == 0 && visible == 1
    }
    fn close(&mut self) -> bool {
        self.intent.stop();
        let Some(raw) = self.raw else { return true };
        // SAFETY: no other owner calls native close. Failure retains pointer,
        // callback storage and global occupied panel slot for later cleanup.
        if unsafe { fr_mac_mic_panel_close(raw.as_ptr()) } != 0 {
            return false;
        }
        self.raw = None;
        true
    }
}
impl Drop for Panel {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

/// This invocation's one explicit source. It is spent by its first configured
/// control attempt; viewing retries and later grants never inherit recording.
// Selection spent, terminal stop, retirement request and current readiness are
// separate lifetime observations, not interchangeable recording states.
#[allow(clippy::struct_excessive_bools)]
pub struct Local {
    options: Option<Options>,
    image: PathBuf,
    intent: Intent,
    panel: Option<Panel>,
    permission: Option<Permission>,
    retirement: Option<CaptureRetirement>,
    used: bool,
    stopped: bool,
    retiring: bool,
    ready: bool,
    ui_state: u32,
    report: MicrophoneReport,
}
impl Local {
    #[must_use]
    pub fn new(options: Options) -> Self {
        Self {
            image: options.image.clone(),
            options: Some(options),
            intent: Intent::default(),
            panel: None,
            permission: None,
            retirement: None,
            used: false,
            stopped: false,
            retiring: false,
            ready: false,
            ui_state: 0,
            report: MicrophoneReport {
                requested: true,
                ..MicrophoneReport::default()
            },
        }
    }
    fn fail(&mut self, reason: &'static str) {
        self.report.reason = Some(reason);
        self.stop();
    }
    fn sample(&mut self, status: Option<mic::Status>) {
        let Some(status) = status else { return };
        self.report.capture_ready |= status.state == mic::State::Talking;
        self.report.state = match status.state {
            mic::State::Negotiating => "negotiating",
            mic::State::Muted => "muted",
            mic::State::AwaitingHost => "awaiting_host",
            mic::State::Starting => "starting",
            mic::State::Talking => "capture_ready",
            mic::State::Stopped => "stopped",
        };
        self.ready =
            status.state == mic::State::Muted && status.retirement != mic::Retirement::Pending;
        if self.ready && self.intent.epoch().is_some() {
            self.intent.mute();
        }
        self.intent.ready(self.ready && self.permission.is_none());
        self.ui_state = match status.state {
            mic::State::Talking => 3,
            mic::State::Muted => 0,
            mic::State::Stopped => 5,
            _ => 2,
        };
        if let Some(error) = status.reason {
            self.report.reason = Some(reason(error));
        }
        if status.state == mic::State::Stopped {
            self.stop();
        }
    }
}
impl Microphone for Local {
    fn may_offer(&self) -> bool {
        !self.used && !self.stopped
    }
    #[allow(clippy::too_many_lines)] // one original owner: attach, local action, status
    fn turn(&mut self, viewer: &mut ControlledViewer) -> Result<(), CallbackError> {
        if !self.used && !self.stopped {
            self.used = true;
            match Panel::open(self.intent.clone()) {
                Ok(panel) => self.panel = Some(panel),
                Err(panel) => {
                    self.panel = Some(panel);
                    self.fail("microphone_controls_unavailable");
                    return Err(CallbackError);
                }
            }
            if !self.intent.observe(true) {
                self.fail("microphone_controls_lost");
                return Err(CallbackError);
            }
            let options = self.options.take().ok_or(CallbackError)?;
            let check = self.intent.clone();
            match Uplink::expect_on_with_consent(
                viewer,
                options,
                Duration::from_secs(2),
                Arc::new(move || check.epoch()),
            ) {
                Ok(retirement) => {
                    self.retirement = Some(retirement);
                    self.report.selected = true;
                }
                Err(mic::Error::NotSelected) => self.fail("microphone_not_selected"),
                Err(error) => {
                    self.fail(reason(error));
                    return Err(CallbackError);
                }
            }
        }
        if self.intent.stopped() {
            self.stop();
        }
        if self.stopped {
            if self.report.selected && !self.retiring {
                self.retiring = true;
                viewer.retire_microphone().map_err(|_| CallbackError)?;
            }
            return Ok(());
        }
        match self.intent.take_request() {
            Some(Gesture {
                action: Action::Mute,
                ..
            }) => {
                if let Some(permission) = &self.permission {
                    permission.stop();
                }
                viewer.mute_microphone().map_err(|_| CallbackError)?;
            }
            Some(Gesture {
                action: Action::Allow,
                launch_until_ns,
                permission_until_ns,
            }) => {
                self.intent.mute();
                if self.permission.is_none() {
                    let check = self.intent.clone();
                    match Permission::start(
                        &self.image,
                        launch_until_ns,
                        permission_until_ns,
                        move || check.live(),
                    ) {
                        Ok(permission) => {
                            self.permission = Some(permission);
                            self.ui_state = 1;
                        }
                        Err(_) => self.report.reason = Some("microphone_permission_unavailable"),
                    }
                }
            }
            Some(Gesture {
                action: Action::Talk,
                ..
            }) => {
                // Permission completion cannot manufacture this gesture, and
                // Busy consumes it without retaining a restart after cleanup.
                let result = viewer
                    .set_microphone_permission(MicPermission::Granted)
                    .and_then(|()| {
                        viewer.begin_microphone_talk(MicTalkMode::OpenMic { active: true })
                    });
                if let Err(error) = result {
                    self.intent.mute();
                    self.report.reason = Some(reason(error));
                } else {
                    self.report.reason = None;
                }
            }
            None => {}
        }
        self.sample(viewer.microphone_status());
        Ok(())
    }
    fn pump(&mut self, visible: bool, stopped: bool) {
        if stopped && self.used {
            self.stop();
        }
        if !self.used || self.stopped {
            return;
        }
        if let Some(result) = self.permission.as_ref().and_then(Permission::result) {
            self.permission = None;
            self.intent.grant(result.is_ok());
            self.report.permission_granted = result.is_ok();
            self.report.reason = result.err().map(|error| match error {
                crate::audio_process::Error::Denied => "microphone_permission_denied",
                crate::audio_process::Error::Configuration => {
                    "microphone_permission_metadata_missing"
                }
                crate::audio_process::Error::Expired => "microphone_permission_request_expired",
                _ => "microphone_permission_unavailable",
            });
            self.ui_state = if result.is_ok() { 0 } else { 4 };
            self.intent.ready(self.ready);
        }
        let requesting = self.permission.is_some();
        let shown = self.panel.as_ref().is_some_and(|panel| {
            panel.update(
                if requesting { 1 } else { self.ui_state },
                !self.intent.granted() && !requesting,
                self.intent.granted() && self.ready && !requesting,
            )
        });
        if !self.intent.observe(visible && shown) {
            self.fail("microphone_controls_lost");
        }
    }
    fn stop(&mut self) {
        self.intent.stop();
        if let Some(permission) = &self.permission {
            permission.stop();
        }
        self.stopped = true;
        self.ready = false;
        self.options = None;
        self.report.state = "stopped";
    }
    fn complete(&mut self) -> bool {
        if !self.stopped {
            return false;
        }
        let native = self
            .retirement
            .as_ref()
            .is_none_or(CaptureRetirement::is_complete);
        let permission = self.permission.as_ref().is_none_or(Permission::is_complete);
        let panel = self.panel.as_mut().is_none_or(Panel::close);
        native && permission && panel
    }
    fn report(&self) -> MicrophoneReport {
        self.report.clone()
    }
}
impl Drop for Local {
    fn drop(&mut self) {
        self.stop();
    }
}

fn reason(error: mic::Error) -> &'static str {
    match error {
        mic::Error::NotSelected => "microphone_not_selected",
        mic::Error::Busy => "microphone_not_ready",
        mic::Error::SetupExpired => "microphone_setup_expired",
        mic::Error::Capture(mic::CaptureFailure::Denied) => "microphone_permission_denied",
        mic::Error::Capture(mic::CaptureFailure::Selection | mic::CaptureFailure::Unavailable) => {
            "microphone_source_unavailable"
        }
        mic::Error::Capture(mic::CaptureFailure::Discontinuity) => {
            "microphone_capture_discontinuity"
        }
        mic::Error::Capture(mic::CaptureFailure::Spawn) => "microphone_worker_failed",
        mic::Error::Cancelled | mic::Error::Closed => "microphone_session_ended",
        mic::Error::Consent(fr_client::audio::uplink::Error::Permission) => {
            "microphone_permission_missing"
        }
        mic::Error::EgressExpired | mic::Error::ReceiveExpired | mic::Error::SendExpired => {
            "microphone_speech_expired"
        }
        _ => "microphone_failed",
    }
}

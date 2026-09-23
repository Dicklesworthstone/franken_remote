//! Local X11 consent for the ORIGINAL one-use Host approval capability.
//!
//! Only the selected local user's X server and a fresh logind lifetime may
//! produce a positive decision. No remote strings, global input hooks, session
//! replacement, permission grant or network approval endpoint are provided.
use super::{Control, Status as SessionStatus};
use fr_wire::negotiation::Role;
use frd::{
    session_agent::AgentIdentity,
    session_startup::{Approval, Error as ApprovalError},
};
use std::{
    ffi::{CString, c_char, c_int, c_void},
    fmt,
    ptr::NonNull,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const TURN: Duration = Duration::from_millis(10);
const MAP_LIMIT: Duration = Duration::from_secs(2);
// A retired/stuck foreign call retains the permit until its thread really exits.
static WORKER: AtomicBool = AtomicBool::new(false);

unsafe extern "C" {
    fn fr_approval_open(display: *const c_char, role: u32, window: *mut u32) -> *mut c_void;
    fn fr_indicator_close(handle: *mut c_void);
    fn fr_indicator_next(handle: *mut c_void, kind: *mut u32) -> c_int;
    fn fr_indicator_draw(handle: *mut c_void) -> c_int;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    WrongSession,
    AgentUnavailable,
    RoleMismatch,
    Closed,
    SessionUnavailable,
    Approval(ApprovalError),
    Busy,
    ThreadUnavailable,
    NativeFailure,
    MappingExpired,
    Hidden,
    OwnerDropped,
    Cancelled,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "local-approval: {self:?}")
    }
}
impl std::error::Error for Error {}

/// Decision of this exact request, not evidence of native cleanup or media
/// visibility. Allowed observation is not an input lease or OS capture grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Allowed(Role),
    Denied,
    Refused(Error),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Opening,
    Mapped,
    Finished(Outcome),
}
struct Shared {
    approval: Approval,
    session: Control,
    agent: Option<AgentIdentity>,
    status: Mutex<Status>,
    window: AtomicU32,
}
impl Shared {
    fn check(&self) -> Result<(), Error> {
        if self.agent.as_ref().is_some_and(AgentIdentity::is_revoked) {
            return Err(Error::AgentUnavailable);
        }
        if self.session.status() != SessionStatus::Active {
            return Err(Error::SessionUnavailable);
        }
        self.approval.check_pending().map_err(Error::Approval)
    }
    fn state(&self) -> std::sync::MutexGuard<'_, Status> {
        self.status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn refuse(&self, error: Error) {
        let mut state = self.state();
        if !matches!(*state, Status::Finished(_)) {
            // One-use CAS never undoes an already committed decision. On expiry
            // the original Host also refuses; no replacement grant is minted.
            let _ = self.approval.decide(false);
            *state = Status::Finished(Outcome::Refused(error));
        }
    }
    fn live(&self) -> bool {
        if matches!(*self.state(), Status::Finished(_)) {
            return false;
        }
        if let Err(error) = self.check() {
            self.refuse(error);
            return false;
        }
        true
    }
    fn mapped(&self) {
        let mut state = self.state();
        if *state == Status::Opening {
            *state = Status::Mapped;
        }
    }
    fn choose(&self, allow: bool) {
        // This tiny lock orders local stop/drop against positive decisions. No
        // native call or runtime polling occurs under it. Native calls may hang
        // without blocking refusal; final approval still checks its own deadline.
        let mut state = self.state();
        if matches!(*state, Status::Finished(_)) {
            return;
        }
        let outcome = self.check().and_then(|()| {
            if allow && *state != Status::Mapped {
                return Err(Error::NativeFailure);
            }
            self.approval.decide(allow).map_err(Error::Approval)
        });
        *state = Status::Finished(match outcome {
            Ok(()) if allow => Outcome::Allowed(self.approval.role()),
            Ok(()) => Outcome::Denied,
            Err(error) => {
                let _ = self.approval.decide(false);
                Outcome::Refused(error)
            }
        });
    }
}
/// Nonblocking status/denial handle. Deliberately has NO allow/approve method.
/// It cannot retain native window ownership, restart a prompt or change its role.
#[derive(Clone)]
pub struct PromptControl(Arc<Shared>);
impl PromptControl {
    pub fn cancel(&self) {
        self.0.refuse(Error::Cancelled);
    }
    pub fn status(&self) -> Status {
        self.0.live();
        *self.0.state()
    }
    /// Local window integration only; never a session ID or an approval token.
    pub fn window(&self) -> Option<u32> {
        match self.0.window.load(Ordering::Acquire) {
            0 => None,
            id => Some(id),
        }
    }
}
impl fmt::Debug for PromptControl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("LocalApprovalControl")
            .field(&self.status())
            .finish()
    }
}
/// One bounded native thread and one original approval. Construction is
/// nonblocking; XCB work runs ONLY on this thread. Keep the owner until
/// `try_finish` reports that the thread and its window have actually retired.
#[must_use = "retain the prompt to collect its original native cleanup"]
pub struct Prompt {
    control: PromptControl,
    worker: Option<JoinHandle<()>>,
}
impl Prompt {
    /// The display and UID come only from the explicitly selected logind owner.
    /// Fresh positive evidence is required; it is NOT itself capture permission.
    /// Role text comes from the original Approval, not the notification argument.
    /// Rejected prompts deny only their supplied original request.
    pub fn start(session: Control, approval: Approval) -> Result<Self, Error> {
        Self::start_scoped(session, approval, None)
    }
    fn start_scoped(
        session: Control,
        approval: Approval,
        agent: Option<AgentIdentity>,
    ) -> Result<Self, Error> {
        let mut denial = Denial(Some(approval.clone()));
        if agent.as_ref().is_some_and(AgentIdentity::is_revoked) {
            return Err(Error::AgentUnavailable);
        }
        let display = &session.0.selection.display;
        if !session.matches_local_x11(display) || !local_display(display) {
            return Err(Error::WrongSession);
        }
        if session.status() != SessionStatus::Active {
            return Err(Error::SessionUnavailable);
        }
        approval.check_pending().map_err(Error::Approval)?;
        let display = CString::new(display.as_str()).map_err(|_| Error::WrongSession)?;
        WORKER
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| Error::Busy)?;
        let permit = Permit;
        let control = PromptControl(Arc::new(Shared {
            approval,
            session,
            agent,
            status: Mutex::new(Status::Opening),
            window: AtomicU32::new(0),
        }));
        let shared = control.0.clone();
        let started = Instant::now();
        let worker = thread::Builder::new()
            .name("fr-local-approval".into())
            .spawn(move || {
                let _permit = permit;
                let _denial = Denial(Some(shared.approval.clone()));
                run(&shared, &display, started);
                shared.refuse(Error::NativeFailure);
            })
            .map_err(|_| Error::ThreadUnavailable)?;
        denial.0 = None;
        Ok(Self {
            control,
            worker: Some(worker),
        })
    }
    pub fn control(&self) -> PromptControl {
        self.control.clone()
    }
    /// Some means this original native thread ended, not merely that consent was
    /// decided. Polling is nonblocking; Drop requests denial without waiting on X.
    pub fn try_finish(&mut self) -> Option<Outcome> {
        if self
            .worker
            .as_ref()
            .is_some_and(|worker| !worker.is_finished())
        {
            return None;
        }
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            self.control.0.refuse(Error::NativeFailure);
        }
        let Status::Finished(outcome) = self.control.status() else {
            self.control.0.refuse(Error::NativeFailure);
            return Some(Outcome::Refused(Error::NativeFailure));
        };
        Some(outcome)
    }
}
impl fmt::Debug for Prompt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalApprovalPrompt")
            .field("status", &self.control.status())
            .field("cleanup_collected", &self.worker.is_none())
            .finish()
    }
}
impl Drop for Prompt {
    fn drop(&mut self) {
        self.control.0.refuse(Error::OwnerDropped);
    }
}
struct Denial(Option<Approval>);
impl Drop for Denial {
    fn drop(&mut self) {
        if let Some(approval) = &self.0 {
            let _ = approval.decide(false);
        }
    }
}
struct Permit;
impl Drop for Permit {
    fn drop(&mut self) {
        WORKER.store(false, Ordering::Release);
    }
}
struct Native(NonNull<c_void>);
impl Drop for Native {
    fn drop(&mut self) {
        // SAFETY: uniquely owned handle, allocated and used on this thread only.
        unsafe {
            fr_indicator_close(self.0.as_ptr());
        }
    }
}
fn local_display(display: &str) -> bool {
    let Some(number) = display.strip_prefix(':') else {
        return false;
    };
    let mut parts = number.split('.');
    let valid = |s: &str| !s.is_empty() && s.len() <= 5 && s.bytes().all(|b| b.is_ascii_digit());
    parts.next().is_some_and(valid) && parts.next().is_none_or(valid) && parts.next().is_none()
}
fn run(shared: &Shared, display: &CString, started: Instant) {
    if !shared.live() {
        return;
    }
    let role = match shared.approval.role() {
        Role::Observe => 0,
        Role::RequestControl => 1,
    };
    let mut window = 0;
    // SAFETY: valid NUL-terminated input and writable scalar; C retains neither.
    let Some(raw) =
        NonNull::new(unsafe { fr_approval_open(display.as_ptr(), role, &raw mut window) })
    else {
        return;
    };
    let native = Native(raw);
    shared.window.store(window, Ordering::Release);
    let mut mapped = false;
    while shared.live() {
        if !mapped && started.elapsed() >= MAP_LIMIT {
            shared.refuse(Error::MappingExpired);
            return;
        }
        let mut draw = false;
        for _ in 0..32 {
            if !shared.live() {
                return;
            }
            let mut kind = 0;
            // SAFETY: uniquely accessed native resource with a writable scalar.
            match unsafe { fr_indicator_next(native.0.as_ptr(), &raw mut kind) } {
                0 => break,
                1 => {}
                _ => return,
            }
            match kind {
                0 => {}
                1 => draw = true,
                2 => {
                    mapped = true;
                    draw = true;
                }
                3 | 4 => {
                    shared.refuse(Error::Hidden);
                    return;
                }
                5 => {
                    shared.choose(false);
                    return;
                }
                6 => {
                    shared.choose(true);
                    return;
                }
                _ => return,
            }
        }
        if draw && shared.live() {
            // SAFETY: matching thread-local XCB connection/window owner.
            if unsafe { fr_indicator_draw(native.0.as_ptr()) } != 1 {
                return;
            }
            if !shared.live() {
                return;
            }
            if mapped {
                shared.mapped();
            }
        }
        thread::park_timeout(TURN);
    }
}

#[cfg(test)]
mod tests;

mod ui;
pub use ui::ApprovalUi;

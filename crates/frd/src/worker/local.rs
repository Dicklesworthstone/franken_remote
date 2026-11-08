//! In-process presentation for a platform where no child can draw into the
//! viewer's window. On macOS `AppKit` and Metal own the window in this process,
//! and `VideoToolbox` already decodes in its own sandboxed system XPC service
//! (`VTDecoderXPCService`), so the encoded bytes still leave this process for
//! decoding. The caller's `HevcGuard` admits the decoder record and every
//! access unit before any native call, exactly as for a child.
//!
//! The worker owner keeps its contract: one outstanding request, the same
//! request/reply kinds (Present, Decode, `CursorOverlay`, Stop; the rest are
//! refused as unsupported), the same deadlines and poisoning. A reply is built only
//! after the native call has returned success; nothing is inferred from
//! submission.
use super::{Deadline, Error, State, Worker, now, runtime_ready};
use asupersync::{cx::Cx, process::ExitStatus, types::Time};
use fr_media::{
    access_unit::EncodedAccessUnit,
    hevc::DecoderRecord,
    worker::{self, Configuration, Identity, Kind, Record, Role, overlay},
};

/// A native decoder and presenter owned by this process. Calls are
/// synchronous and must stay bounded: the caller checks its deadline before
/// and after each one. Dropping the presenter releases the native decoder.
/// `Send` because the `Worker` that owns it is.
pub trait LocalPresenter: Send {
    /// Configure from the exact admitted hvcC (canonical `DecoderRecord`).
    fn configure(
        &mut self,
        configuration: &Configuration,
        record: &DecoderRecord,
    ) -> Result<(), worker::Error>;
    /// Decode one admitted access unit; when `present`, also draw it.
    /// Returns only after the native decode (and present) completed.
    fn submit(&mut self, unit: &EncodedAccessUnit, present: bool) -> Result<(), worker::Error>;
    /// Install the client-rendered remote cursor between decodes and redraw
    /// the retained picture with it. Not a decode or presentation receipt.
    fn overlay(&mut self, update: &overlay::Update<'_>) -> Result<(), worker::Error>;
}

impl Worker {
    pub(super) fn start_local(
        cx: &Cx,
        mut launch: super::Launch,
        configuration: Configuration,
        record: &DecoderRecord,
        deadline: Deadline,
    ) -> Result<Self, Error> {
        runtime_ready(cx)?;
        let limits = configuration.limits()?;
        let mut presenter = launch.local.take().ok_or(Error::InvalidLaunch)?;
        if launch.retirement.is_some() || launch.target.is_some() {
            return Err(Error::InvalidLaunch);
        }
        if now(cx)? >= deadline.0 {
            return Err(Error::Deadline);
        }
        presenter
            .configure(&configuration, record)
            .map_err(Error::WorkerRefused)?;
        if now(cx)? >= deadline.0 {
            return Err(Error::Deadline);
        }
        Ok(Self {
            child: None,
            retirement: None,
            input: None,
            output: None,
            limits,
            identity: Identity {
                epoch: launch.epoch,
                sequence: 0,
            },
            role: Role::Present,
            state: State::Running,
            exit: None,
            selected: None,
            local: Some(presenter),
            in_process: true,
        })
    }

    /// The in-process half of `exchange_checked`. Present/Decode run the
    /// native call; Stop releases it. Every other request is refused as
    /// unsupported rather than answered with an invented reply.
    pub(super) fn exchange_local<E: From<Error>>(
        &mut self,
        cx: &Cx,
        kind: Kind,
        body: Vec<u8>,
        deadline: Deadline,
        mut live: impl FnMut() -> Result<Time, E>,
    ) -> Result<Record, E> {
        runtime_ready(cx)?;
        let until = live()?.min(deadline.0);
        if now(cx)? >= until {
            return Err(Error::Deadline.into());
        }
        let identity = self.identity;
        let next = identity.sequence.checked_add(1).ok_or(Error::Unavailable)?;
        let limits = self.limits;
        let (reply, payload) = match kind {
            Kind::Present | Kind::Decode => {
                let present = kind == Kind::Present;
                let unit = worker::parse_unit(body, &limits).map_err(Error::Protocol)?;
                let presenter = self.local.as_mut().ok_or(Error::Unavailable)?;
                if let Err(error) = presenter.submit(&unit, present) {
                    self.abort();
                    return Err(Error::WorkerRefused(error).into());
                }
                let reply = if present {
                    Kind::Presented
                } else {
                    Kind::Decoded
                };
                (reply, unit.frame().as_raw().to_be_bytes().to_vec())
            }
            Kind::CursorOverlay => {
                let update = overlay::decode(&body, &limits).map_err(Error::Protocol)?;
                let presenter = self.local.as_mut().ok_or(Error::Unavailable)?;
                if let Err(error) = presenter.overlay(&update) {
                    self.abort();
                    return Err(Error::WorkerRefused(error).into());
                }
                (Kind::CursorOverlayApplied, Vec::new())
            }
            Kind::Stop => {
                self.local = None;
                self.state = State::Stopped;
                (Kind::Stopped, Vec::new())
            }
            _ => return Err(Error::WorkerRefused(worker::Error::Unsupported).into()),
        };
        // A native call that overran its deadline is not a timely reply.
        if now(cx)? >= live()?.min(deadline.0) {
            self.abort();
            return Err(Error::Deadline.into());
        }
        let record = Record::new(reply, identity, payload, &limits).map_err(Error::from)?;
        self.identity.sequence = next;
        Ok(record)
    }

    /// No process exists: reaping releases the native decoder, which is what
    /// reaping a child proves, and reports a zero status for that release.
    pub(super) fn reap_local(&mut self) -> ExitStatus {
        self.local = None;
        let status = ExitStatus::from_parts(Some(0), None);
        self.exit = Some(status);
        self.state = State::Reaped;
        status
    }
}

#[cfg(test)]
#[path = "local_tests.rs"]
mod tests;

//! No native call or process wait escapes this one retained I/O worker.
use super::{Error, OPEN_BUDGET, POLL_INTERVAL, REPLY_BUDGET, Report, Shared, TURN};
use crate::{
    input_process::{ProcessLaunch, kill_group},
    session_ui_process,
};
use fr_core::indicator_process::{
    FRAME_BYTES, Frame, Kind,
    approval::{CONTEXT_BYTES, Context},
};
use fr_wire::negotiation::Role;
use std::{
    io::{self, Read, Write},
    os::unix::net::{UnixDatagram, UnixStream},
    process::Child,
    thread,
    time::Instant,
};

pub(super) fn run(launch: &ProcessLaunch, shared: &Shared) -> Report {
    let opened = shared
        .check()
        .and_then(|()| session_ui_process::spawn(launch).map_err(|_| Error::Unavailable));
    let (child, command, signals) = match opened {
        Ok(opened) => opened,
        Err(error) => {
            return Report {
                decision: Err(error),
                cleanup: Ok(()),
            };
        }
    };
    let mut process = Process {
        child: Some(child),
        command,
        signals,
        shared,
    };
    let decision = process.serve(launch.epoch);
    // A terminal native reply is not enough: actual process retirement precedes
    // caller-side consumption. Native I/O failure is not hidden by cleanup.
    let cleanup = process.finish();
    Report { decision, cleanup }
}
struct Process<'a> {
    child: Option<Child>,
    command: UnixStream,
    signals: UnixDatagram,
    shared: &'a Shared,
}
impl Process<'_> {
    fn check(&self) -> Result<(), Error> {
        self.shared.check()?;
        let mut bytes = [0; 17];
        match self.signals.recv(&mut bytes) {
            Ok(_) => Err(Error::Cancelled),
            Err(error) if retry(&error) => Ok(()),
            Err(_) => Err(Error::Unavailable),
        }
    }
    fn serve(&mut self, epoch: u128) -> Result<bool, Error> {
        let kind = opening_kind(self.shared.original.role());
        let context = if cfg!(target_os = "macos") {
            Some(self.shared.original.context().map_err(Error::Original)?)
        } else {
            None
        };
        let mut request = Frame {
            kind,
            sequence: 1,
            epoch,
        };
        let mut until = self
            .shared
            .started
            .checked_add(OPEN_BUDGET)
            .ok_or(Error::Expired)?;
        loop {
            let opening = (request.sequence == 1)
                .then_some(context.as_ref())
                .flatten();
            if let Some(decision) = self.exchange(request, opening, until)? {
                return Ok(decision);
            }
            let next = Instant::now()
                .checked_add(POLL_INTERVAL)
                .ok_or(Error::Expired)?;
            while Instant::now() < next {
                self.check()?;
                let mut byte = [0; 1];
                match self.command.read(&mut byte) {
                    Err(error) if retry(&error) => {}
                    _ => return Err(Error::Protocol),
                }
                thread::sleep(TURN);
            }
            request = Frame {
                kind: Kind::ApprovalCheck,
                sequence: request.sequence.checked_add(1).ok_or(Error::Protocol)?,
                epoch,
            };
            until = Instant::now()
                .checked_add(REPLY_BUDGET)
                .ok_or(Error::Expired)?;
        }
    }
    fn exchange(
        &mut self,
        request: Frame,
        context: Option<&Context>,
        until: Instant,
    ) -> Result<Option<bool>, Error> {
        let (output, length) = opening_bytes(request, context)?;
        let output = &output[..length];
        let (mut written, mut filled) = (0, 0);
        let mut input = [0; FRAME_BYTES];
        loop {
            self.check()?;
            if Instant::now() >= until {
                return Err(Error::Expired);
            }
            if written < output.len() {
                match self.command.write(&output[written..]) {
                    Ok(0) => return Err(Error::Unavailable),
                    Ok(n) => written += n,
                    Err(error) if retry(&error) => {}
                    Err(_) => return Err(Error::Unavailable),
                }
            }
            if written == output.len() {
                match self.command.read(&mut input[filled..]) {
                    Ok(0) => return Err(Error::Unavailable),
                    Ok(n) => filled += n,
                    Err(error) if retry(&error) => {}
                    Err(_) => return Err(Error::Unavailable),
                }
                if filled == input.len() {
                    self.check()?;
                    if Instant::now() >= until {
                        return Err(Error::Expired);
                    }
                    return response(request, &input);
                }
            }
            thread::sleep(TURN);
        }
    }
    fn finish(&mut self) -> Result<(), Error> {
        let Some(child) = self.child.as_mut() else {
            return Ok(());
        };
        // Never reap during polling: the unreaped child reserves the original
        // process-group ID until this signal. No replacement or unrelated PID.
        kill_group(child);
        child.wait().map_err(|_| Error::Cleanup)?;
        self.child = None;
        Ok(())
    }
}
fn opening_kind(role: Role) -> Kind {
    match (cfg!(target_os = "macos"), role) {
        (true, Role::Observe) => Kind::ApproveViewDetails,
        (true, Role::RequestControl) => Kind::ApproveControlDetails,
        (false, Role::Observe) => Kind::ApproveView,
        (false, Role::RequestControl) => Kind::ApproveControl,
    }
}
fn opening_bytes(
    request: Frame,
    context: Option<&Context>,
) -> Result<([u8; FRAME_BYTES + CONTEXT_BYTES], usize), Error> {
    let attributed = matches!(
        request.kind,
        Kind::ApproveViewDetails | Kind::ApproveControlDetails
    );
    if attributed != context.is_some() {
        return Err(Error::Protocol);
    }
    let mut output = [0; FRAME_BYTES + CONTEXT_BYTES];
    output[..FRAME_BYTES].copy_from_slice(&request.encode().map_err(|_| Error::Protocol)?);
    if let Some(context) = context {
        output[FRAME_BYTES..]
            .copy_from_slice(&context.encode(request).map_err(|_| Error::Protocol)?);
        Ok((output, FRAME_BYTES + CONTEXT_BYTES))
    } else {
        Ok((output, FRAME_BYTES))
    }
}

impl Drop for Process<'_> {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}
fn retry(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
    )
}
pub(super) fn response(request: Frame, bytes: &[u8]) -> Result<Option<bool>, Error> {
    // This adapter cannot be used to reinterpret ordinary indicator readiness
    // or attach an answer from the input executor's protocol.
    if !matches!(
        request.kind,
        Kind::ApproveView
            | Kind::ApproveControl
            | Kind::ApproveViewDetails
            | Kind::ApproveControlDetails
            | Kind::ApprovalCheck
    ) {
        return Err(Error::Protocol);
    }
    match Frame::decode(bytes).and_then(|reply| reply.response_to(request)) {
        Ok(Kind::Ready) => Ok(None),
        Ok(Kind::Allowed) => Ok(Some(true)),
        Ok(Kind::Denied) => Ok(Some(false)),
        Ok(Kind::Refused) => Err(Error::Unavailable),
        _ => Err(Error::Protocol),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fr_core::indicator_process::approval::Features;

    #[test]
    fn attributed_open_requires_complete_context_before_any_reply_is_interpretable() {
        let context = Context::new("nPeer", Some("peer.example"), Features::PLAYBACK, 1).unwrap();
        let request = Frame {
            kind: Kind::ApproveViewDetails,
            sequence: 1,
            epoch: 9,
        };
        assert_eq!(opening_bytes(request, None), Err(Error::Protocol));
        for kind in [
            Kind::ApproveView,
            Kind::ApproveControl,
            Kind::Open,
            Kind::ApprovalCheck,
        ] {
            assert_eq!(
                opening_bytes(Frame { kind, ..request }, Some(&context)),
                Err(Error::Protocol)
            );
        }
        let (bytes, count) = opening_bytes(request, Some(&context)).unwrap();
        assert_eq!(count, FRAME_BYTES + CONTEXT_BYTES);
        assert_eq!(Frame::decode(&bytes[..FRAME_BYTES]), Ok(request));
        assert_eq!(
            Context::decode(request, &bytes[FRAME_BYTES..count]),
            Ok(context)
        );
        assert_eq!(
            opening_bytes(
                Frame {
                    sequence: 2,
                    ..request
                },
                Some(&Context::new("nPeer", None, Features::NONE, 1).unwrap())
            ),
            Err(Error::Protocol)
        );
    }

    #[test]
    fn actual_attributed_exchange_writes_exact_original_context_before_ready() {
        let rt = super::super::tests::runtime();
        let cx = rt.request_cx_with_budget(asupersync::types::Budget::INFINITE);
        let (original, _state) = super::super::tests::original(&cx, Role::Observe);
        let shared = super::super::tests::shared(original);
        let (command, mut child) = UnixStream::pair().unwrap();
        let (signals, _signal_peer) = UnixDatagram::pair().unwrap();
        command.set_nonblocking(true).unwrap();
        signals.set_nonblocking(true).unwrap();
        let context =
            Context::new("nExactPeer", Some("peer.example"), Features::PLAYBACK, 2).unwrap();
        let original_context = context.clone();
        let request = Frame {
            kind: Kind::ApproveViewDetails,
            sequence: 1,
            epoch: 9,
        };
        let peer = thread::spawn(move || {
            child
                .set_read_timeout(Some(std::time::Duration::from_secs(1)))
                .unwrap();
            let mut header = [0; FRAME_BYTES];
            child.read_exact(&mut header).unwrap();
            assert_eq!(Frame::decode(&header), Ok(request));
            let mut bytes = [0; CONTEXT_BYTES];
            child.read_exact(&mut bytes).unwrap();
            assert_eq!(Context::decode(request, &bytes), Ok(original_context));
            for fragment in request.reply(Kind::Ready).encode().unwrap().chunks(3) {
                child.write_all(fragment).unwrap();
            }
        });
        let mut process = Process {
            child: None,
            command,
            signals,
            shared: &shared,
        };
        assert_eq!(
            process.exchange(request, Some(&context), Instant::now() + REPLY_BUDGET),
            Ok(None)
        );
        peer.join().unwrap();
        assert!(
            shared.original.check_pending().is_ok(),
            "readiness and display labels are not consent"
        );
    }
}

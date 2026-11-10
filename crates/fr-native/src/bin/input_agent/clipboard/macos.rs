//! macOS host clipboard role: `AppKit` stays on the main thread; the original
//! framed executor owns `NSPasteboard` on one separate thread. The independent
//! privacy monitor closes frd's content-free signal channel on local Off, lost
//! parent or a stalled UI, even when pasteboard/provider work is blocked.
use super::{
    EXIT_CHANNEL, EXIT_REFUSED, Reply, UnixStream, hello, send,
    serve::{self, Backend},
};
use fr_core::{
    clipboard::{PlatformError, Publication, Stamp, process::Failure},
    limits::{LimitOverrides, ProtocolLimits},
    time::HostInstant,
};
use fr_native::{
    clock,
    macos_clipboard::{Error, MacClipboard, Privacy},
    macos_input::{Indicator, IndicatorStatus},
};
use fr_wire::clipboard::session::synchronize::NativeClipboard;
use std::{
    io::{self, Read},
    net::Shutdown,
    os::fd::AsFd,
    sync::Arc,
    thread::{self, JoinHandle},
    time::Duration,
};

const TURN: Duration = Duration::from_millis(10);
const UI_TURN: Duration = Duration::from_millis(20);

struct State {
    privacy: Privacy,
    signal: UnixStream,
    command: UnixStream,
}
impl State {
    fn stop(&self, user_off: bool) {
        self.privacy.stop(user_off);
        // No AppKit, clipboard lock or native provider call. This immediately
        // fences frd's original lane and wakes a blocked command receive.
        let _ = self.signal.shutdown(Shutdown::Both);
        // Keep writes available for an already attempted Published receipt.
        let _ = self.command.shutdown(Shutdown::Read);
    }

    fn live(&self) -> bool {
        if self.privacy.live() {
            true
        } else {
            self.stop(false);
            false
        }
    }
}

struct Fence<'a>(&'a State);
impl Drop for Fence<'_> {
    fn drop(&mut self) {
        self.0.stop(false);
    }
}

fn watch(state: &State) {
    let _fence = Fence(state);
    let mut signal = &state.signal;
    let mut fixed = [0_u8; 1];
    while state.live() {
        match signal.read(&mut fixed) {
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            // No messages are accepted on this content-free lifetime channel.
            // Parent EOF, any byte and every other fault are terminal.
            _ => break,
        }
    }
}

/// Main-thread native owner. No cleanup waits on a clipboard call: the outer
/// broker retains process custody and can kill/reap a blocked native worker.
struct Surface {
    indicator: Indicator,
    state: Arc<State>,
    watcher: Option<JoinHandle<()>>,
}
impl Surface {
    fn open(command: &UnixStream) -> Result<Self, ()> {
        let signal = UnixStream::from(io::stdout().as_fd().try_clone_to_owned().map_err(drop)?);
        signal.peer_addr().map_err(drop)?;
        signal.set_read_timeout(Some(TURN)).map_err(drop)?;
        let command = command.try_clone().map_err(drop)?;
        let indicator = Indicator::open_clipboard().map_err(drop)?;
        let state = Arc::new(State {
            privacy: Privacy::default(),
            signal,
            command,
        });
        let mut owner = Self {
            indicator,
            state,
            watcher: None,
        };
        if owner.indicator.pump(Duration::ZERO) != IndicatorStatus::Shown
            || !owner.state.privacy.progress()
        {
            return Err(());
        }
        let state = owner.state.clone();
        owner.watcher = Some(
            thread::Builder::new()
                .name("fr-clipboard-privacy".into())
                .spawn(move || watch(&state))
                .map_err(drop)?,
        );
        Ok(owner)
    }

    fn turn(&self) {
        if self.state.privacy.stopped() {
            // The broker retains this blocked worker until kill/reap. Keep the
            // main thread paced without refreshing terminal privacy authority.
            thread::sleep(UI_TURN);
            return;
        }
        match self.indicator.pump(UI_TURN) {
            IndicatorStatus::Shown if self.state.privacy.progress() => {}
            IndicatorStatus::LocalStop => self.state.stop(true),
            _ => self.state.stop(false),
        }
    }
}
impl Drop for Surface {
    fn drop(&mut self) {
        self.state.stop(false);
        if let Some(watcher) = self.watcher.take() {
            // shutdown woke this thread's only bounded socket read. It never
            // enters AppKit or NSPasteboard, and holds no native owner.
            let _ = watcher.join();
        }
    }
}

fn refused(command: &mut UnixStream, error: PlatformError) -> i32 {
    let _ = send(command, 1, Reply::Refused(error), &[]);
    EXIT_REFUSED
}

pub(super) fn run(mut command: UnixStream) -> i32 {
    // The Mac dispatcher has already checked/bound the original parent before
    // reading this role's Hello or doing any AppKit/pasteboard work.
    let (epoch, max) = match hello(&mut command) {
        Ok(hello) => hello,
        Err(code) => return code,
    };
    let Ok(limits) = ProtocolLimits::with_overrides(LimitOverrides {
        max_clipboard_item_bytes: Some(max),
        ..LimitOverrides::default()
    }) else {
        return refused(&mut command, PlatformError::Unsupported);
    };
    let Ok(surface) = Surface::open(&command) else {
        return refused(&mut command, PlatformError::Unavailable);
    };
    let state = surface.state.clone();
    let worker = thread::Builder::new()
        .name("fr-clipboard-executor".into())
        .spawn(move || {
            let original = state.clone();
            let mut clipboard = match MacClipboard::open(&limits, move |until| {
                original.live()
                    && until.is_none_or(|until| before_deadline(clock::monotonic_ns(), until))
            }) {
                Ok(clipboard) => clipboard,
                Err(error) => return refused(&mut command, error),
            };
            if !state.live() {
                return refused(&mut command, PlatformError::Unavailable);
            }
            if send(&mut command, 1, Reply::Ready { epoch }, &[]).is_err() {
                return EXIT_CHANNEL;
            }
            let code = serve::serve(&mut command, &mut clipboard, max, &|| state.live());
            NativeClipboard::close(&mut clipboard);
            code
        });
    let Ok(worker) = worker else {
        return EXIT_CHANNEL;
    };
    while !worker.is_finished() {
        surface.turn();
    }
    // Only a finished executor is joined. A stuck native call stays with its
    // original child; Off/expiry still closes privacy and frd reaps that child.
    let code = worker.join().unwrap_or(EXIT_CHANNEL);
    drop(surface);
    code
}

/// This typed instant is used only by this owner's gate, in raw child-clock
/// micros. Rounding DOWN never extends frd's exclusive nanosecond deadline.
fn deadline(not_after_ns: u64) -> HostInstant {
    HostInstant::from_micros(not_after_ns / 1_000)
}
fn before_deadline(now_ns: Option<u64>, until: HostInstant) -> bool {
    now_ns
        .zip(until.as_micros().checked_mul(1_000))
        .is_some_and(|(now, cap)| now < cap)
}

impl Backend for MacClipboard {
    fn publish_prepared(
        &mut self,
        stamp: Stamp,
        not_after_ns: u64,
        live: &dyn Fn() -> bool,
    ) -> Publication {
        if !live() {
            fr_core::clipboard::ClipboardSink::cancel_prepared(self);
            return Publication::NotSubmitted(PlatformError::Unavailable);
        }
        // The original gate is invoked within every multi-call native mutation,
        // not just here before potentially blocking pasteboard metadata.
        self.publish_prepared_until(stamp, deadline(not_after_ns))
    }

    fn failure(error: Error) -> Failure {
        match error {
            Error::Platform(error) => Failure::Platform(error),
            Error::Access => Failure::Platform(PlatformError::Unavailable),
            Error::NotWatching => Failure::NotWatching,
            Error::NotReading => Failure::NotReading,
            Error::Busy => Failure::Busy,
            Error::LocalChanged => Failure::LocalChanged,
            Error::NoText => Failure::NoSelection,
            Error::Limit => Failure::Limit,
            Error::Allocation => Failure::Allocation,
            Error::InvalidUtf8 => Failure::InvalidUtf8,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn nanosecond_projection_only_shortens_the_original_deadline() {
        for original in [0, 1, 999, 1_000, 1_001, 1_999, 9_999, u64::MAX] {
            let until = deadline(original);
            let cap = until.as_micros().checked_mul(1_000).unwrap();
            assert!(cap <= original);
            assert!(!before_deadline(Some(original), until));
            assert!(!before_deadline(Some(cap), until));
            assert!(!before_deadline(None, until));
            if let Some(before) = cap.checked_sub(1) {
                assert!(before_deadline(Some(before), until));
            }
        }
    }

    fn channels() -> (State, UnixStream, UnixStream, UnixStream) {
        let (signal, peer_signal) = UnixStream::pair().unwrap();
        signal.set_read_timeout(Some(TURN)).unwrap();
        let (command, peer_command) = UnixStream::pair().unwrap();
        peer_command
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let executor = command.try_clone().unwrap();
        let state = State {
            privacy: Privacy::default(),
            signal,
            command,
        };
        assert!(state.privacy.progress());
        (state, peer_signal, peer_command, executor)
    }

    #[test]
    fn off_fences_reads_and_signals_but_preserves_a_completed_receipt_write() {
        let (state, mut peer_signal, mut peer_command, mut executor) = channels();
        state.stop(true);
        assert!(!state.live());
        assert!(!state.privacy.progress());
        assert!(state.privacy.user_off());
        let mut byte = [0; 1];
        assert_eq!(executor.read(&mut byte).unwrap(), 0);
        assert_eq!(peer_signal.read(&mut byte).unwrap(), 0);
        executor.write_all(b"receipt").unwrap();
        let mut receipt = [0; 7];
        peer_command.read_exact(&mut receipt).unwrap();
        assert_eq!(&receipt, b"receipt");
    }

    #[test]
    fn parent_channel_loss_fences_the_original_executor_without_native_work() {
        let (state, peer_signal, _peer_command, mut executor) = channels();
        let state = Arc::new(state);
        let original = state.clone();
        let task = thread::spawn(move || watch(&original));
        drop(peer_signal);
        task.join().unwrap();
        assert!(!state.live());
        assert!(!state.privacy.user_off());
        assert_eq!(executor.read(&mut [0; 1]).unwrap(), 0);
    }

    #[test]
    fn clipboard_panel_cannot_be_opened_off_the_main_thread() {
        let result = thread::spawn(|| Indicator::open_clipboard().map(drop))
            .join()
            .unwrap();
        assert!(result.is_err());
    }
}

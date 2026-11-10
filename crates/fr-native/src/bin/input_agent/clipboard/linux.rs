//! The existing X11 native clipboard owner and its independent Off window.
use super::{
    EXIT_CHANNEL, EXIT_REFUSED, Reply, UnixStream, hello,
    privacy::Privacy,
    send,
    serve::{self, Backend},
};
use fr_core::{
    clipboard::{PlatformError, Publication, Stamp, process::Failure},
    limits::{LimitOverrides, ProtocolLimits},
};
use fr_native::{
    clipboard::{ReadError, SynchronizationError, WatchError, X11Clipboard},
    clock,
};
use fr_wire::clipboard::session::synchronize::NativeClipboard;

pub(super) fn run(mut command: UnixStream) -> i32 {
    let (epoch, max) = match hello(&mut command) {
        Ok(hello) => hello,
        Err(code) => return code,
    };
    let Ok(display) = std::env::var("DISPLAY") else {
        let _ = send(
            &mut command,
            1,
            Reply::Refused(PlatformError::Unsupported),
            &[],
        );
        return EXIT_REFUSED;
    };
    // A real mapped/responsive off switch must precede opening, watching
    // or reading the native clipboard. No controller can enable it again.
    let Ok(privacy) = Privacy::start(&display) else {
        let _ = send(
            &mut command,
            1,
            Reply::Refused(PlatformError::Unavailable),
            &[],
        );
        return EXIT_REFUSED;
    };
    let limits = ProtocolLimits::with_overrides(LimitOverrides {
        max_clipboard_item_bytes: Some(max),
        ..LimitOverrides::default()
    });
    // Only the explicit local display frd selected; no ambient fallback.
    let opened = match limits {
        Ok(limits) if privacy.live() => X11Clipboard::open(&display, &limits, true),
        _ => Err(PlatformError::Unsupported),
    };
    let mut clipboard = match opened {
        Ok(clipboard) => {
            let original = privacy.gate();
            clipboard.with_privacy_gate(move || original.live())
        }
        Err(error) => {
            let _ = send(&mut command, 1, Reply::Refused(error), &[]);
            return EXIT_REFUSED;
        }
    };
    if !privacy.live() {
        return EXIT_REFUSED;
    }
    if send(&mut command, 1, Reply::Ready { epoch }, &[]).is_err() {
        return EXIT_CHANNEL;
    }
    let code = serve::serve(&mut command, &mut clipboard, max, &|| privacy.live());
    // Erase private copies and close X11 on every exit path; no selection
    // reset is sent, so another application's newer copy is untouched.
    NativeClipboard::close(&mut clipboard);
    code
}

impl Backend for X11Clipboard {
    fn publish_prepared(
        &mut self,
        stamp: Stamp,
        not_after_ns: u64,
        live: &dyn Fn() -> bool,
    ) -> Publication {
        self.publish_checked(stamp, || {
            live() && clock::monotonic_ns().is_some_and(|now| now < not_after_ns)
        })
    }

    fn failure(error: SynchronizationError) -> Failure {
        failure(error)
    }
}

const fn failure(error: SynchronizationError) -> Failure {
    match error {
        SynchronizationError::Watch(error) => match error {
            WatchError::Platform(error) => Failure::Platform(error),
            WatchError::Unsupported => Failure::Unsupported,
            WatchError::NotWatching => Failure::NotWatching,
            WatchError::AlreadyWatching => Failure::AlreadyWatching,
            WatchError::GenerationExhausted => Failure::Exhausted,
        },
        SynchronizationError::Read(error) => match error {
            ReadError::Platform(error) => Failure::Platform(error),
            ReadError::Busy => Failure::Busy,
            ReadError::NotReading => Failure::NotReading,
            ReadError::NoSelection => Failure::NoSelection,
            ReadError::Expired => Failure::Expired,
            ReadError::LocalChanged => Failure::LocalChanged,
            ReadError::Limit => Failure::Limit,
            ReadError::Allocation => Failure::Allocation,
            ReadError::Unsupported => Failure::Unsupported,
            ReadError::InvalidUtf8 => Failure::InvalidUtf8,
            ReadError::Malformed => Failure::Malformed,
        },
    }
}

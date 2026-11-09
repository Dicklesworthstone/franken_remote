//! Explicit selections on one already running, opted-in client upload owner.
use crate::Failure;
#[cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "macos-desktop")
))]
#[path = "file_commands/client.rs"]
mod client;
#[path = "file_commands/options.rs"]
mod options;
#[cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "macos-desktop")
))]
#[path = "file_commands/report.rs"]
mod report;

pub(crate) struct Output {
    pub text: String,
    pub exit: u8,
}

pub(crate) fn requested(args: &[String]) -> bool {
    args.first().is_some_and(|arg| arg == "send")
}

pub(crate) fn run(args: &[String]) -> Result<Output, Failure> {
    let command = options::Options::parse(args).map_err(|_| Failure::new(
        "invalid_file_command",
        "Use fr send NODE_ID --connection-id ID --status. Sending (--send /absolute/path), history (--status --request N), cancellation (--cancel N), and pacing (--pause N / --resume N) require the original --lease HANDLE. See fr --help.",
        2,
    ))?;
    #[cfg(any(
        target_os = "linux",
        all(target_os = "macos", feature = "macos-desktop")
    ))]
    {
        client::run(&command)
    }
    #[cfg(not(any(
        target_os = "linux",
        all(target_os = "macos", feature = "macos-desktop")
    )))]
    {
        let _ = command;
        Err(Failure::new(
            "platform_unavailable",
            "Local file commands require Linux or a macOS build with --features macos-desktop.",
            2,
        ))
    }
}

#[cfg(any(
    all(target_os = "linux", feature = "linux-desktop"),
    all(target_os = "macos", feature = "macos-desktop")
))]
pub(crate) fn local_failure(error: frd::native_files::local::Error) -> Failure {
    let (code, next) = report::error_detail(error);
    Failure::new(code, next, 1)
}

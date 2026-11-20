#![forbid(unsafe_code)]
//! Private explicit-source microphone worker. All three standard descriptors
//! are private IPC; none carries diagnostics, authority or a remote connection.
fn main() -> std::process::ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    #[cfg(target_os = "macos")]
    if args.len() == 3 && args[0] == "--authorize" && args[1] == "--parent-pid" {
        let allowed = args[2]
            .to_str()
            .and_then(|value| value.parse().ok())
            .is_some_and(|parent| fr_native::audio_process::permission::run(parent).is_ok());
        return if allowed {
            std::process::ExitCode::SUCCESS
        } else {
            std::process::ExitCode::from(70)
        };
    }
    let parent = (args.len() == 2 && args[0] == "--parent-pid")
        .then(|| args[1].to_str()?.parse().ok())
        .flatten();
    if parent.is_some_and(|id| fr_native::audio_process::child::run(id).is_ok()) {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::from(70)
    }
}

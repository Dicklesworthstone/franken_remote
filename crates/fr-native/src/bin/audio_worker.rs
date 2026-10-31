#![forbid(unsafe_code)]
//! Private explicit-source microphone worker. All three standard descriptors
//! are private IPC; none carries diagnostics, authority or a remote connection.
fn main() -> std::process::ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let parent = (args.len() == 2 && args[0] == "--parent-pid")
        .then(|| args[1].to_str()?.parse().ok())
        .flatten();
    if parent.is_some_and(|id| fr_native::audio_process::child::run(id).is_ok()) {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::from(70)
    }
}

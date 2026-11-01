#![forbid(unsafe_code)]
//! Private sibling-device executor; all standard descriptors are bounded IPC.
fn main() -> std::process::ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let parent = (args.len() == 2 && args[0] == "--parent-pid")
        .then(|| args[1].to_str()?.parse().ok())
        .flatten();
    if parent.is_some_and(|id| fr_native::pipewire_mic::process::run(id).is_ok()) {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::from(70)
    }
}

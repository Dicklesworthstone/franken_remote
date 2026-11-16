//! `fr-windows`: the `FrankenRemote` Windows command line. The library
//! documents its commands; see also docs/windows-client.md.
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    fr_windows::cli(&args)
}

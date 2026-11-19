//! `FrankenRemote.exe`: the Windows desktop application (machine picker,
//! viewer and per-user installer). A windowed program with no console; the
//! `fr-windows` executable remains the command line.
#![windows_subsystem = "windows"]
#![allow(non_snake_case, reason = "the installed application's file name")]

fn main() -> std::process::ExitCode {
    fr_windows::app()
}

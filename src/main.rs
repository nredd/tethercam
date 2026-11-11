//! `tethercam` command-line entry point.

use std::io::{self, Write};
use std::process::ExitCode;

/// Prints the version banner. Subcommands land with the layers they drive.
fn main() -> ExitCode {
    let banner = concat!(env!("CARGO_PKG_NAME"), " ", env!("CARGO_PKG_VERSION"), "\n");
    match io::stdout().lock().write_all(banner.as_bytes()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}

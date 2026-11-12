//! `tethercam` command-line entry point: a thin shell over [`tethercam::cli::run`].

use std::io;
use std::process::ExitCode;

fn main() -> ExitCode {
    tethercam::cli::run(&mut io::stdout().lock())
}

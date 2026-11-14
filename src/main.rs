//! `tethercam` command-line entry point: a thin shell over [`tethercam::cli::run`].

use std::io;
use std::process::ExitCode;

fn main() -> ExitCode {
    // Not locked: with `camera_backend = "icc"` this thread parks in the native run loop and the
    // worker writes to stdout.
    tethercam::cli::run(&mut io::stdout())
}

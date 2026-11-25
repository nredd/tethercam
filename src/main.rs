//! `tethercam` command-line entry point: a thin shell over [`tethercam::cli::run_stdout`].

use std::process::ExitCode;

fn main() -> ExitCode {
    tethercam::cli::run_stdout()
}

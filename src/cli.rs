//! Command-line entry point logic, kept in the library so it is testable.
//!
//! `src/main.rs` is a one-line shell over [`run`]. Subcommands land with the layers they drive.

use std::io::Write;
use std::process::ExitCode;

/// The version banner printed by the binary.
pub const BANNER: &str = concat!(env!("CARGO_PKG_NAME"), " ", env!("CARGO_PKG_VERSION"), "\n");

/// Print the version banner to `out`.
///
/// Returns [`ExitCode::FAILURE`] when `out` cannot be written.
pub fn run(out: &mut dyn Write) -> ExitCode {
    match out.write_all(BANNER.as_bytes()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prints_the_banner() {
        let mut out = Vec::new();
        assert_eq!(run(&mut out), ExitCode::SUCCESS);
        assert_eq!(out, BANNER.as_bytes());
        assert!(BANNER.starts_with("tethercam "));
    }

    #[test]
    fn write_failure_is_a_failure_exit() {
        // A full sink accepts zero bytes, which `write_all` reports as an error.
        let mut full: &mut [u8] = &mut [];
        assert_eq!(run(&mut full), ExitCode::FAILURE);
    }
}

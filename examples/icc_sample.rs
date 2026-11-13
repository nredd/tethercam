//! Gated, read-only `ImageCaptureCore` acceptance consumer. HARDWARE ADMISSION REQUIRED.
//!
//! Without `--hardware-admitted` this prints a notice and exits 0: it never starts native code.
//! With it, and the exact device identifier in `TETHERCAM_ICC_DEVICE_ID`, it runs the
//! `tethercam::transport::icc::read_only_sample` against that one camera: discovery, then two
//! cycles of open, `GetDeviceInfo` (`0x1001`) and a confirmed close in one process. It writes
//! no settings, fires no shutter, runs no vendor handshake and streams nothing.
//!
//! The identifier is read from the environment so it stays out of the process list and shell
//! history, and nothing here prints it. The single output line carries counts and response
//! codes only: never a device identity, name, serial or payload byte. CI never passes the flag.
//!
//! Exit status: 0 for a passing sample, 1 for a failed sample, 2 for usage or startup errors.
//!
//! References:
//! - `docs/macos-camera.md`, Evidence and validation
//! - <https://github.com/nredd/image-capture/blob/main/README.md>

use std::ffi::OsString;
use std::io::{self, Write};
use std::process::ExitCode;

use tethercam::transport::icc::{IccDeviceId, IccError};

/// Open, `GetDeviceInfo`, close cycles in one process, including a same-process reopen.
const CYCLES: usize = 2;
/// Environment variable holding the exact device identifier.
const DEVICE_ENV: &str = "TETHERCAM_ICC_DEVICE_ID";
/// The only accepted argument.
const ADMISSION_FLAG: &str = "--hardware-admitted";

/// Startup failures. Display text never contains the identifier.
#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("usage: icc_sample --hardware-admitted (exact identifier in {DEVICE_ENV})")]
    Usage,
    #[error("{DEVICE_ENV} is missing or not a valid exact identifier")]
    Selector,
    #[error("the native runtime failed: {0}")]
    Native(IccError),
    #[cfg(target_os = "macos")]
    #[error("the async runtime could not start")]
    Runtime(#[source] io::Error),
    #[error("writing the receipt failed")]
    Output(#[source] io::Error),
}

/// What this invocation was asked to do.
#[derive(Debug, PartialEq, Eq)]
enum Mode {
    /// No admission: describe and exit without starting anything native.
    Notice,
    /// Run against exactly this camera.
    Run(IccDeviceId),
}

fn parse(
    args: impl IntoIterator<Item = OsString>, device: Option<OsString>,
) -> Result<Mode, Error> {
    let mut args = args.into_iter();
    let Some(first) = args.next() else {
        return Ok(Mode::Notice);
    };
    if first != ADMISSION_FLAG || args.next().is_some() {
        return Err(Error::Usage);
    }
    let device = device
        .and_then(|value| value.into_string().ok())
        .ok_or(Error::Selector)?;
    IccDeviceId::new(device)
        .map(Mode::Run)
        .map_err(|_| Error::Selector)
}

#[cfg(target_os = "macos")]
fn sample(device: IccDeviceId) -> Result<tethercam::transport::icc::Receipt, Error> {
    use tethercam::transport::icc::{read_only_sample, run};

    // `run` blocks the main thread in the native run loop and hands this closure to a joined
    // worker, which builds its own async runtime.
    run(move |handle| -> io::Result<_> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()?;
        Ok(runtime.block_on(read_only_sample(handle, &device, CYCLES)))
    })
    .map_err(Error::Native)?
    .map_err(Error::Runtime)
}

#[cfg(not(target_os = "macos"))]
fn sample(_device: IccDeviceId) -> Result<tethercam::transport::icc::Receipt, Error> {
    Err(Error::Native(IccError::Runtime))
}

fn execute() -> Result<ExitCode, Error> {
    let mode = parse(std::env::args_os().skip(1), std::env::var_os(DEVICE_ENV))?;
    let Mode::Run(device) = mode else {
        writeln!(
            io::stderr().lock(),
            "compile-only: pass {ADMISSION_FLAG} with {DEVICE_ENV} set to run on a camera; \
             the hardware owner must admit the run"
        )
        .map_err(Error::Output)?;
        return Ok(ExitCode::SUCCESS);
    };
    let receipt = sample(device)?;
    receipt
        .write_to(&mut io::stdout().lock())
        .map_err(Error::Output)?;
    Ok(if receipt.passed(CYCLES) {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

fn main() -> ExitCode {
    execute().unwrap_or_else(|error| {
        // Best effort: there is nowhere left to report a failure to write to stderr.
        let _ = writeln!(io::stderr().lock(), "icc_sample: {error}");
        ExitCode::from(2)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(value: &str) -> OsString {
        OsString::from(value)
    }

    #[test]
    fn no_arguments_never_starts_native_code() {
        assert!(matches!(parse([], None), Ok(Mode::Notice)));
        assert!(matches!(parse([], Some(os("synthetic"))), Ok(Mode::Notice)));
    }

    #[test]
    fn admission_needs_the_flag_alone_and_an_exact_identifier() {
        let run = parse([os(ADMISSION_FLAG)], Some(os("synthetic-camera")));
        assert!(matches!(run, Ok(Mode::Run(id)) if id.as_str() == "synthetic-camera"));
        for args in [vec!["--other"], vec![ADMISSION_FLAG, "extra"]] {
            let args = args.into_iter().map(os);
            assert!(matches!(parse(args, Some(os("x"))), Err(Error::Usage)));
        }
        for device in [None, Some(os("")), Some(os("a\0b"))] {
            assert!(matches!(
                parse([os(ADMISSION_FLAG)], device),
                Err(Error::Selector)
            ));
        }
    }

    #[test]
    fn errors_never_print_the_identifier() {
        let error = parse([os(ADMISSION_FLAG)], Some(os("a\0secret-identifier"))).unwrap_err();
        assert!(!error.to_string().contains("secret-identifier"));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn other_platforms_refuse_to_run() {
        let device = IccDeviceId::new("synthetic".into()).expect("valid");
        assert!(matches!(
            sample(device),
            Err(Error::Native(IccError::Runtime))
        ));
    }
}

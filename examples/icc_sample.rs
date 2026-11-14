//! Gated `ImageCaptureCore` acceptance consumer. HARDWARE ADMISSION REQUIRED.
//!
//! Without `--hardware-admitted` this prints a notice and exits 0: it never starts native code.
//! With it, and either the exact device identifier in `TETHERCAM_ICC_DEVICE_ID` or `--sole-candidate`,
//! it runs the `tethercam::transport::icc::read_only_sample` against one camera: discovery, then two
//! cycles of open, `GetDeviceInfo` (`0x1001`) and a confirmed close in one process. It writes
//! no settings, fires no shutter, runs no vendor handshake and streams nothing.
//!
//! With `--live-view` it runs `tethercam::transport::icc::live_view_probe` instead: the production
//! stack (`IccSession` under the Sony driver) for two cycles of handshake, live-view readiness,
//! thirty decoded frames and a confirmed close, reopening on the same runtime. Beyond the read-only
//! sample it does what the Sony live-view handshake does (it takes host priority); it fires no
//! shutter. The output line carries counts and image dimensions only.
//!
//! `--sole-candidate` selects the camera only if discovery returns exactly one, so the hardware
//! owner never has to handle the identifier; zero or several candidates fail (exit 1, counts
//! only) and the first of several is never taken. The two modes are mutually exclusive. The
//! exact identifier is read from the environment so it stays out of the process list and shell
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

use tethercam::transport::icc::{IccDeviceId, IccError, Selector};

/// Open, `GetDeviceInfo`, close cycles in one process, including a same-process reopen.
const CYCLES: usize = 2;
/// Live-view cycles, likewise reopening on the same runtime.
const LIVE_CYCLES: usize = 2;
/// Decoded live-view frames per cycle.
const LIVE_FRAMES: usize = 30;
/// Environment variable holding the exact device identifier.
const DEVICE_ENV: &str = "TETHERCAM_ICC_DEVICE_ID";
/// Required to start anything native.
const ADMISSION_FLAG: &str = "--hardware-admitted";
/// Pick the camera only if discovery returns exactly one.
const SOLE_FLAG: &str = "--sole-candidate";
/// Run the live-view probe instead of the read-only sample.
const LIVE_FLAG: &str = "--live-view";

/// Startup failures. Display text never contains the identifier.
#[derive(Debug, thiserror::Error)]
enum Error {
    #[error(
        "usage: icc_sample --hardware-admitted [--sole-candidate] [--live-view] (else the exact \
         identifier in {DEVICE_ENV})"
    )]
    Usage,
    #[error("{DEVICE_ENV} is missing or not a valid exact identifier")]
    Identifier,
    #[error("the native runtime failed: {0}")]
    Native(IccError),
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
    /// Run against the one camera this selector picks; `live` selects the live-view probe.
    Run { selector: Selector, live: bool },
}

fn parse(
    args: impl IntoIterator<Item = OsString>, device: Option<OsString>,
) -> Result<Mode, Error> {
    let mut args = args.into_iter();
    let Some(first) = args.next() else {
        return Ok(Mode::Notice);
    };
    if first != ADMISSION_FLAG {
        return Err(Error::Usage);
    }
    let (mut sole, mut live) = (false, false);
    for flag in args {
        // A repeated flag is a mistake, not a no-op.
        let seen = match flag.to_str() {
            Some(SOLE_FLAG) => &mut sole,
            Some(LIVE_FLAG) => &mut live,
            _ => return Err(Error::Usage),
        };
        if std::mem::replace(seen, true) {
            return Err(Error::Usage);
        }
    }
    if sole {
        // Two ways to select a camera at once is a mistake, not a precedence rule.
        return match device {
            None => Ok(Mode::Run {
                selector: Selector::SoleCandidate,
                live,
            }),
            Some(_) => Err(Error::Usage),
        };
    }
    let device = device
        .and_then(|value| value.into_string().ok())
        .ok_or(Error::Identifier)?;
    IccDeviceId::new(device)
        .map(|id| Mode::Run {
            selector: Selector::Exact(id),
            live,
        })
        .map_err(|_| Error::Identifier)
}

/// What a run observed.
enum Receipt {
    Sample(tethercam::transport::icc::Receipt),
    Live(tethercam::transport::icc::LiveReceipt),
}

impl Receipt {
    fn passed(&self) -> bool {
        match self {
            Self::Sample(receipt) => receipt.passed(CYCLES),
            Self::Live(receipt) => receipt.passed(LIVE_CYCLES, LIVE_FRAMES),
        }
    }

    fn write_to(&self, out: &mut dyn Write) -> io::Result<()> {
        match self {
            Self::Sample(receipt) => receipt.write_to(out),
            Self::Live(receipt) => receipt.write_to(out),
        }
    }
}

fn sample(selector: Selector, live: bool) -> Result<Receipt, Error> {
    use tethercam::transport::icc::{live_view_probe, read_only_sample, run};

    // Off macOS `run` is a stub that reports the runtime unavailable. `run` blocks the main thread in the native run loop and hands this closure to a joined
    // worker, which builds its own async runtime.
    run(move |handle| -> io::Result<_> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()?;
        Ok(runtime.block_on(async {
            if live {
                Receipt::Live(live_view_probe(handle, &selector, LIVE_CYCLES, LIVE_FRAMES).await)
            } else {
                Receipt::Sample(read_only_sample(handle, &selector, CYCLES).await)
            }
        }))
    })
    .map_err(Error::Native)?
    .map_err(Error::Runtime)
}

fn execute() -> Result<ExitCode, Error> {
    let mode = parse(std::env::args_os().skip(1), std::env::var_os(DEVICE_ENV))?;
    let Mode::Run { selector, live } = mode else {
        writeln!(
            io::stderr().lock(),
            "compile-only: pass {ADMISSION_FLAG} with {SOLE_FLAG} (or {DEVICE_ENV} set) to run on a \
             camera; the hardware owner must admit the run"
        )
        .map_err(Error::Output)?;
        return Ok(ExitCode::SUCCESS);
    };
    let receipt = sample(selector, live)?;
    receipt
        .write_to(&mut io::stdout().lock())
        .map_err(Error::Output)?;
    Ok(if receipt.passed() {
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
    fn admission_needs_the_flag_and_exactly_one_selection_mode() {
        let run = parse([os(ADMISSION_FLAG)], Some(os("synthetic-camera")));
        let expected = IccDeviceId::new("synthetic-camera".into()).expect("valid");
        assert!(
            matches!(run, Ok(Mode::Run { selector: Selector::Exact(id), live: false }) if id == expected)
        );
        let sole = parse([os(ADMISSION_FLAG), os(SOLE_FLAG)], None);
        assert!(matches!(
            sole,
            Ok(Mode::Run {
                selector: Selector::SoleCandidate,
                live: false
            })
        ));
        for args in [
            [ADMISSION_FLAG, SOLE_FLAG, LIVE_FLAG],
            [ADMISSION_FLAG, LIVE_FLAG, SOLE_FLAG],
        ] {
            assert!(matches!(
                parse(args.map(os), None),
                Ok(Mode::Run {
                    selector: Selector::SoleCandidate,
                    live: true
                })
            ));
        }
        let live = parse(
            [os(ADMISSION_FLAG), os(LIVE_FLAG)],
            Some(os("synthetic-camera")),
        );
        assert!(matches!(live, Ok(Mode::Run { live: true, .. })));
        for args in [
            vec![ADMISSION_FLAG, LIVE_FLAG, LIVE_FLAG],
            vec![ADMISSION_FLAG, SOLE_FLAG, SOLE_FLAG],
        ] {
            let args = args.into_iter().map(os);
            assert!(matches!(parse(args, None), Err(Error::Usage)));
        }
        for args in [
            vec!["--other"],
            vec![ADMISSION_FLAG, "extra"],
            vec![ADMISSION_FLAG, SOLE_FLAG, "extra"],
            vec![SOLE_FLAG],
        ] {
            let args = args.into_iter().map(os);
            assert!(matches!(parse(args, Some(os("x"))), Err(Error::Usage)));
        }
        // An identifier alongside `--sole-candidate` is ambiguous intent, so it is refused.
        let both = parse([os(ADMISSION_FLAG), os(SOLE_FLAG)], Some(os("x")));
        assert!(matches!(both, Err(Error::Usage)));
        for device in [None, Some(os("")), Some(os("a\0b"))] {
            assert!(matches!(
                parse([os(ADMISSION_FLAG)], device),
                Err(Error::Identifier)
            ));
        }
    }

    #[test]
    fn errors_never_print_the_identifier() {
        let error = parse([os(ADMISSION_FLAG)], Some(os("a\0secret-identifier"))).unwrap_err();
        assert!(!error.to_string().contains("secret-identifier"));
    }

    #[test]
    fn a_test_thread_is_never_the_native_main_thread() {
        // Off macOS `run` is a stub; on macOS the real one refuses off the main thread before it
        // touches anything native. The guard keeps that true even if a harness ran this inline.
        assert_ne!(std::thread::current().name(), Some("main"));
        assert!(matches!(
            sample(Selector::SoleCandidate, false),
            Err(Error::Native(IccError::Runtime))
        ));
    }
}

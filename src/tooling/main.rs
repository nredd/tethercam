//! Repository verification utilities, separate from the installed camera application.
//!
//! References:
//! - <https://llvm.org/docs/CommandGuide/llvm-cov.html>
//! - <https://docs.kernel.org/usb/usbip_protocol.html>

mod clients;
mod gate;
mod profile;
mod usbip;
mod verify;

use std::ffi::{OsStr, OsString};
use std::io::{self, Write};
use std::path::Path;
use std::process::ExitCode;

/// Failures intentionally omit native output, environment values and report payloads.
#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("invalid utility arguments")]
    Usage,
    #[error("utility I/O failed")]
    Io(#[from] io::Error),
    #[error("invalid measurement JSON")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Invalid(&'static str),
}

fn main() -> ExitCode {
    run(
        std::env::args_os().skip(1),
        &mut io::stdout().lock(),
        &mut io::stderr().lock(),
        &mut io::stdin().lock(),
    )
}

/// Bound argument consumption before filesystem or process operations.
fn dispatch(args: &[OsString], out: &mut dyn Write, input: &mut dyn io::Read) -> Result<(), Error> {
    match args {
        [cmd, rest @ ..] if cmd == "verify" => verify::run(rest),
        [cmd, rest @ ..] if cmd == "soak" => verify::soak(rest),
        [cmd, rest @ ..] if cmd == "serve-owned" => verify::serve(rest, input),
        [cmd, cfg, output] if cmd == "prepare-clients" => {
            clients::prepare(Path::new(cfg), Path::new(output)).map(drop)
        }
        [cmd, action] if cmd == "usbip" => usbip::run(action),
        [cmd, action] if cmd == "usbip-local" => usbip::local(action),
        [cmd, tag] if cmd == "release-tag" => release_tag(tag),
        [cmd, framehash, packets, metrics, source, h264] if cmd == "profile-summary" => {
            profile::check(
                [framehash, packets, metrics, source, h264].map(Path::new),
                out,
            )
        }
        [cmd] if cmd == "coverage-gate" => gate::check(out),
        [cmd] if cmd == "make-help" => make_help(include_str!("../../Makefile"), out),
        _ => Err(Error::Usage),
    }
}

fn run(
    args: impl IntoIterator<Item = OsString>, out: &mut dyn Write, err: &mut dyn Write,
    input: &mut dyn io::Read,
) -> ExitCode {
    let args: Vec<_> = args.into_iter().take(10).collect();
    match dispatch(&args, out, input) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(err, "{error}");
            ExitCode::FAILURE
        }
    }
}

fn release_tag(tag: &OsStr) -> Result<(), Error> {
    if tag != concat!("v", env!("CARGO_PKG_VERSION")) {
        return Err(Error::Invalid("release tag does not match package version"));
    }
    Ok(())
}

fn make_help(makefile: &str, out: &mut dyn Write) -> Result<(), Error> {
    for line in makefile.lines() {
        if let Some((rule, help)) = line.split_once("##")
            && let Some((target, _)) = rule.split_once(':')
        {
            writeln!(out, "{target:20} {}", help.trim())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;

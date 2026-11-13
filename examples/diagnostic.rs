//! Explicit native/synthetic wiring for the reusable diagnostic API.
//!
//! References:
//! - <https://docs.rs/tethercam/latest/tethercam/test_support/diagnostics/>

use std::io::{self, Write};
use std::path::Path;
use tethercam::ptp::session::{Session, SessionConfig};
use tethercam::sony::SonyConfig;
use tethercam::test_support::diagnostics::{self, Operation};
use tethercam::transport::Transport;
use tethercam::transport::sim::sony::{ResolutionControls, SonySimConfig, sony_camera};
use tethercam::transport::usb::{NusbBus, UsbError, camera, list_cameras, open};

/// Preserve distinct argument, diagnostic, filesystem and native transport failure sources.
#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("diagnostic operation failed")]
    Diagnostic(#[from] diagnostics::Error),
    #[error("invalid diagnostic count")]
    Count(#[from] std::num::ParseIntError),
    #[error("diagnostic count is out of range")]
    CountRange(#[from] std::num::TryFromIntError),
    #[error("diagnostic I/O failed")]
    Io(#[from] io::Error),
    #[error("native diagnostic USB failed")]
    Usb(#[from] UsbError),
}

async fn measure<T: Transport + Send>(
    transport: T, packet: usize, operation: Operation, output: &Path,
) -> Result<(), Error> {
    let mut session = Session::new(
        transport,
        SessionConfig {
            max_packet_size: packet,
            ..SessionConfig::default()
        },
    );
    let artifacts = diagnostics::run(
        &mut session,
        SonyConfig::default(),
        operation,
        &mut io::stdout().lock(),
    )
    .await?;
    for artifact in artifacts {
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(output.join(artifact.name))?;
        file.write_all(&artifact.bytes)?;
        file.sync_all()?;
    }
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Error> {
    let args: Vec<_> = std::env::args_os().skip(1).take(5).collect();
    let [mode, operation, count, output] = args.as_slice() else {
        return Err(diagnostics::Error::Limits.into());
    };
    let count: u16 = count.to_str().ok_or(diagnostics::Error::Limits)?.parse()?;
    let operation = match operation.to_str() {
        Some("properties") => Operation::Properties,
        Some("source") => Operation::Source { frames: count },
        Some("configure") => Operation::Configure {
            size: u8::try_from(count)?,
            single_drive: true,
            jpeg_quality: true,
        },
        Some("capture") => Operation::Capture {
            count: u8::try_from(count)?,
        },
        _ => return Err(diagnostics::Error::Limits.into()),
    };
    operation.validate()?;
    let native = match mode.to_str() {
        Some("--native") => true,
        Some("--simulate") => false,
        _ => return Err(diagnostics::Error::Limits.into()),
    };
    let mut directory = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        directory.mode(0o700);
    }
    directory.create(output)?;
    if native {
        let cameras = list_cameras(&NusbBus)?;
        let selected = camera::pick(&cameras)?;
        let transport = open(&NusbBus, selected).await?;
        let packet = transport.bulk_max_packet_size();
        writeln!(
            io::stderr().lock(),
            "Negotiated USB speed: {:?}; bulk packet bytes={packet}",
            selected.speed
        )?;
        measure(transport, packet, operation, Path::new(output)).await
    } else {
        let transport = sony_camera(SonySimConfig {
            resolution_controls: ResolutionControls::Present,
            ..SonySimConfig::default()
        });
        measure(transport, 512, operation, Path::new(output)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;

    #[test]
    fn diagnostic_failure_sources_remain_typed() {
        let errors: [Error; 5] = [
            diagnostics::Error::Limits.into(),
            "not-a-count"
                .parse::<u16>()
                .expect_err("invalid integer")
                .into(),
            u8::try_from(256_u16).expect_err("out of range").into(),
            io::Error::other("synthetic output failure").into(),
            UsbError::NoCamera.into(),
        ];
        for error in errors {
            assert!(error.source().is_some());
            assert_ne!(error.to_string(), "");
            assert_ne!(format!("{error:?}"), "");
        }
    }
}

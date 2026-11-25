//! Explicit operator I/O, total argument parsing and command dispatch.
//!
//! References:
//! - `docs/architecture.md`, configuration, security and supervisor contracts

#[cfg(feature = "macos-icc")]
mod cameras;
#[cfg(test)]
mod diagnostic_tests;
mod diagnostics;
#[cfg(feature = "macos-icc")]
mod empty;
mod first_frame;
#[cfg(feature = "macos-icc")]
mod icc;
#[cfg(not(feature = "macos-icc"))]
mod icc {
    //! Without the `macos-icc` feature the native backend cannot be selected.

    use super::{Error, Options, Write};

    pub(super) fn run_stdout() -> std::process::ExitCode {
        super::run(&mut std::io::stdout())
    }
    use crate::config::Config;

    pub(super) fn launch(
        _options: &Options, _cfg: &Config, _dispatch: &tracing::Dispatch, _out: &mut dyn Write,
    ) -> Result<(), Error> {
        Err(Error::IccUnavailable)
    }
}
mod native;
mod options;
#[cfg(test)]
mod options_tests;
mod simulator;
#[cfg(test)]
mod tests;

pub use first_frame::FirstFrameCause;
pub use options::{Command, Options, ParseError};

use std::io::Write;
use std::process::ExitCode;

use thiserror::Error;

use crate::config::{self, Config};
use crate::supervisor::CameraFactory;

/// The version banner printed without arguments or with `--version`.
pub const BANNER: &str = concat!(env!("CARGO_PKG_NAME"), " ", env!("CARGO_PKG_VERSION"), "\n");
/// Short command/option help. Daemon operations require an explicit `service` subcommand.
pub const HELP: &str = "tethercam setup|probe|run|doctor [--config PATH] [--simulate JPEG] [--no-discovery]\n\
setup creates owner-only credentials and identity; probe inspects a camera; run serves in the foreground; doctor checks readiness.\n\
tethercam doctor [--config PATH] [--report]\n\
--report prints a sanitized support summary.\n\
tethercam cameras [--config PATH] [--describe]\n\
cameras lists what the configured backend can see; --describe (icc only) briefly opens each device read-only for its make and model, which can raise a trust prompt on a phone; with camera_id set only that device is described.\n\
tethercam service install|uninstall|status [--user]\n\
Linux defaults to a privileged system unit; --user selects its user manager. macOS uses a per-user launchd agent. Service paths are fixed; uninstall retains credentials.\n";

/// User-facing diagnostic categories. Values supplied by cameras and configuration parsers are
/// retained as sources but never interpolated into CLI errors or logs.
#[derive(Debug, Error)]
pub enum Error {
    /// A fixed, sanitized report failure category.
    #[error("{0}")]
    Report(&'static str),
    /// Invalid arguments.
    #[error(transparent)]
    Arguments(#[from] ParseError),
    /// A protected native service lifecycle operation failed.
    #[error(transparent)]
    Service(#[from] crate::service::Error),
    /// Configuration parsing, permissions or validation failed.
    #[error("configuration rejected; check syntax, values and owner-only permissions")]
    Config(#[from] config::Error),
    /// Writing operator output or reading a simulator JPEG failed.
    #[error("operator input/output failed")]
    Io(#[from] std::io::Error),
    /// No selected camera is attached.
    #[error("no selected camera; connect a camera in PC Remote mode")]
    NoCamera,
    /// A sanitized USB enumeration/opening failure.
    #[error(transparent)]
    Open(#[from] crate::supervisor::OpenError),
    /// A sanitized failure while inspecting the camera session.
    #[error("camera probe failed: {}", .0.to_string().trim_start_matches("no first frame: "))]
    Probe(FirstFrameCause),
    /// `doctor` got no first live-view frame; the category names the likely cause.
    #[error(transparent)]
    FirstFrame(#[from] FirstFrameCause),
    /// The clock is unsuitable for authenticated ONVIF clients.
    #[error("system clock is implausible; synchronize time before adoption")]
    Clock,
    /// A configured port is not available.
    #[error("configured listener unavailable; check addresses, permissions and port conflicts")]
    Ports,
    /// No selected network interface can advertise this listener.
    #[error("no usable network interface for discovery")]
    Interface,
    /// `camera_backend = "icc"` cannot run here: unsupported platform or build, or the one native
    /// runtime could not start. The application never ran, so nothing was started or opened.
    #[error(
        "camera_backend `icc` is unavailable: it needs macOS, a build with the `macos-icc` feature and a free native runtime"
    )]
    IccUnavailable,
    /// The native runtime failed after the application started, e.g. its worker panicked. This
    /// is not a platform or build problem: something ran and may have touched the camera.
    #[error("camera_backend `icc` failed after it started; check the log and restart the process")]
    IccFailed,
    /// The one native runtime is quarantined for the life of the process.
    #[error("native camera runtime quarantined; it cannot be reused, restart the process")]
    Quarantined,
    /// A support report failed and native cleanup quarantined the runtime.
    #[cfg(feature = "macos-icc")]
    #[error("native camera runtime quarantined; it cannot be reused, restart the process")]
    QuarantinedReport(Option<&'static str>),
    /// Probe failed and quarantined the native camera runtime.
    #[error("{}; native camera runtime quarantined; restart the process", Error::Probe(*.0))]
    QuarantinedProbe(FirstFrameCause),
    /// The simulator input is not a supported JPEG.
    #[error("simulator JPEG rejected")]
    Simulator,
    /// Network/media service startup or serving failed.
    #[error("runtime failed; check listener permissions and media settings")]
    Runtime(#[from] crate::supervisor::RunError),
}

/// Parse process arguments, dispatch and report errors through `out`, never an implicit print.
pub fn run(out: &mut dyn Write) -> ExitCode {
    run_using(out, &icc::launch)
}

/// Run with an owned stdout sink so the ICC worker and cleanup watcher can write to it
/// even when native cleanup prevents the main thread from returning.
pub fn run_stdout() -> ExitCode {
    icc::run_stdout()
}

type IccLaunch<'a> =
    dyn Fn(&Options, &Config, &tracing::Dispatch, &mut dyn Write) -> Result<(), Error> + 'a;

fn run_using(out: &mut dyn Write, launch: &IccLaunch<'_>) -> ExitCode {
    // The executable owns global logging. If an embedding host already installed a logger,
    // retain it; `execute` itself changes only the scoped tracing subscriber.
    let _ = tracing_log::LogTracer::init();
    finish(
        execute_using(
            Options::parse(std::env::args_os().skip(1)),
            out,
            config::default_path(),
            &mut crate::service::Manager::new,
            launch,
        ),
        out,
    )
}

fn finish(result: Result<(), Error>, out: &mut dyn Write) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(Error::Report(_)) => ExitCode::FAILURE,
        Err(error) => {
            let _ = writeln!(out, "Error: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Dispatch already parsed options. Setup explicitly presents the generated credentials to
/// the operator through `out`; no tracing event contains credentials, URLs or device identity.
///
/// # Errors
/// Returns typed argument, file, camera or runtime diagnostics. Support reports write and
/// flush their own sanitized failure category before returning `Error::Report`. On ICC,
/// borrowed output from the worker is buffered until native cleanup returns.
pub fn execute(options: Result<Options, ParseError>, out: &mut dyn Write) -> Result<(), Error> {
    execute_with_path(options, out, config::default_path())
}

fn execute_with_path(
    options: Result<Options, ParseError>, out: &mut dyn Write,
    default_path: Result<std::path::PathBuf, config::Error>,
) -> Result<(), Error> {
    execute_with_service(
        options,
        out,
        default_path,
        &mut crate::service::Manager::new,
    )
}

fn execute_with_service(
    options: Result<Options, ParseError>, out: &mut dyn Write,
    default_path: Result<std::path::PathBuf, config::Error>,
    service: &mut dyn FnMut(bool) -> Result<crate::service::Manager, crate::service::Error>,
) -> Result<(), Error> {
    execute_using(options, out, default_path, service, &icc::launch)
}

fn execute_using(
    options: Result<Options, ParseError>, out: &mut dyn Write,
    default_path: Result<std::path::PathBuf, config::Error>,
    service: &mut dyn FnMut(bool) -> Result<crate::service::Manager, crate::service::Error>,
    launch: &IccLaunch<'_>,
) -> Result<(), Error> {
    let options = options?;
    if options.report {
        if options.command != Command::Doctor {
            return Err(ParseError::Invalid.into());
        }
        let result = writeln!(
            out,
            "tethercam: {}\nos: {}\narch: {}",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH
        )
        .map_err(Error::Io)
        .and_then(|()| execute_selected(&options, out, default_path, service, launch));
        return finish_report(out, &result);
    }
    execute_selected(&options, out, default_path, service, launch)
}

fn finish_report(out: &mut dyn Write, result: &Result<(), Error>) -> Result<(), Error> {
    let category = result.as_ref().err().map(report_error);
    finalize_report(out, category)
        .or(Err(Error::Report("io")))
        .and(category.map_or(Ok(()), |category| Err(Error::Report(category))))
}

/// Write fixed categories and attempt the flush even when a write fails.
fn finalize_report<'a>(
    out: &mut dyn Write, categories: impl IntoIterator<Item = &'a str>,
) -> std::io::Result<()> {
    let written = categories
        .into_iter()
        .try_for_each(|category| writeln!(out, "Error: {category}"));
    written.and(out.flush())
}

fn report_error(error: &Error) -> &'static str {
    match error {
        Error::Config(_) => "configuration",
        Error::Clock => "clock",
        Error::Ports => "ports",
        Error::NoCamera => "no_camera",
        Error::Open(_) => "camera_open",
        Error::FirstFrame(cause) => match cause {
            FirstFrameCause::NotRemote => "not_remote",
            FirstFrameCause::Refused => "handshake_refused",
            FirstFrameCause::LiveViewUnavailable => "live_view_unavailable",
            FirstFrameCause::NotReady => "live_view_not_ready",
            FirstFrameCause::NoFrame => "no_frame",
            FirstFrameCause::Timeout => "camera_timeout",
            FirstFrameCause::Disconnected => "camera_disconnected",
            FirstFrameCause::Transport => "transport",
            FirstFrameCause::Busy => "camera_busy",
            FirstFrameCause::Permission => "camera_permission",
            FirstFrameCause::Rejected => "camera_rejected",
            FirstFrameCause::Protocol => "protocol",
            FirstFrameCause::Decode => "frame_decode",
            FirstFrameCause::Encoder => "encoder",
            FirstFrameCause::CloseFailed => "camera_close",
            FirstFrameCause::Framing => "framing",
            FirstFrameCause::Unclassified => "camera_unclassified",
            #[cfg(feature = "macos-icc")]
            FirstFrameCause::NativeDeadline => "camera_timeout",
            #[cfg(feature = "macos-icc")]
            FirstFrameCause::Native(_) => "backend",
            FirstFrameCause::Quarantined => "backend",
            FirstFrameCause::CleanupFailed => "camera_close",
        },
        Error::IccUnavailable | Error::IccFailed | Error::Quarantined => "backend",
        Error::Io(_) => "io",
        _ => "other",
    }
}

fn execute_selected(
    options: &Options, out: &mut dyn Write,
    default_path: Result<std::path::PathBuf, config::Error>,
    service: &mut dyn FnMut(bool) -> Result<crate::service::Manager, crate::service::Error>,
    launch: &IccLaunch<'_>,
) -> Result<(), Error> {
    match options.command {
        Command::Version => {
            out.write_all(BANNER.as_bytes())?;
            return Ok(());
        }
        Command::Help => {
            out.write_all(HELP.as_bytes())?;
            return Ok(());
        }
        Command::Service { action, user } => {
            writeln!(out, "Service: {}", service(user)?.manage(action)?)?;
            return Ok(());
        }
        _ => {}
    }
    let path = options.config.clone().map_or(default_path, Ok)?;
    if options.command == Command::Setup {
        let cfg = Config::load_or_create(&path)?;
        let address = if cfg.onvif_address.is_unspecified() {
            format!("<host>:{}", cfg.onvif_port)
        } else {
            std::net::SocketAddr::new(cfg.onvif_address, cfg.onvif_port).to_string()
        };
        writeln!(
            out,
            "Configuration: {}\nONVIF: http://{address}/onvif/device_service\nUsername: {}\nPassword: {}\nAdopt by the host's reachable address using these credentials.",
            path.display(),
            cfg.credentials.username.expose(),
            cfg.credentials.password.expose()
        )?;
        return Ok(());
    }
    let cfg = Config::load(&path)?;
    if options.report {
        let backend = match cfg.camera_backend {
            config::CameraBackend::Usb => "usb",
            config::CameraBackend::Icc => "icc",
        };
        writeln!(out, "backend: {backend}")?;
    }
    let dispatch = tracing::Dispatch::new(
        tracing_subscriber::fmt()
            .with_max_level(if options.report {
                tracing::level_filters::LevelFilter::OFF
            } else {
                diagnostics::level(&cfg.log_level).into()
            })
            .with_target(false)
            .with_ansi(false)
            .with_writer(std::io::stderr)
            .finish(),
    );
    tracing::dispatcher::with_default(&dispatch, || {
        if options.simulate.is_none() && cfg.camera_backend == config::CameraBackend::Icc {
            // Selected, never a fallback: it fails before any camera access where unsupported.
            return launch(options, &cfg, &dispatch, out);
        }
        dispatch_runtime(
            options,
            &cfg,
            out,
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build(),
            crate::transport::usb::NusbBus,
        )
    })
}

fn dispatch_runtime<B: crate::transport::usb::Bus + Send>(
    options: &Options, cfg: &Config, out: &mut dyn Write,
    runtime: std::io::Result<tokio::runtime::Runtime>, bus: B,
) -> Result<(), Error> {
    runtime?.block_on(async {
        if let Some(path) = &options.simulate {
            let jpeg = std::fs::read(path)?;
            let factory = simulator::Simulator::new(jpeg)?;
            command(factory, cfg, options, out, diagnostics::Signals::install()).await
        } else {
            if let Command::Cameras { .. } = options.command {
                return native::list(&bus, out);
            }
            if options.command == Command::Probe {
                native::report(&bus, out)?;
            }
            let factory = native::Native::new(bus, cfg.camera_serial.clone());
            command(factory, cfg, options, out, diagnostics::Signals::install()).await
        }
    })
}

async fn command<F: CameraFactory>(
    factory: F, cfg: &Config, options: &Options, out: &mut dyn Write,
    signals: std::io::Result<diagnostics::Signals>,
) -> Result<(), Error> {
    let signals = signals?;
    command_until(factory, cfg, options, out, Box::pin(signals.wait())).await
}

async fn command_until<F: CameraFactory>(
    factory: F, cfg: &Config, options: &Options, out: &mut dyn Write,
    shutdown: std::pin::Pin<Box<dyn Future<Output = ()> + '_>>,
) -> Result<(), Error> {
    command_with(factory, cfg, options, out, shutdown, if_addrs::get_if_addrs).await
}

/// [`command_until`] with the host interface enumeration injected, so a failing one is testable.
async fn command_with<F: CameraFactory>(
    factory: F, cfg: &Config, options: &Options, out: &mut dyn Write,
    shutdown: std::pin::Pin<Box<dyn Future<Output = ()> + '_>>,
    interfaces: fn() -> std::io::Result<Vec<if_addrs::Interface>>,
) -> Result<(), Error> {
    match options.command {
        Command::Probe => diagnostics::probe(factory, out).await,
        Command::Doctor => {
            diagnostics::check_host(
                cfg,
                crate::onvif::time::Clock::now_unix(&crate::onvif::time::SystemClock),
            )?;
            if options.report {
                return diagnostics::doctor_report(factory, &cfg.streams, out).await;
            }
            diagnostics::doctor_camera(
                factory,
                crate::sony::SonyConfig::default(),
                &cfg.streams,
                out,
            )
            .await?;
            writeln!(
                out,
                "Clock, permissions, listener ports and camera mode are ready."
            )?;
            Ok(())
        }
        Command::Run => {
            diagnostics::check_host(
                cfg,
                crate::onvif::time::Clock::now_unix(&crate::onvif::time::SystemClock),
            )?;
            let discovery = if options.no_discovery {
                None
            } else {
                Some(diagnostics::watched_discovery(cfg, interfaces)?)
            };
            Box::pin(crate::supervisor::serve(
                factory, cfg, discovery, shutdown, out,
            ))
            .await?;
            Ok(())
        }
        Command::Setup
        | Command::Version
        | Command::Help
        | Command::Cameras { .. }
        | Command::Service { .. } => Ok(()),
    }
}

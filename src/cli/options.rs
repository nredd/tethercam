//! A small total command parser. Unknown argument contents are never echoed to diagnostics.
//!
//! References:
//! - `docs/architecture.md`, security model

use std::ffi::OsString;
use std::path::PathBuf;

use thiserror::Error;

/// Foreground CLI operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Show the version.
    Version,
    /// Show help.
    Help,
    /// Publish or load owner-only credentials and identity.
    Setup,
    /// Inspect the selected camera without starting listeners.
    Probe,
    /// Serve until interrupted.
    Run,
    /// Check clock, ports, permissions and camera readiness.
    Doctor,
    /// List the cameras the configured backend can see, so one can be selected explicitly.
    Cameras {
        /// Also open each listed `ImageCaptureCore` device read-only to read its `DeviceInfo`
        /// make and model. Opt-in: it can raise a trust prompt on a phone.
        describe: bool,
    },
    /// Manage a native service without touching a camera.
    Service {
        /// Requested lifecycle operation.
        action: crate::service::Action,
        /// Select Linux's unprivileged user manager; macOS is always per-user.
        user: bool,
    },
}

/// Validated CLI syntax. Paths preserve non-UTF-8 OS strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// Foreground operation.
    pub command: Command,
    /// Explicit configuration path, otherwise the platform default.
    pub config: Option<PathBuf>,
    /// Explicit synthetic JPEG source instead of any real USB access.
    pub simulate: Option<PathBuf>,
    /// Suppress multicast announcements, without disabling authenticated endpoints.
    pub no_discovery: bool,
}

/// Syntax error with no attacker-controlled text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ParseError {
    /// Unknown subcommand/option, duplicate option or illegal option combination.
    #[error("invalid arguments; use --help")]
    Invalid,
    /// An option requiring a path has none.
    #[error("missing option path; use --help")]
    MissingPath,
}

impl Options {
    /// Parse arguments excluding the executable. Every argument sequence has a typed result.
    ///
    /// # Errors
    /// Rejects unknown/duplicate arguments and missing paths. Only four distinct options
    /// exist, so parsing stops after at most eight arguments even for an infinite iterator.
    pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Self, ParseError> {
        let mut args = args.into_iter();
        let first = args.next();
        let command = match first.as_deref().map(std::ffi::OsStr::to_str) {
            None | Some(Some("--version" | "-V")) => Command::Version,
            Some(Some("--help" | "-h")) => Command::Help,
            Some(Some("setup")) => Command::Setup,
            Some(Some("probe")) => Command::Probe,
            Some(Some("run")) => Command::Run,
            Some(Some("doctor")) => Command::Doctor,
            Some(Some("cameras")) => Command::Cameras { describe: false },
            Some(Some("service")) => return service(&mut args),
            _ => return Err(ParseError::Invalid),
        };
        let mut options = Self {
            command,
            config: None,
            simulate: None,
            no_discovery: false,
        };
        while let Some(arg) = args.next() {
            match arg.to_str() {
                Some("--config") if options.config.is_none() => {
                    options.config = Some(path(&mut args)?);
                }
                Some("--simulate") if options.simulate.is_none() => {
                    options.simulate = Some(path(&mut args)?);
                }
                Some("--no-discovery") if !options.no_discovery => options.no_discovery = true,
                Some("--describe") if options.command == (Command::Cameras { describe: false }) => {
                    options.command = Command::Cameras { describe: true };
                }
                _ => return Err(ParseError::Invalid),
            }
        }
        if matches!(command, Command::Help | Command::Version)
            && (options.config.is_some() || options.simulate.is_some() || options.no_discovery)
        {
            return Err(ParseError::Invalid);
        }
        let listing = matches!(command, Command::Setup | Command::Cameras { .. });
        if listing && (options.simulate.is_some() || options.no_discovery) {
            return Err(ParseError::Invalid);
        }
        Ok(options)
    }
}

fn service(args: &mut dyn Iterator<Item = OsString>) -> Result<Options, ParseError> {
    use crate::service::Action;
    let first = args.next().ok_or(ParseError::Invalid)?;
    let action = match first.to_str() {
        Some("install") => Action::Install,
        Some("uninstall") => Action::Uninstall,
        Some("status") => Action::Status,
        Some("--help" | "-h") if args.next().is_none() => {
            return Ok(Options {
                command: Command::Help,
                config: None,
                simulate: None,
                no_discovery: false,
            });
        }
        _ => return Err(ParseError::Invalid),
    };
    let user = match args.next() {
        None => false,
        Some(value) if value == "--user" && args.next().is_none() => true,
        _ => return Err(ParseError::Invalid),
    };
    Ok(Options {
        command: Command::Service { action, user },
        config: None,
        simulate: None,
        no_discovery: false,
    })
}

fn path(args: &mut dyn Iterator<Item = OsString>) -> Result<PathBuf, ParseError> {
    let path = args.next().ok_or(ParseError::MissingPath)?;
    if path.is_empty() || path.to_string_lossy().starts_with('-') {
        return Err(ParseError::MissingPath);
    }
    Ok(PathBuf::from(path))
}

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
    /// Rejects unknown/duplicate arguments and missing paths. Only three distinct options
    /// exist, so parsing stops after at most seven arguments even for an infinite iterator.
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
                _ => return Err(ParseError::Invalid),
            }
        }
        if matches!(command, Command::Help | Command::Version)
            && (options.config.is_some() || options.simulate.is_some() || options.no_discovery)
        {
            return Err(ParseError::Invalid);
        }
        if command == Command::Setup && (options.simulate.is_some() || options.no_discovery) {
            return Err(ParseError::Invalid);
        }
        Ok(options)
    }
}

fn path(args: &mut dyn Iterator<Item = OsString>) -> Result<PathBuf, ParseError> {
    let path = args.next().ok_or(ParseError::MissingPath)?;
    if path.is_empty() || path.to_string_lossy().starts_with('-') {
        return Err(ParseError::MissingPath);
    }
    Ok(PathBuf::from(path))
}

//! Explicit native service installation; no operation runs until requested by the operator.
//!
//! References:
//! - <https://www.freedesktop.org/software/systemd/man/latest/systemd.service.html>
//! - <https://www.freedesktop.org/software/systemd/man/latest/systemd.exec.html>
//! - <https://www.freedesktop.org/software/systemd/man/latest/sysusers.d.html>
//! - <https://www.freedesktop.org/software/systemd/man/latest/udev.html>
//! - `launchd.plist(5)`, `launchctl(1)` on macOS

#[cfg(unix)]
mod files;
#[cfg(unix)]
mod native;
#[cfg(unix)]
pub use native::Manager;
#[cfg(unix)]
mod templates;
#[cfg(unix)]
#[cfg(test)]
mod tests;
#[cfg(not(unix))]
mod unsupported;
#[cfg(not(unix))]
pub use unsupported::Manager;

#[cfg(unix)]
use std::path::{Path, PathBuf};

use thiserror::Error;

/// Requested lifecycle operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Publish files, load and enable the service.
    Install,
    /// Stop, unload and remove only matching generated files; retain credentials.
    Uninstall,
    /// Inspect installation and manager state without changing anything.
    Status,
}

/// Service management failures. External command output is deliberately never displayed.
#[derive(Debug, Error)]
pub enum Error {
    /// An OS operation failed.
    #[error("service filesystem/process I/O failed")]
    Io(#[from] std::io::Error),
    /// Path, identity or manager reply cannot be represented safely.
    #[error("unsupported service path, identity or manager reply")]
    Invalid,
    /// A file is unrelated, a symlink, has unsafe permissions or unexpected ownership.
    #[error("service path conflicts with an existing or insecure file; refusing to change it")]
    Conflict,
    /// The eventual service identity cannot traverse or execute the installed program.
    #[error(
        "service executable access rejected; user services need owner access, system services need other read/execute and ancestor traversal permissions"
    )]
    ExecutableAccess,
    /// Native manager rejected an operation or was inaccessible.
    #[error(
        "native service manager rejected the operation; check scope, privileges and manager availability"
    )]
    Manager,
    /// Installation failed while preparing protected persisted credentials.
    #[error("service configuration rejected; check owner-only permissions and syntax")]
    Config(#[from] crate::config::Error),
    /// Unsupported operating system.
    #[error("services require macOS or Linux")]
    Unsupported,
}

/// Observed installation and native manager state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// No owned unit/agent file or registered service exists.
    Absent,
    /// Files exist but the manager has not loaded the service.
    Unloaded,
    /// Registered but not running.
    Inactive,
    /// systemd reports active or launchd reports a running PID.
    Active,
    /// systemd reports a failed unit or a stopped launchd process exited unsuccessfully.
    Failed,
}

impl std::fmt::Display for Status {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str(match self {
            Self::Absent => "absent",
            Self::Unloaded => "installed, not loaded",
            Self::Inactive => "inactive",
            Self::Active => "active",
            Self::Failed => "failed",
        })
    }
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Platform {
    Mac,
    Linux,
}

#[cfg(unix)]
struct Plan {
    platform: Platform,
    user: bool,
    uid: u32,
    config: PathBuf,
    binary: PathBuf,
    unit: PathBuf,
    files: Vec<(PathBuf, String)>,
    uninstall: (PathBuf, String),
    logs: Option<PathBuf>,
}

#[cfg(unix)]
impl Plan {
    fn new(
        platform: Platform, user: bool, uid: u32, home: &Path, binary: &Path,
    ) -> Result<Self, Error> {
        let user = user || platform == Platform::Mac;
        if !user && uid != 0 {
            return Err(Error::Manager);
        }
        // ProtectHome deliberately hides home directories from the system account. Require
        // a durable, administrator-installed binary rather than a build in someone's home.
        if !(user || binary.starts_with("/usr") || binary.starts_with("/opt")) {
            return Err(Error::Invalid);
        }
        let config = if platform == Platform::Mac {
            home.join("Library/Application Support/tethercam/config.toml")
        } else if user {
            home.join(".config/tethercam/config.toml")
        } else {
            PathBuf::from("/var/lib/tethercam/config.toml")
        };
        let logs = (platform == Platform::Mac).then(|| home.join("Library/Logs/tethercam"));
        let unit = if platform == Platform::Mac {
            home.join("Library/LaunchAgents/net.tethercam.agent.plist")
        } else if user {
            home.join(".config/systemd/user/tethercam.service")
        } else {
            PathBuf::from("/etc/systemd/system/tethercam.service")
        };
        let template = if let Some(logs) = &logs {
            templates::launchd(binary, &config, logs)?
        } else {
            templates::systemd(binary, &config, user)?
        };
        let uninstall = (unit.with_extension("uninstall"), template.clone());
        let mut files = vec![(unit.clone(), template)];
        if !user {
            files.push((
                "/etc/sysusers.d/tethercam.conf".into(),
                templates::SYSUSERS.into(),
            ));
            files.push((
                "/etc/udev/rules.d/70-tethercam.rules".into(),
                templates::UDEV.into(),
            ));
        }
        Ok(Self {
            platform,
            user,
            uid,
            config,
            binary: binary.into(),
            unit,
            files,
            uninstall,
            logs,
        })
    }

    fn unit(&self) -> &Path {
        &self.unit
    }
}

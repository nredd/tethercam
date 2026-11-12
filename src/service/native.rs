//! Native command argv and lifecycle decisions, shared with injected-runner tests.
//!
//! References: launchctl(1), systemctl(1), systemd-sysusers(8), udevadm(8).

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::{Action, Error, Plan, Platform, Status, files::Files};

pub(super) struct Reply {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
}

type Runner<'a> = dyn FnMut(&str, &[OsString]) -> io::Result<Reply> + 'a;

fn run(program: &str, args: &[OsString]) -> io::Result<Reply> {
    let output = Command::new(program)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C")
        .envs(std::env::vars_os().filter(|(name, _)| bus_variable(name)))
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()?;
    Ok(Reply {
        code: output.status.code(),
        stdout: output.stdout,
    })
}

fn bus_variable(name: &std::ffi::OsStr) -> bool {
    name == "XDG_RUNTIME_DIR" || name == "DBUS_SESSION_BUS_ADDRESS"
}

fn checked(runner: &mut Runner<'_>, program: &str, args: &[OsString]) -> Result<Vec<u8>, Error> {
    let reply = runner(program, args)?;
    if reply.code != Some(0) {
        return Err(Error::Manager);
    }
    Ok(reply.stdout)
}

fn number(bytes: &[u8]) -> Result<u32, Error> {
    std::str::from_utf8(bytes)
        .map_err(|_| Error::Invalid)?
        .trim()
        .parse()
        .map_err(|_| Error::Invalid)
}

fn platform(os: &str) -> Result<Platform, Error> {
    match os {
        "macos" => Ok(Platform::Mac),
        "linux" => Ok(Platform::Linux),
        _ => Err(Error::Unsupported),
    }
}

/// An explicitly scoped native manager. Construction only resolves identity and paths.
pub struct Manager {
    plan: Plan,
    fs: Files,
    runner: Box<Runner<'static>>,
}

impl Manager {
    /// Resolve this process's platform, identity and standard paths without installing anything.
    ///
    /// # Errors
    /// Rejects unsupported platforms, invalid paths and insufficient system-scope privileges.
    pub fn new(user: bool) -> Result<Self, Error> {
        prepare(
            user,
            std::env::consts::OS,
            std::env::var_os("HOME").map(PathBuf::from),
            std::env::current_exe(),
            &mut run,
        )
        .map(|plan| {
            let uid = plan.uid;
            Self {
                plan,
                fs: Files::new("/".into(), uid),
                runner: Box::new(run),
            }
        })
    }

    /// Apply the requested operation, without enumerating or opening any camera.
    ///
    /// # Errors
    /// Returns protected-file, credential, or native manager failures. Failed installations
    /// retain matching files so the same invocation can safely resume.
    pub fn manage(&mut self, action: Action) -> Result<Status, Error> {
        lifecycle(&self.plan, action, &self.fs, &mut self.runner)
    }
}

fn prepare(
    user: bool, os: &str, home: Option<PathBuf>, binary: io::Result<PathBuf>,
    runner: &mut Runner<'_>,
) -> Result<Plan, Error> {
    let platform = platform(os)?;
    let uid = number(&checked(runner, "/usr/bin/id", &["-u".into()])?)?;
    let home = home.ok_or(Error::Invalid)?;
    let binary = binary?;
    Plan::new(platform, user, uid, &home, &binary)
}

fn systemctl(plan: &Plan, runner: &mut Runner<'_>, args: &[&str]) -> Result<Vec<u8>, Error> {
    let mut command = Vec::new();
    if plan.user {
        command.push("--user".into());
    }
    command.extend(args.iter().map(OsString::from));
    checked(runner, "/usr/bin/systemctl", &command)
}

fn launch_target(plan: &Plan) -> String {
    format!("gui/{}/net.tethercam.agent", plan.uid)
}

fn observe(plan: &Plan, present: bool, runner: &mut Runner<'_>) -> Result<Status, Error> {
    let status = match plan.platform {
        Platform::Mac => {
            // A missing GUI domain is an unavailable manager, not an absent agent.
            checked(
                runner,
                "/bin/launchctl",
                &["print".into(), format!("gui/{}", plan.uid).into()],
            )?;
            let reply = runner(
                "/bin/launchctl",
                &["list".into(), "net.tethercam.agent".into()],
            )?;
            match reply.code {
                Some(0) => parse_launchd(&reply.stdout)?,
                Some(113) => {
                    if present {
                        Status::Unloaded
                    } else {
                        Status::Absent
                    }
                }
                _ => return Err(Error::Manager),
            }
        }
        Platform::Linux => {
            let bytes = systemctl(
                plan,
                runner,
                &[
                    "show",
                    "tethercam.service",
                    "--property=LoadState,ActiveState",
                    "--no-pager",
                ],
            )?;
            parse_state(&bytes, present)?
        }
    };
    if !present && status != Status::Absent {
        return Err(Error::Conflict);
    }
    Ok(status)
}

fn parse_launchd(bytes: &[u8]) -> Result<Status, Error> {
    // `list <label>` returns launchctl's property dictionary, not the human summary table.
    // Only scalar process keys matter; absolute, control-free argv cannot inject these lines.
    let text = std::str::from_utf8(bytes).map_err(|_| Error::Invalid)?;
    let mut pid = None;
    let mut exit = None;
    for line in text.lines().map(str::trim) {
        if let Some(value) = line.strip_prefix("\"PID\" = ") {
            if pid.is_some() {
                return Err(Error::Invalid);
            }
            let value = value.strip_suffix(';').ok_or(Error::Invalid)?;
            pid = Some(value.parse::<u32>().map_err(|_| Error::Invalid)?);
        } else if let Some(value) = line.strip_prefix("\"LastExitStatus\" = ") {
            if exit.is_some() {
                return Err(Error::Invalid);
            }
            let value = value.strip_suffix(';').ok_or(Error::Invalid)?;
            exit = Some(value.parse::<i32>().map_err(|_| Error::Invalid)?);
        }
    }
    match (pid, exit) {
        (Some(1..), _) => Ok(Status::Active),
        (_, Some(0)) => Ok(Status::Inactive),
        (_, Some(_)) => Ok(Status::Failed),
        _ => Err(Error::Invalid),
    }
}

fn parse_state(bytes: &[u8], present: bool) -> Result<Status, Error> {
    let text = std::str::from_utf8(bytes).map_err(|_| Error::Invalid)?;
    let mut load = None;
    let mut active = None;
    for line in text.lines() {
        match line.split_once('=') {
            Some(("LoadState", value)) if load.is_none() => load = Some(value),
            Some(("ActiveState", value)) if active.is_none() => active = Some(value),
            _ => return Err(Error::Invalid),
        }
    }
    match (load, active) {
        (Some("not-found"), Some("inactive")) => Ok(if present {
            Status::Unloaded
        } else {
            Status::Absent
        }),
        (Some("loaded"), Some("active" | "reloading" | "activating")) => Ok(Status::Active),
        (Some("loaded"), Some("inactive" | "deactivating")) => Ok(Status::Inactive),
        (Some("loaded"), Some("failed")) | (Some("error" | "bad-setting" | "masked"), Some(_)) => {
            Ok(Status::Failed)
        }
        _ => Err(Error::Invalid),
    }
}

fn lifecycle(
    plan: &Plan, action: Action, fs: &Files, runner: &mut Runner<'_>,
) -> Result<Status, Error> {
    // Inspect *all* owned paths before changing any file or calling a mutating command.
    let mut present = false;
    for (path, content) in &plan.files {
        let exists = fs.matches(path, content.as_bytes())?;
        if path == plan.unit() {
            present = exists;
        }
    }
    // A matching protected receipt survives unit unlink, directory-sync and reload failures.
    // Its contents bind recovery to this exact installation, never an arbitrary cached job.
    let (receipt, unit_content) = &plan.uninstall;
    let unit_content = unit_content.as_bytes();
    let resuming = fs.matches(receipt, unit_content)?;
    if resuming && action != Action::Uninstall {
        return Err(Error::Conflict);
    }
    let status = observe(plan, present || resuming, runner)?;
    match action {
        Action::Status => Ok(status),
        Action::Install => {
            fs.executable(&plan.binary, !plan.user)?;
            for (path, content) in &plan.files {
                fs.publish(path, content.as_bytes())?;
            }
            let owner = if plan.user {
                None
            } else {
                Some(system_user(fs, runner)?)
            };
            // Root-owned ancestors remain trusted, but the config directory/file belong to the
            // dedicated account, which is the only account allowed to read the persisted secret.
            let config_fs = Files {
                root: fs.root.clone(),
                uid: owner.map_or(fs.uid, |(uid, _)| uid),
                calls: fs.calls,
            };
            config_fs.protect_config(&plan.config)?;
            if let Some((uid, gid)) = owner {
                config_fs.chown(&plan.config, uid, gid, false)?;
                config_fs.chown(Path::new("/var/lib/tethercam"), uid, gid, true)?;
            }
            if let Some(logs) = &plan.logs {
                fs.directory(logs, true)?;
                fs.log(&logs.join("stdout.log"))?;
                fs.log(&logs.join("stderr.log"))?;
            }
            match plan.platform {
                Platform::Mac if matches!(status, Status::Unloaded | Status::Absent) => {
                    checked(
                        runner,
                        "/bin/launchctl",
                        &[
                            "bootstrap".into(),
                            format!("gui/{}", plan.uid).into(),
                            plan.unit().into(),
                        ],
                    )?;
                }
                Platform::Mac if status != Status::Active => {
                    checked(
                        runner,
                        "/bin/launchctl",
                        &["kickstart".into(), launch_target(plan).into()],
                    )?;
                }
                Platform::Mac => {}
                Platform::Linux => {
                    systemctl(plan, runner, &["daemon-reload"])?;
                    systemctl(plan, runner, &["enable", "--now", "tethercam.service"])?;
                }
            }
            // Manager replies describe actual state after starting, not an assumed success.
            observe(plan, true, runner)
        }
        Action::Uninstall => {
            if present {
                match plan.platform {
                    Platform::Mac if status != Status::Unloaded => {
                        checked(
                            runner,
                            "/bin/launchctl",
                            &["bootout".into(), launch_target(plan).into()],
                        )?;
                    }
                    Platform::Mac => {}
                    Platform::Linux => {
                        systemctl(plan, runner, &["disable", "--now", "tethercam.service"])?;
                    }
                }
            }
            if present || resuming {
                fs.publish(receipt, unit_content)?;
            }
            for (path, content) in &plan.files {
                fs.remove(path, content.as_bytes())?;
            }
            if plan.platform == Platform::Linux {
                systemctl(plan, runner, &["daemon-reload"])?;
                if !plan.user {
                    reload_udev(runner)?;
                }
            }
            // Clear ownership evidence only after all generated-file syncs and native
            // reloads have completed. A failure after this unlink is also retryable: the
            // manager has already forgotten the unit, so no receipt is needed then.
            fs.remove(receipt, unit_content)?;
            Ok(Status::Absent)
        }
    }
}

fn system_user(fs: &Files, runner: &mut Runner<'_>) -> Result<(u32, u32), Error> {
    checked(
        runner,
        "/usr/bin/systemd-sysusers",
        &[fs.resolve(Path::new("/etc/sysusers.d/tethercam.conf"))
            .into()],
    )?;
    let uid = number(&checked(
        runner,
        "/usr/bin/id",
        &["-u".into(), "tethercam".into()],
    )?)?;
    let gid = number(&checked(
        runner,
        "/usr/bin/id",
        &["-g".into(), "tethercam".into()],
    )?)?;
    if uid == 0 || gid == 0 {
        return Err(Error::Conflict);
    }
    let account = checked(
        runner,
        "/usr/bin/getent",
        &["passwd".into(), "tethercam".into()],
    )?;
    validate_account(&account, uid, gid)?;
    reload_udev(runner)?;
    Ok((uid, gid))
}

fn validate_account(bytes: &[u8], uid: u32, gid: u32) -> Result<(), Error> {
    let text = std::str::from_utf8(bytes).map_err(|_| Error::Invalid)?;
    let mut fields = text.trim_end_matches('\n').split(':');
    let name = fields.next();
    let _password_marker = fields.next();
    let owner = number(fields.next().ok_or(Error::Invalid)?.as_bytes())?;
    let group = number(fields.next().ok_or(Error::Invalid)?.as_bytes())?;
    let _description = fields.next();
    let home = fields.next();
    let shell = fields.next();
    if name != Some("tethercam")
        || owner != uid
        || group != gid
        || home != Some("/var/lib/tethercam")
        || shell != Some("/usr/sbin/nologin")
        || fields.next().is_some()
    {
        return Err(Error::Conflict);
    }
    Ok(())
}

fn reload_udev(runner: &mut Runner<'_>) -> Result<(), Error> {
    checked(
        runner,
        "/usr/bin/udevadm",
        &["control".into(), "--reload-rules".into()],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests;

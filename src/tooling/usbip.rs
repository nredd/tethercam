//! Ephemeral hosted-runner USB/IP prerequisites and exact synthetic-device permissions.
//!
//! References:
//! - <https://docs.kernel.org/usb/usbip_protocol.html>
//! - <https://docs.github.com/en/actions/reference/variables-reference>

use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use super::Error;

const SERIAL: &str = "TETHERCAM-USBIP-0001";

fn invalid() -> Error {
    Error::Invalid(
        "USB/IP requires a verified ephemeral GitHub Linux runner and synthetic VHCI device",
    )
}

fn authorize(env: &mut dyn FnMut(&str) -> Option<OsString>, linux: bool) -> Result<(), Error> {
    for (name, value) in [
        ("CI", "true"),
        ("GITHUB_ACTIONS", "true"),
        ("RUNNER_ENVIRONMENT", "github-hosted"),
        ("RUNNER_OS", "Linux"),
    ] {
        if env(name).as_deref() != Some(OsStr::new(value)) {
            return Err(invalid());
        }
    }
    if !linux {
        return Err(invalid());
    }
    Ok(())
}

/// Native argv only; discard output on failure rather than echoing privileged diagnostics.
fn command(program: &Path, args: &[OsString]) -> Result<String, Error> {
    let output = Command::new(program)
        .args(args)
        .env_clear()
        .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
        .env("LC_ALL", "C")
        .output()?;
    if !output.status.success() {
        return Err(Error::Invalid("USB/IP native command failed"));
    }
    String::from_utf8(output.stdout).map_err(|_| Error::Invalid("invalid native command output"))
}

fn argv(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

fn numeric(text: &str) -> Result<u16, Error> {
    if text.is_empty() || !text.bytes().all(|v| v.is_ascii_digit()) {
        return Err(invalid());
    }
    text.parse().map_err(|_| invalid())
}

/// No path separators, whitespace or dot components can enter a sysfs lookup.
fn busid(text: &str) -> Result<(), Error> {
    let mut parts = text.split('-');
    numeric(parts.next().unwrap_or_default())?;
    let chain = parts.next().ok_or_else(invalid)?;
    if parts.next().is_some() {
        return Err(invalid());
    }
    for part in chain.split('.') {
        numeric(part)?;
    }
    Ok(())
}

fn canonical(path: &Path) -> std::io::Result<PathBuf> {
    fs::canonicalize(path)
}

fn name(value: &OsStr) -> Result<&str, Error> {
    value.to_str().ok_or_else(invalid)
}

fn read(path: &Path) -> Result<String, Error> {
    Ok(fs::read_to_string(path)?.trim_end_matches('\n').to_owned())
}

/// Sysfs and device roots are fixed in production; temporary roots exercise the same I/O in tests.
struct Host<'a> {
    root: &'a Path,
    privileged: bool,
    canonical: fn(&Path) -> std::io::Result<PathBuf>,
    entries: fn(&Path) -> std::io::Result<Vec<PathBuf>>,
    exec: fn(&Path, &[OsString]) -> Result<String, Error>,
    pause: fn(Duration),
    character: fn(&Path) -> Result<bool, Error>,
}

impl Host<'_> {
    fn call(&self, program: &str, args: &[&str]) -> Result<String, Error> {
        (self.exec)(Path::new(program), &argv(args))
    }

    fn tool(&self) -> Result<PathBuf, Error> {
        let mut candidates = Vec::new();
        for entry in (self.entries)(&self.root.join("usr/lib/linux-tools"))? {
            let path = entry.join("usbip");
            if path.is_file() {
                candidates.push(path);
            }
        }
        candidates.sort();
        candidates.into_iter().next().ok_or_else(invalid)
    }

    fn usbip(&self, args: &[&str]) -> Result<(), Error> {
        let mut all = argv(&["15"]);
        if self.privileged {
            all.extend(argv(&["/usr/bin/sudo", "--"]));
        } else {
            for control in ["attach", "detach"] {
                fs::OpenOptions::new().write(true).open(
                    self.root
                        .join("sys/devices/platform/vhci_hcd.0")
                        .join(control),
                )?;
            }
        }
        all.push(self.tool()?.into_os_string());
        all.extend(argv(args));
        (self.exec)(Path::new("/usr/bin/timeout"), &all)?;
        Ok(())
    }

    /// Canonical VHCI ancestry plus exact serial/vendor excludes physical USB devices.
    fn synthetic(&self, device: &Path) -> Result<bool, Error> {
        let path = (self.canonical)(device)?;
        let platform = (self.canonical)(&self.root.join("sys/devices/platform"))?;
        let Ok(relative) = path.strip_prefix(platform) else {
            return Ok(false);
        };
        let mut parts = relative.components();
        let controller = parts.next().ok_or_else(invalid)?;
        let controller = name(controller.as_os_str())?;
        let Some(index) = controller.strip_prefix("vhci_hcd.") else {
            return Ok(false);
        };
        numeric(index)?;
        let usb = parts.next().ok_or_else(invalid)?;
        let usb = name(usb.as_os_str())?;
        let Some(index) = usb.strip_prefix("usb") else {
            return Ok(false);
        };
        numeric(index)?;
        let serial = read(&path.join("serial"))?;
        if serial != SERIAL && serial != "TETHERCAM-ECHO-0001" {
            return Ok(false);
        }
        let vendor = read(&path.join("idVendor"))?;
        let product = read(&path.join("idProduct"))?;
        if !matches!(
            (serial.as_str(), vendor.as_str(), product.as_str()),
            ("TETHERCAM-USBIP-0001", "054c", "0c34" | "0c03")
                | ("TETHERCAM-ECHO-0001", "ffff", "0001")
        ) {
            return Err(invalid());
        }
        Ok(true)
    }

    fn attach(&self) -> Result<(), Error> {
        self.usbip(&["attach", "-r", "127.0.0.1", "-b", "1-1"])?;
        for _ in 0..100 {
            for device in (self.entries)(&self.root.join("sys/bus/usb/devices"))? {
                if !device.join("serial").is_file() || !self.synthetic(&device)? {
                    continue;
                }
                let bus = numeric(&read(&device.join("busnum"))?)?;
                let dev = numeric(&read(&device.join("devnum"))?)?;
                if bus == 0 || dev == 0 {
                    return Err(invalid());
                }
                let node = self.root.join(format!("dev/bus/usb/{bus:03}/{dev:03}"));
                if !(self.character)(&node)? {
                    continue;
                }
                if !self.privileged {
                    fs::OpenOptions::new().read(true).write(true).open(node)?;
                    return Ok(());
                }
                let uid = self.call("/usr/bin/id", &["-u"])?;
                let gid = self.call("/usr/bin/id", &["-g"])?;
                // Numeric identity is validated without the USB bus's u16 range restriction.
                let uid = identity(&uid)?;
                let gid = identity(&gid)?;
                if uid == 0 {
                    return Err(invalid());
                }
                let args = vec![
                    OsString::from("--"),
                    OsString::from("/usr/bin/chown"),
                    OsString::from(format!("{uid}:{gid}")),
                    node.into_os_string(),
                ];
                (self.exec)(Path::new("/usr/bin/sudo"), &args)?;
                return Ok(());
            }
            (self.pause)(Duration::from_millis(100));
        }
        Err(Error::Invalid("synthetic USB/IP device did not enumerate"))
    }

    fn detach(&self) -> Result<(), Error> {
        for controller in (self.entries)(&self.root.join("sys/devices/platform"))? {
            let name = name(controller.file_name().unwrap_or_default())?;
            let Some(index) = name.strip_prefix("vhci_hcd.") else {
                continue;
            };
            numeric(index)?;
            for status in (self.entries)(&controller)? {
                if !status
                    .file_name()
                    .and_then(OsStr::to_str)
                    .is_some_and(|s| s.starts_with("status"))
                {
                    continue;
                }
                for line in fs::read_to_string(status)?.lines().skip(1) {
                    let fields: Vec<_> = line.split_whitespace().collect();
                    let [_, port, _, _, _, _, id, ..] = fields.as_slice() else {
                        return Err(invalid());
                    };
                    // Disconnected slots use 0-0; no sysfs entry is expected for those slots.
                    busid(id)?;
                    numeric(port)?;
                    let device = self.root.join("sys/bus/usb/devices").join(id);
                    if !device.join("serial").is_file() || !self.synthetic(&device)? {
                        continue;
                    }
                    self.usbip(&["detach", "-p", port])?;
                }
            }
        }
        Ok(())
    }

    fn action(&self, action: &OsStr) -> Result<(), Error> {
        match action.to_str() {
            Some("prepare") if self.privileged => {
                self.call(
                    "/usr/bin/sudo",
                    &["--", "/usr/bin/apt-get", "update", "-qq"],
                )?;
                self.call(
                    "/usr/bin/sudo",
                    &[
                        "--",
                        "/usr/bin/apt-get",
                        "install",
                        "-y",
                        "linux-tools-generic",
                    ],
                )?;
                self.call("/usr/bin/sudo", &["--", "/usr/sbin/modprobe", "vhci_hcd"])?;
                fs::read(self.root.join("sys/devices/platform/vhci_hcd.0/status"))?;
                (self.exec)(&self.tool()?, &argv(&["version"]))?;
                Ok(())
            }
            Some("attach") => self.attach(),
            Some("detach") => self.detach(),
            _ => Err(Error::Usage),
        }
    }
}

fn identity(text: &str) -> Result<u32, Error> {
    let text = text.trim_end_matches('\n');
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid());
    }
    text.parse().map_err(|_| invalid())
}

#[cfg(unix)]
fn character(path: &Path) -> Result<bool, Error> {
    use std::os::unix::fs::FileTypeExt;
    Ok(fs::symlink_metadata(path)?.file_type().is_char_device())
}

#[cfg(not(unix))]
fn character(_path: &Path) -> Result<bool, Error> {
    Err(invalid())
}

pub(super) fn run(action: &OsStr) -> Result<(), Error> {
    execute(
        action,
        authorize(&mut |key| std::env::var_os(key), cfg!(target_os = "linux")),
        &Host {
            root: Path::new("/"),
            privileged: true,
            canonical,
            entries,
            exec: command,
            pause: std::thread::sleep,
            character,
        },
    )
}

/// Local execution never escalates or provisions. The operator must delegate both VHCI
/// controls and the synthetic node; merely loading the module is insufficient.
fn authorize_local(ci_present: bool, enabled: Option<&OsStr>, linux: bool) -> Result<(), Error> {
    if !linux || ci_present || enabled != Some(OsStr::new("1")) {
        return Err(invalid());
    }
    Ok(())
}

pub(super) fn local(action: &OsStr) -> Result<(), Error> {
    execute(
        action,
        authorize_local(
            std::env::var_os("CI").is_some(),
            std::env::var_os("TETHERCAM_USBIP").as_deref(),
            cfg!(target_os = "linux"),
        ),
        &Host {
            root: Path::new("/"),
            privileged: false,
            canonical,
            entries,
            exec: command,
            pause: std::thread::sleep,
            character,
        },
    )
}

fn entries(path: &Path) -> std::io::Result<Vec<PathBuf>> {
    fs::read_dir(path)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect()
}

fn execute(action: &OsStr, permission: Result<(), Error>, host: &Host<'_>) -> Result<(), Error> {
    permission?;
    if identity(&host.call("/usr/bin/id", &["-u"])?)? == 0 {
        return Err(invalid());
    }
    host.action(action)
}

#[cfg(test)]
mod tests;

# Native services

Service operations are explicit. `setup`, `probe`, `doctor` and foreground `run` never install,
start or enable a daemon. Installation starts the service and enables subsequent automatic
starts. It can open the selected camera and authenticated listeners: invoke it only when ready.

```sh
# macOS, from a logged-in GUI session:
tethercam service install
tethercam service status
tethercam service uninstall

# Linux system manager, using an administrator-installed binary:
sudo /usr/local/bin/tethercam service install
sudo /usr/local/bin/tethercam service status
sudo /usr/local/bin/tethercam service uninstall

# Linux user manager, while its user bus is available:
tethercam service install --user
tethercam service status --user
tethercam service uninstall --user
```

`service --help` describes syntax. Runtime flags (`--config`, `--simulate`, `--no-discovery`)
are intentionally not service flags. The installed command is the absolute currently running
executable followed by `run --config` and the fixed standard config path. Move a Linux system
binary into `/usr` or `/opt` before installing; a Cargo build in a home directory is deliberately
rejected, because `ProtectHome=yes` hides it. Keep the executable and its parents owned by root
or the installing user, without group/other write permission. Executable validation happens before
any service file or account is published: the eventual identity needs traversal on every ancestor
and read/execute access to the binary. System installs conservatively require other traversal and
other read/execute (normally `0755`); they do not guess the not-yet-created account's UID/groups.
User installs accept owner access for caller-owned paths and otherwise require other access.
Group-only access is intentionally not inferred. A root-owned `0700` binary or ancestor is
therefore rejected for a non-root system service rather than installing an unusable unit.
Reinstall after uninstall if the
binary path changes. A changed template/path is a conflict, not permission to overwrite a unit.

| Scope | Files |
| --- | --- |
| macOS user | `~/Library/LaunchAgents/net.tethercam.agent.plist`, config `~/Library/Application Support/tethercam/config.toml`, `~/Library/Logs/tethercam/stdout.log` and `stderr.log` |
| Linux user | `~/.config/systemd/user/tethercam.service`, config `~/.config/tethercam/config.toml` |
| Linux system | `/etc/systemd/system/tethercam.service`, `/etc/sysusers.d/tethercam.conf`, `/etc/udev/rules.d/70-tethercam.rules`, config `/var/lib/tethercam/config.toml` |

These fixed paths intentionally do not follow `XDG_CONFIG_HOME`. If foreground setup used a
custom path, it is a separate config, not silently copied into a daemon. To reuse an identity,
prepare the service's documented config path with `setup --config PATH` first, or move the
protected file there while stopped. Installing loads it or generates credentials and identity
through the same `Config::load_or_create` implementation as foreground setup. Installation and
status never print credentials. For adoption, use `setup --config PATH` privately as the file's
owner, e.g. `sudo -u tethercam /usr/local/bin/tethercam setup --config /var/lib/tethercam/config.toml`
for a system service. Never put credentials in command arguments, environment or unit files.

Files are atomically published without clobbering existing contents and directory entries are
synced. New directories are `0700`, files `0600`. Existing unit/rule/sysusers contents must match
exactly, with secure ownership, permissions and no symlinks or hardlinks. Existing ancestors
must be real directories, owned by root or the expected user and not group/other writable.
Symlinked home/install paths are rejected; choose a real canonical home/binary location.
Privileged config ownership changes walk from the trusted root through no-follow directory
handles, open the final file without following symlinks, validate that opened inode's type,
owner, permissions and single-link policy, and apply `fchown` to the same handle. Directory
ownership uses the same descriptor-relative validation. The service account can replace its
config entry during reinstall, but cannot redirect root's ownership change to another target.
Run lifecycle commands serially; concurrent administrative lifecycle operations are not coordinated.

`install` is repeatable: matching files, credentials and UUID are preserved; systemd reloads and
idempotently enables/starts the unit; launchd bootstraps only an unregistered agent and kickstarts
a registered non-running agent without killing a running one. A failed step leaves matching files
for a repeat invocation, not a half-rollback that loses an identity. systemd executable paths do not use environment-dollar expansion; subsequent arguments escape
literal dollars as `$$`. systemd itself rejects quotes and backslashes in executable names even
after unquoting, so Linux service installation rejects such binary paths before writing anything.
Spaces, dollars and percent signs in executables are supported. Config arguments and launchd
paths additionally support escaped quotes/backslashes. Native command output is never echoed, and subprocess environments retain only the user-bus locator variables, a fixed
`PATH` and `LC_ALL=C`, not arbitrary inherited tokens or loader settings.

`status` never mutates anything. It distinguishes `absent`, `installed, not loaded`, `inactive`,
`active` and `failed`. Unrelated/insecure files and manager/permission errors are typed errors,
not reported as absence. systemd uses `show`'s machine property keys; launchd checks the GUI
domain and uses `list <label>`'s property dictionary scalar `PID` / `LastExitStatus`, not its
human summary table. A running PID takes precedence over a previous exit failure; a stopped
process with nonzero exit status is failed. Unknown/malformed native replies fail closed.
State is an observation, not a readiness probe or a guarantee of future process health.

`uninstall` stops and unloads/disables before removing matching generated files. A failed stop
never removes them. After a successful stop, a protected `<unit-stem>.uninstall` receipt beside
the unit preserves its exact matching template until generated-file removals, directory syncs,
daemon reload and udev reload have all completed. Retry `uninstall` after any failure, including
an unlink that succeeded before its sync failed. A matching receipt authorizes cleanup of the
cached inactive manager job even after the unit file is gone; an unrelated registered job or
unrelated/insecure receipt is still rejected. `install` and `status` reject a pending receipt:
finish `uninstall` first. Receipt removal happens only after the manager has forgotten the unit,
so an interruption in that final directory sync can also be retried. It retains config, credentials, logs, and the Linux
system account/group: deleting an account or another application's data is not uninstall's job.
The Linux system scope requires root for all three operations, including reading protected files.

## Platform behavior and limits

macOS uses `gui/<uid>`'s per-user launchd agent, `KeepAlive`, `RunAtLoad`, a ten-second restart
throttle, an owner-only umask, and separate stdout/stderr files under `~/Library/Logs`. No root
LaunchDaemon is installed. A GUI login/domain is required; an unavailable domain is an error.
Logs are not rotated by this installer: arrange normal host log rotation or remove old logs
while stopped. Reinstallation checks existing logs only through protected metadata, never by
reading or truncating their contents. Unit/rule/control-file comparisons use a bounded reader
and reject files above 65,535 bytes; a growing or unrelated file cannot cause an unbounded read. The foreground runtime logs only sanitized lifecycle categories.

Linux requires systemd with `/usr/bin/systemctl`, `/usr/bin/systemd-sysusers`, `/usr/bin/getent`,
`/usr/bin/id` and `/usr/bin/udevadm` for system installation. The dedicated `tethercam` account
has `/var/lib/tethercam` as its home and `/usr/sbin/nologin` as its shell. A preexisting account
with incompatible identity/home/shell is rejected. The system config directory and file are
owned by that account/group, not made world-readable. System units use `Restart=on-failure`,
`NoNewPrivileges=yes`, `ProtectSystem=strict`, `ProtectHome=yes`, an owner-only umask, and the
state directory as the sole explicit writable path. `DevicePolicy=closed` plus
`DeviceAllow=char-usb_device rw` permits USB character devices across hotplug bus/device numbers;
there is no `PrivateDevices=yes`, which would hide the camera. The cgroup allowance is USB-wide,
not a pretend stable per-camera device number; udev permissions narrow access to Sony's USB
vendor `054c`. No privileged capability is granted. Linux logs use the journal.

The udev rule gives `tethercam` group `0660` USB access and adds `uaccess` for a local active
session. Rules are reloaded, but existing devices are not retriggered: reconnect the camera
after initial rule installation. This grants access to Sony USB devices, not arbitrary vendors;
USB permissions are not a defense against a device spoofing vendor IDs. Check any host-specific
udev overrides when troubleshooting. The sysusers group is not used to add arbitrary login users.

A user manager cannot install a global udev rule or create an account without administrator
privileges. `--user` therefore only installs its user unit; an administrator must first provision
USB access (the system installer supplies the rule), or the active desktop session must already
have suitable permissions. Headless users may need administrator-managed group membership and
login renewal. The user unit keeps `NoNewPrivileges` and restart policy but omits `ProtectSystem`,
`ProtectHome` and device-cgroup directives: user namespaces/device-controller delegation are not
universally available, and emitting mandatory inaccessible namespaces would prevent startup.
No privilege escalation, global rule installation, lingering change or bus creation is attempted
by a user install. User services normally stop on logout unless an administrator enables lingering.

## Starting without a camera

The installed service starts, and stays up, whether or not the camera is connected, awake or in PC
Remote mode. RTSP and ONVIF HTTP bind at startup (so a busy port fails the unit at once), answer
`503` until the first live-view frame defines the stream, and the authenticated
`GET /tethercam/status` reports `waiting` or `ready` with battery and power. WS-Discovery starts
announcing when the camera is ready. Unplugging or power-cycling the camera returns the service to
`waiting` without a restart; a camera that returns with a different live-view size ends the run,
and the manager's restart policy picks up the new size. See [`architecture.md`](architecture.md#waiting-for-the-camera).

## Shutdown and validation

Stopping a service sends `SIGTERM` and waits for joined runtime cleanup. systemd uses
`TimeoutStopSec=infinity`; launchd uses `ExitTimeOut=0` (unlimited). With full-resolution snapshots enabled, a healthy five-minute still
download can finish, shutter buttons can be released, the PTP session can close, and network/CPU
workers can join without a manager's short default timeout sending `SIGKILL`. This intentionally
means a wedged process can delay stop, uninstall or host shutdown indefinitely. An operator may
force-kill a genuinely wedged process, accepting the camera hygiene consequences. External host
shutdown policies can still impose their own limits; the installer does not modify them.

Tests run the real isolated-file lifecycle with injected native argv/replies and concrete syscall
failure points, verify persisted identity and secret ownership, and cover file/directory swaps
before and after descriptor opening, hardlinks, interrupted cleanup with cached manager state,
and process failures and
malformed replies. Property tests cover arbitrary OS-path bytes and native manager replies.
macOS validates generated temporary plists with `plutil -lint`; hosted Linux validates both
system and user units with `systemd-analyze verify`. No test installs, starts, enables or reloads
an actual daemon, account or udev rule on the developer's host, and no service test enumerates USB.

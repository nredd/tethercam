//! Template, scope and protected-file tests; no daemon or device operations.
//!
//! References: docs/service.md.

use super::*;
use proptest::prelude::*;
use std::fs;
use std::os::unix::{
    ffi::OsStringExt,
    fs::{MetadataExt, PermissionsExt, symlink},
};

fn files(temp: &tempfile::TempDir) -> files::Files {
    files::Files::new(temp.path().into(), fs::metadata(temp.path()).unwrap().uid())
}

#[test]
fn scopes_and_standard_paths() {
    for (platform, user, uid, suffix) in [
        (
            Platform::Mac,
            false,
            501,
            "Library/LaunchAgents/net.tethercam.agent.plist",
        ),
        (
            Platform::Linux,
            true,
            501,
            ".config/systemd/user/tethercam.service",
        ),
        (
            Platform::Linux,
            false,
            0,
            "/etc/systemd/system/tethercam.service",
        ),
    ] {
        let plan = Plan::new(
            platform,
            user,
            uid,
            Path::new("/synthetic/home"),
            Path::new("/usr/local/bin/tethercam"),
        )
        .unwrap();
        assert!(plan.unit().ends_with(suffix));
        assert_eq!(plan.files.len(), if plan.user { 1 } else { 3 });
    }
    assert!(
        Plan::new(
            Platform::Linux,
            false,
            501,
            Path::new("/home/test"),
            Path::new("/bin/test")
        )
        .is_err()
    );
    for path in [
        PathBuf::from("relative"),
        PathBuf::from("/a/../b"),
        PathBuf::from("/bad\npath"),
        PathBuf::from("/bad\u{fffe}"),
        PathBuf::from("/bad\u{ffff}"),
        PathBuf::from(std::ffi::OsString::from_vec(vec![b'/', 0xff])),
    ] {
        assert!(Plan::new(Platform::Mac, true, 501, &path, Path::new("/bin/test")).is_err());
        assert!(Plan::new(Platform::Mac, true, 501, Path::new("/home/test"), &path).is_err());
    }
    assert!(
        Plan::new(
            Platform::Linux,
            false,
            0,
            Path::new("/synthetic/home"),
            Path::new("/usr/bad\npath")
        )
        .is_err()
    );
    assert!(
        Plan::new(
            Platform::Linux,
            false,
            0,
            Path::new("/synthetic/home"),
            Path::new("/home/test/tethercam")
        )
        .is_err()
    );
    assert!(
        Plan::new(
            Platform::Linux,
            false,
            0,
            Path::new("/synthetic/home"),
            Path::new("/opt/tethercam/bin/tethercam")
        )
        .is_ok()
    );
}

#[test]
fn templates_escape_each_language_not_a_shell() {
    let binary = Path::new("/opt/a &<>\"' %$\\ tool");
    let config = Path::new("/synthetic/a &<>\"' %$\\ config");
    let logs = Path::new("/synthetic/logs");
    let plist = templates::launchd(binary, config, logs).unwrap();
    let doc = roxmltree::Document::parse_with_options(
        &plist,
        roxmltree::ParsingOptions {
            allow_dtd: true,
            ..Default::default()
        },
    )
    .unwrap();
    let strings: Vec<_> = doc
        .descendants()
        .filter(|node| node.has_tag_name("string"))
        .filter_map(|node| node.text())
        .collect();
    assert!(strings.contains(&binary.to_str().unwrap()));
    assert!(strings.contains(&config.to_str().unwrap()));
    assert!(plist.contains("<key>ExitTimeOut</key><integer>0</integer>"));
    let executable = Path::new("/opt/a &<> %$ tool");
    let unit = templates::systemd(executable, config, false).unwrap();
    assert!(unit.contains(
        "ExecStart=\"/opt/a &<> %%$ tool\" run --config \"/synthetic/a &<>\\\"' %%$$\\\\ config\""
    ));
    for unsupported in ["/opt/a\"tool", "/opt/a'tool", "/opt/a\\tool"] {
        assert!(templates::systemd(Path::new(unsupported), config, false).is_err());
    }
    for required in [
        "User=tethercam",
        "Group=tethercam",
        "ProtectSystem=strict",
        "DeviceAllow=char-usb_device rw",
        "TimeoutStopSec=infinity",
        "Restart=on-failure",
        "NoNewPrivileges=yes",
    ] {
        assert!(unit.contains(required));
    }
    let user = templates::systemd(executable, config, true).unwrap();
    for unsupported in ["ProtectSystem=", "DeviceAllow=", "User=", "Group="] {
        assert!(!user.contains(unsupported));
    }
    assert!(user.contains("WantedBy=default.target"));
    assert!(templates::launchd(Path::new("relative"), config, logs).is_err());
    assert!(templates::launchd(binary, Path::new("relative"), logs).is_err());
    assert!(templates::launchd(binary, config, Path::new("relative")).is_err());
    assert!(templates::systemd(Path::new("relative"), config, true).is_err());
    assert!(templates::systemd(executable, Path::new("relative"), true).is_err());
}

#[test]
fn native_template_validator() {
    let temp = tempfile::tempdir().unwrap();
    #[cfg(target_os = "macos")]
    {
        let path = temp.path().join("agent.plist");
        fs::write(
            &path,
            templates::launchd(
                Path::new("/usr/bin/true"),
                Path::new("/synthetic/config"),
                Path::new("/synthetic/logs"),
            )
            .unwrap(),
        )
        .unwrap();
        let result = std::process::Command::new("/usr/bin/plutil")
            .arg("-lint")
            .arg(&path)
            .output()
            .unwrap();
        let diagnostic = String::from_utf8_lossy(&result.stderr);
        assert!(result.status.success(), "{diagnostic}");
    }
    #[cfg(target_os = "linux")]
    {
        // Verify the actual first-token path against an existing executable. Unlike later
        // argv, systemd does not environment-expand this token, so `$` must stay single.
        let executable = temp.path().join("native $ & % tool");
        fs::copy("/usr/bin/true", &executable).unwrap();
        for user in [false, true] {
            let path = temp.path().join("tethercam.service");
            fs::write(
                &path,
                templates::systemd(
                    &executable,
                    Path::new("/synthetic/config $ &\"' %\\ file"),
                    user,
                )
                .unwrap(),
            )
            .unwrap();
            let result = std::process::Command::new("/usr/bin/systemd-analyze")
                .args(["verify", "--man=no"])
                .arg(&path)
                .output()
                .unwrap();
            let diagnostic = String::from_utf8_lossy(&result.stderr);
            assert!(result.status.success(), "{diagnostic}");
        }
    }
}

#[test]
fn file_lifecycle_preserves_identity_and_unrelated_files() {
    let temp = tempfile::tempdir().unwrap();
    let files = files(&temp);
    let unit = Path::new("/standard/deep/unit");
    assert_eq!(files.read(unit).unwrap(), None);
    files.publish(unit, b"generated").unwrap();
    files.publish(unit, b"generated").unwrap();
    assert_eq!(files.read(unit).unwrap().unwrap(), b"generated");
    assert!(files.publish(unit, b"changed").is_err());
    assert!(files.remove(unit, b"unrelated").is_err());
    files.remove(unit, b"generated").unwrap();
    files.remove(unit, b"generated").unwrap();
    files.log(unit).unwrap();
    fs::write(files.resolve(unit), b"retained log").unwrap();
    files.log(unit).unwrap();
    assert_eq!(files.read(unit).unwrap().unwrap(), b"retained log");
    let cfg = Path::new("/standard/credentials/config.toml");
    files.protect_config(cfg).unwrap();
    let first = crate::config::Config::load(&files.resolve(cfg)).unwrap();
    files.protect_config(cfg).unwrap();
    let second = crate::config::Config::load(&files.resolve(cfg)).unwrap();
    assert_eq!(first.endpoint_uuid, second.endpoint_uuid);
    assert!(
        first
            .credentials
            .password
            .constant_time_eq(&second.credentials.password)
    );
    let meta = fs::metadata(files.resolve(cfg)).unwrap();
    files.chown(cfg, meta.uid(), meta.gid(), false).unwrap();
    assert_eq!(meta.permissions().mode() & 0o077, 0);
}

#[test]
fn files_fail_closed_on_unsafe_paths_and_io() {
    let temp = tempfile::tempdir().unwrap();
    let files = files(&temp);
    let path = Path::new("/data/file");
    files.publish(path, b"unit").unwrap();
    fs::set_permissions(files.resolve(path), fs::Permissions::from_mode(0o666)).unwrap();
    assert!(files.read(path).is_err());
    fs::set_permissions(files.resolve(path), fs::Permissions::from_mode(0o600)).unwrap();
    let hard = files.resolve(Path::new("/data/hard"));
    fs::hard_link(files.resolve(path), &hard).unwrap();
    assert!(files.read(path).is_err());
    fs::remove_file(hard).unwrap();
    let link = Path::new("/data/link");
    symlink(files.resolve(path), files.resolve(link)).unwrap();
    assert!(files.read(link).is_err());
    assert!(files.directory(link, true).is_err());
    assert!(files.directory(path, true).is_err());
    assert!(files.read(Path::new("/data")).is_err());
    assert!(files.directory(Path::new("relative"), true).is_err());
    assert!(files::parent(Path::new("/")).is_err());
    assert!(files.read(Path::new("/data/file/child")).is_err());
    assert!(files.protect_config(Path::new("/data/file/child")).is_err());
    assert!(files.protect_config(path).is_err());
    assert!(files.chown(Path::new("/missing"), 0, 0, false).is_err());
    assert!(files.executable(Path::new("/missing"), false).is_err());
    assert!(files.executable(Path::new("/data"), false).is_err());
    assert!(files.executable(path, false).is_err());
    fs::set_permissions(files.resolve(path), fs::Permissions::from_mode(0o700)).unwrap();
    files.executable(path, false).unwrap();
    fs::set_permissions(
        files.resolve(Path::new("/data")),
        fs::Permissions::from_mode(0o777),
    )
    .unwrap();
    assert!(files.directory(Path::new("/data"), false).is_err());
}

proptest! {
    #[test]
    fn arbitrary_path_bytes_are_total(bytes in proptest::collection::vec(any::<u8>(), 0..256)) {
        let value = PathBuf::from(std::ffi::OsString::from_vec(bytes));
        let _ = templates::path(&value);
        let _ = templates::launchd(&value, &value, &value);
        let _ = templates::systemd(&value, &value, true);
    }
}

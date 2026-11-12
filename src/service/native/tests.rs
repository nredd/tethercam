//! Isolated lifecycle execution with explicit fake native argv/replies.
//!
//! References: docs/service.md.

use super::*;
use proptest::prelude::*;
use std::collections::VecDeque;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

fn reply(code: i32, text: &str) -> Reply {
    Reply {
        code: Some(code),
        stdout: text.as_bytes().to_vec(),
    }
}
fn io_error() -> io::Error {
    io::Error::other("synthetic failure")
}

fn unavailable(_: &str, _: &[OsString]) -> io::Result<Reply> {
    Err(io_error())
}

fn chown_failure(_: &std::fs::File, _: Option<u32>, _: Option<u32>) -> io::Result<()> {
    Err(io_error())
}

struct Script {
    expected: VecDeque<(String, Vec<OsString>, io::Result<Reply>)>,
}
impl Script {
    fn new() -> Self {
        Self {
            expected: VecDeque::new(),
        }
    }
    fn add(&mut self, program: &str, args: &[&str], result: io::Result<Reply>) {
        if program == "/bin/launchctl" && args.first() == Some(&"print") {
            let target = args.get(1).unwrap();
            let domain = target.rsplit_once('/').unwrap().0;
            self.expected.push_back((
                program.into(),
                vec!["print".into(), domain.into()],
                Ok(reply(0, "")),
            ));
            let result = result.map(|value| {
                if value.code == Some(0) {
                    reply(0, "{\n\t\"PID\" = 123;\n\t\"LastExitStatus\" = 0;\n}\n")
                } else {
                    value
                }
            });
            self.expected.push_back((
                program.into(),
                vec!["list".into(), "net.tethercam.agent".into()],
                result,
            ));
            return;
        }
        self.expected.push_back((
            program.into(),
            args.iter().map(OsString::from).collect(),
            result,
        ));
    }
    fn run(&mut self, program: &str, args: &[OsString]) -> io::Result<Reply> {
        let (want_program, want_args, result) = self.expected.pop_front().unwrap();
        assert_eq!(program, want_program);
        assert_eq!(args, want_args);
        result
    }
    fn done(&self) {
        assert!(self.expected.is_empty());
    }
}

fn fixture(platform: Platform, user: bool) -> (tempfile::TempDir, Plan, Files) {
    let temp = tempfile::tempdir().unwrap();
    let uid = fs::metadata(temp.path()).unwrap().uid();
    let plan = Plan::new(
        platform,
        user,
        if user { uid } else { 0 },
        Path::new("/synthetic/home"),
        Path::new("/usr/local/bin/tethercam"),
    )
    .unwrap();
    let fs = Files::new(temp.path().into(), uid);
    fs.publish(&plan.binary, b"synthetic executable").unwrap();
    fs::set_permissions(
        fs.resolve(&plan.binary),
        fs::Permissions::from_mode(if user { 0o700 } else { 0o755 }),
    )
    .unwrap();
    if !user {
        for directory in ["/", "/usr", "/usr/local", "/usr/local/bin"] {
            fs::set_permissions(
                fs.resolve(Path::new(directory)),
                fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
    }
    (temp, plan, fs)
}
fn show(script: &mut Script, user: bool, load: &str, active: &str) {
    let mut args = vec![];
    if user {
        args.push("--user");
    }
    args.extend([
        "show",
        "tethercam.service",
        "--property=LoadState,ActiveState",
        "--no-pager",
    ]);
    script.add(
        "/usr/bin/systemctl",
        &args,
        Ok(reply(
            0,
            &format!("LoadState={load}\nActiveState={active}\n"),
        )),
    );
}
fn publish(plan: &Plan, fs: &Files) {
    for (path, value) in &plan.files {
        fs.publish(path, value.as_bytes()).unwrap();
    }
}

#[test]
fn linux_user_real_file_lifecycle_and_exact_native_argv() {
    let (_temp, plan, fs) = fixture(Platform::Linux, true);
    let mut script = Script::new();
    show(&mut script, true, "not-found", "inactive");
    script.add(
        "/usr/bin/systemctl",
        &["--user", "daemon-reload"],
        Ok(reply(0, "")),
    );
    script.add(
        "/usr/bin/systemctl",
        &["--user", "enable", "--now", "tethercam.service"],
        Ok(reply(0, "")),
    );
    show(&mut script, true, "loaded", "active");
    assert_eq!(
        lifecycle(&plan, Action::Install, &fs, &mut |p, a| script.run(p, a)).unwrap(),
        Status::Active
    );
    let id = crate::config::Config::load(&fs.resolve(&plan.config))
        .unwrap()
        .endpoint_uuid;
    show(&mut script, true, "loaded", "failed");
    assert_eq!(
        lifecycle(&plan, Action::Status, &fs, &mut |p, a| script.run(p, a)).unwrap(),
        Status::Failed
    );
    show(&mut script, true, "loaded", "active");
    script.add(
        "/usr/bin/systemctl",
        &["--user", "daemon-reload"],
        Ok(reply(0, "")),
    );
    script.add(
        "/usr/bin/systemctl",
        &["--user", "enable", "--now", "tethercam.service"],
        Ok(reply(0, "")),
    );
    show(&mut script, true, "loaded", "active");
    assert_eq!(
        lifecycle(&plan, Action::Install, &fs, &mut |p, a| script.run(p, a)).unwrap(),
        Status::Active
    );
    assert_eq!(
        crate::config::Config::load(&fs.resolve(&plan.config))
            .unwrap()
            .endpoint_uuid,
        id
    );
    show(&mut script, true, "loaded", "active");
    script.add(
        "/usr/bin/systemctl",
        &["--user", "disable", "--now", "tethercam.service"],
        Ok(reply(0, "")),
    );
    script.add(
        "/usr/bin/systemctl",
        &["--user", "daemon-reload"],
        Ok(reply(0, "")),
    );
    assert_eq!(
        lifecycle(&plan, Action::Uninstall, &fs, &mut |p, a| script.run(p, a)).unwrap(),
        Status::Absent
    );
    show(&mut script, true, "not-found", "inactive");
    script.add(
        "/usr/bin/systemctl",
        &["--user", "daemon-reload"],
        Ok(reply(0, "")),
    );
    assert_eq!(
        lifecycle(&plan, Action::Uninstall, &fs, &mut |p, a| script.run(p, a)).unwrap(),
        Status::Absent
    );
    assert!(fs.resolve(&plan.config).is_file());
    script.done();
}

#[test]
fn uninstall_retry_repairs_unlinked_unit_with_cached_manager_state() {
    let (_temp, plan, mut fs) = fixture(Platform::Linux, true);
    publish(&plan, &fs);
    let original = fs.calls;
    fs.calls.sync_directory = |path| {
        if path.file_name() == Some(std::ffi::OsStr::new("user"))
            && !path.join("tethercam.service").exists()
        {
            Err(io_error())
        } else {
            std::fs::File::open(path).unwrap().sync_all()
        }
    };
    let mut runner = |_: &str, args: &[OsString]| {
        Ok(reply(
            0,
            if args.iter().any(|arg| arg == "show") {
                "LoadState=loaded\nActiveState=inactive\n"
            } else {
                ""
            },
        ))
    };
    assert!(lifecycle(&plan, Action::Uninstall, &fs, &mut runner).is_err());
    assert!(!fs.resolve(plan.unit()).exists());
    fs.calls = original;
    assert_eq!(
        lifecycle(&plan, Action::Uninstall, &fs, &mut runner).unwrap(),
        Status::Absent
    );
    // No owned evidence means an arbitrary cached manager job must still be rejected.
    assert_eq!(
        std::mem::discriminant(&lifecycle(&plan, Action::Uninstall, &fs, &mut runner).unwrap_err()),
        std::mem::discriminant(&Error::Conflict)
    );
}

fn cached_reply(cached: bool) -> Reply {
    reply(
        0,
        if cached {
            "LoadState=loaded\nActiveState=inactive\n"
        } else {
            "LoadState=not-found\nActiveState=inactive\n"
        },
    )
}

#[test]
fn system_uninstall_keeps_owned_receipt_until_every_cleanup_step_finishes() {
    for step in 0..5 {
        let (_temp, plan, mut fs) = fixture(Platform::Linux, false);
        publish(&plan, &fs);
        fs.protect_config(&plan.config).unwrap();
        let identity = crate::config::Config::load(&fs.resolve(&plan.config))
            .unwrap()
            .endpoint_uuid;
        let calls = fs.calls;
        if step == 0 {
            fs.calls.sync_directory = |path| {
                if path.ends_with("systemd/system") && !path.join("tethercam.service").exists() {
                    Err(io_error())
                } else {
                    std::fs::File::open(path).unwrap().sync_all()
                }
            };
        } else if step == 1 {
            fs.calls.remove = |path| {
                if path.ends_with("sysusers.d/tethercam.conf") {
                    Err(io_error())
                } else {
                    fs::remove_file(path)
                }
            };
        } else if step == 4 {
            fs.calls.sync_directory = |path| {
                if path.ends_with("systemd/system")
                    && !path.join("tethercam.service").exists()
                    && !path.join("tethercam.uninstall").exists()
                {
                    Err(io_error())
                } else {
                    std::fs::File::open(path).unwrap().sync_all()
                }
            };
        }
        let mut cached = true;
        let mut reloaded = false;
        let mut runner = |program: &str, args: &[OsString]| {
            let command = args.first().unwrap();
            if command == "show" {
                return Ok(cached_reply(cached));
            }
            if command == "daemon-reload" {
                if step == 2 {
                    return Err(io_error());
                }
                cached = false;
                reloaded = true;
            }
            if program == "/usr/bin/udevadm" && step == 3 {
                return Err(io_error());
            }
            Ok(reply(0, ""))
        };
        assert!(lifecycle(&plan, Action::Uninstall, &fs, &mut runner).is_err());
        assert!(!fs.resolve(plan.unit()).exists());
        let receipt = plan.unit().with_extension("uninstall");
        assert_eq!(fs.resolve(&receipt).exists(), step != 4);
        assert_eq!(reloaded, step >= 3);
        fs.calls = calls;
        let mut reloads = 0;
        assert_eq!(
            lifecycle(&plan, Action::Uninstall, &fs, &mut |program, args| {
                if args.first() == Some(&OsString::from("show")) {
                    Ok(cached_reply(cached))
                } else {
                    if program == "/usr/bin/systemctl" {
                        assert_eq!(args.first(), Some(&OsString::from("daemon-reload")));
                        reloads += 1;
                    }
                    Ok(reply(0, ""))
                }
            })
            .unwrap(),
            Status::Absent
        );
        assert_eq!(reloads, 1);
        for (path, _) in &plan.files {
            assert!(!fs.resolve(path).exists());
        }
        assert!(!fs.resolve(receipt.as_path()).exists());
        assert_eq!(
            crate::config::Config::load(&fs.resolve(&plan.config))
                .unwrap()
                .endpoint_uuid,
            identity
        );
    }
}

#[test]
fn uninstall_receipt_must_be_durable_before_generated_files_are_unlinked() {
    let (_temp, plan, mut fs) = fixture(Platform::Linux, true);
    publish(&plan, &fs);
    fs.calls.sync_directory = |_| Err(io_error());
    assert!(
        lifecycle(&plan, Action::Uninstall, &fs, &mut |_, _| Ok(cached_reply(
            true
        )))
        .is_err()
    );
    assert!(fs.resolve(plan.unit()).exists());
}

#[test]
fn unrelated_or_insecure_uninstall_receipts_never_authorize_manager_mutation() {
    for insecure in [false, true] {
        let (_temp, plan, fs) = fixture(Platform::Linux, true);
        let receipt = plan.unit().with_extension("uninstall");
        fs.publish(
            &receipt,
            if insecure {
                plan.files.first().unwrap().1.as_bytes()
            } else {
                b"unrelated"
            },
        )
        .unwrap();
        if insecure {
            fs::set_permissions(fs.resolve(&receipt), fs::Permissions::from_mode(0o666)).unwrap();
        }
        assert_eq!(
            std::mem::discriminant(
                &lifecycle(&plan, Action::Uninstall, &fs, &mut unavailable).unwrap_err()
            ),
            std::mem::discriminant(&Error::Conflict)
        );
    }
    let (_temp, plan, fs) = fixture(Platform::Linux, true);
    fs.publish(
        &plan.unit().with_extension("uninstall"),
        plan.files.first().unwrap().1.as_bytes(),
    )
    .unwrap();
    for action in [Action::Install, Action::Status] {
        assert_eq!(
            std::mem::discriminant(&lifecycle(&plan, action, &fs, &mut unavailable).unwrap_err()),
            std::mem::discriminant(&Error::Conflict)
        );
    }
}

#[test]
fn interrupted_install_publication_resumes_without_replacing_identity() {
    let (_temp, plan, mut fs) = fixture(Platform::Linux, true);
    fs.protect_config(&plan.config).unwrap();
    let identity = crate::config::Config::load(&fs.resolve(&plan.config))
        .unwrap()
        .endpoint_uuid;
    let calls = fs.calls;
    fs.calls.sync_directory = |path| {
        if path.join("tethercam.service").exists() {
            Err(io_error())
        } else {
            std::fs::File::open(path).unwrap().sync_all()
        }
    };
    assert!(
        lifecycle(&plan, Action::Install, &fs, &mut |_, _| Ok(reply(
            0,
            "LoadState=not-found\nActiveState=inactive\n"
        )))
        .is_err()
    );
    assert!(fs.resolve(plan.unit()).exists());
    fs.calls = calls;
    assert_eq!(
        lifecycle(&plan, Action::Install, &fs, &mut |_, args| Ok(reply(
            0,
            if args.iter().any(|arg| arg == "show") {
                "LoadState=loaded\nActiveState=active\n"
            } else {
                ""
            }
        )))
        .unwrap(),
        Status::Active
    );
    assert_eq!(
        crate::config::Config::load(&fs.resolve(&plan.config))
            .unwrap()
            .endpoint_uuid,
        identity
    );
}

#[test]
fn mac_load_idempotence_and_unload_are_explicit() {
    let (_temp, plan, fs) = fixture(Platform::Mac, true);
    let target = launch_target(&plan);
    let domain = format!("gui/{}", plan.uid);
    let mut script = Script::new();
    script.add("/bin/launchctl", &["print", &target], Ok(reply(113, "")));
    assert_eq!(
        lifecycle(&plan, Action::Status, &fs, &mut |p, a| script.run(p, a)).unwrap(),
        Status::Absent
    );
    script.add("/bin/launchctl", &["print", &target], Ok(reply(113, "")));
    script.add(
        "/bin/launchctl",
        &["bootstrap", &domain, plan.unit().to_str().unwrap()],
        Ok(reply(0, "")),
    );
    script.add(
        "/bin/launchctl",
        &["print", &target],
        Ok(reply(0, "not parsed")),
    );
    assert_eq!(
        lifecycle(&plan, Action::Install, &fs, &mut |p, a| script.run(p, a)).unwrap(),
        Status::Active
    );
    script.add("/bin/launchctl", &["print", &target], Ok(reply(0, "")));
    script.add("/bin/launchctl", &["print", &target], Ok(reply(0, "")));
    assert_eq!(
        lifecycle(&plan, Action::Install, &fs, &mut |p, a| script.run(p, a)).unwrap(),
        Status::Active
    );
    script.add("/bin/launchctl", &["print", &target], Ok(reply(0, "")));
    script.add("/bin/launchctl", &["bootout", &target], Ok(reply(0, "")));
    assert_eq!(
        lifecycle(&plan, Action::Uninstall, &fs, &mut |p, a| script.run(p, a)).unwrap(),
        Status::Absent
    );
    // Installed but not registered is distinct from missing files.
    publish(&plan, &fs);
    script.add("/bin/launchctl", &["print", &target], Ok(reply(113, "")));
    assert_eq!(
        lifecycle(&plan, Action::Status, &fs, &mut |p, a| script.run(p, a)).unwrap(),
        Status::Unloaded
    );
    script.add("/bin/launchctl", &["print", &target], Ok(reply(113, "")));
    assert_eq!(
        lifecycle(&plan, Action::Uninstall, &fs, &mut |p, a| script.run(p, a)).unwrap(),
        Status::Absent
    );
    script.add("/bin/launchctl", &["print", &target], Ok(reply(113, "")));
    assert_eq!(
        lifecycle(&plan, Action::Uninstall, &fs, &mut |p, a| script.run(p, a)).unwrap(),
        Status::Absent
    );
    script.done();
}

#[test]
fn inaccessible_system_binary_or_ancestor_is_rejected_before_publication() {
    for private in ["/usr/local/bin/tethercam", "/usr/local"] {
        let (_temp, plan, fs) = fixture(Platform::Linux, false);
        fs::set_permissions(
            fs.resolve(Path::new(private)),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let mut calls = 0;
        let error = lifecycle(&plan, Action::Install, &fs, &mut |program, args| {
            calls += 1;
            assert_eq!(program, "/usr/bin/systemctl");
            assert_eq!(args.first(), Some(&OsString::from("show")));
            Ok(reply(0, "LoadState=not-found\nActiveState=inactive\n"))
        })
        .unwrap_err();
        assert_eq!(
            std::mem::discriminant(&error),
            std::mem::discriminant(&Error::ExecutableAccess)
        );
        assert_eq!(calls, 1);
        for (path, _) in &plan.files {
            assert!(!fs.resolve(path).exists());
        }
        assert!(!fs.resolve(&plan.config).exists());
    }
}

#[test]
fn linux_system_user_udev_and_ownership_without_host_changes() {
    let (_temp, plan, fs) = fixture(Platform::Linux, false);
    let meta = std::fs::metadata(&fs.root).unwrap();
    let mut script = Script::new();
    show(&mut script, false, "not-found", "inactive");
    script.add(
        "/usr/bin/systemd-sysusers",
        &[fs.resolve(Path::new("/etc/sysusers.d/tethercam.conf"))
            .to_str()
            .unwrap()],
        Ok(reply(0, "")),
    );
    script.add(
        "/usr/bin/id",
        &["-u", "tethercam"],
        Ok(reply(0, &meta.uid().to_string())),
    );
    script.add(
        "/usr/bin/id",
        &["-g", "tethercam"],
        Ok(reply(0, &meta.gid().to_string())),
    );
    script.add(
        "/usr/bin/getent",
        &["passwd", "tethercam"],
        Ok(reply(
            0,
            &format!(
                "tethercam:x:{}:{}:Tethercam service:/var/lib/tethercam:/usr/sbin/nologin\n",
                meta.uid(),
                meta.gid()
            ),
        )),
    );
    script.add(
        "/usr/bin/udevadm",
        &["control", "--reload-rules"],
        Ok(reply(0, "")),
    );
    script.add("/usr/bin/systemctl", &["daemon-reload"], Ok(reply(0, "")));
    script.add(
        "/usr/bin/systemctl",
        &["enable", "--now", "tethercam.service"],
        Ok(reply(0, "")),
    );
    show(&mut script, false, "loaded", "active");
    assert_eq!(
        lifecycle(&plan, Action::Install, &fs, &mut |p, a| script.run(p, a)).unwrap(),
        Status::Active
    );
    let cfg_meta = std::fs::metadata(fs.resolve(&plan.config)).unwrap();
    assert_eq!(cfg_meta.uid(), meta.uid());
    assert_eq!(cfg_meta.gid(), meta.gid());
    show(&mut script, false, "loaded", "active");
    script.add(
        "/usr/bin/systemctl",
        &["disable", "--now", "tethercam.service"],
        Ok(reply(0, "")),
    );
    script.add("/usr/bin/systemctl", &["daemon-reload"], Ok(reply(0, "")));
    script.add(
        "/usr/bin/udevadm",
        &["control", "--reload-rules"],
        Ok(reply(0, "")),
    );
    assert_eq!(
        lifecycle(&plan, Action::Uninstall, &fs, &mut |p, a| script.run(p, a)).unwrap(),
        Status::Absent
    );
    assert!(fs.resolve(&plan.config).exists());
    script.done();
}

#[test]
fn manager_and_process_failures_are_typed_and_never_echo_output() {
    assert!(run("/synthetic/missing", &[]).is_err());
    assert_eq!(run("/usr/bin/true", &[]).unwrap().code, Some(0));
    assert!(
        checked(
            &mut |_, _| Ok(reply(1, "synthetic secret")),
            "/synthetic/tool",
            &[]
        )
        .is_err()
    );
    assert!(
        checked(
            &mut |_, _| Ok(Reply {
                code: None,
                stdout: vec![]
            }),
            "/synthetic/tool",
            &[]
        )
        .is_err()
    );
    assert!(checked(&mut |_, _| Err(io_error()), "/synthetic/tool", &[]).is_err());
    assert!(Manager::new(true).is_ok()); // only safe identity/path discovery, no daemon operations
    for (os, home, binary, result) in [
        (
            "unsupported",
            Some("/home/test".into()),
            Ok("/bin/test".into()),
            Ok(reply(0, "501")),
        ),
        ("macos", None, Ok("/bin/test".into()), Ok(reply(0, "501"))),
        (
            "macos",
            Some("/home/test".into()),
            Err(io_error()),
            Ok(reply(0, "501")),
        ),
        (
            "macos",
            Some("/home/test".into()),
            Ok("/bin/test".into()),
            Ok(reply(0, "bad")),
        ),
        (
            "macos",
            Some("/home/test".into()),
            Ok("/bin/test".into()),
            Err(io_error()),
        ),
    ] {
        let mut result = Some(result);
        assert!(prepare(true, os, home, binary, &mut |_, _| result.take().unwrap()).is_err());
    }
    for platform in [Platform::Mac, Platform::Linux] {
        let (_temp, plan, fs) = fixture(platform, true);
        assert!(lifecycle(&plan, Action::Status, &fs, &mut unavailable).is_err());
        assert!(lifecycle(&plan, Action::Status, &fs, &mut |_, _| Ok(reply(1, ""))).is_err());
        if platform == Platform::Mac {
            assert!(observe(&plan, false, &mut |_, _| Ok(reply(0, ""))).is_err());
        } else {
            assert!(
                observe(&plan, false, &mut |_, _| Ok(reply(
                    0,
                    "LoadState=loaded\nActiveState=active\n"
                )))
                .is_err()
            );
            assert!(observe(&plan, false, &mut |_, _| Ok(reply(0, "bad"))).is_err());
        }
    }
    for value in [b"".as_slice(), b"-1", b"4294967296", &[0xff]] {
        assert!(number(value).is_err());
    }
    assert_eq!(number(b"501\n").unwrap(), 501);
    assert_eq!(platform("linux").unwrap(), Platform::Linux);
}

#[test]
fn failed_starts_preserve_files_failed_stops_never_remove_files() {
    for platform in [Platform::Linux, Platform::Mac] {
        for action in [Action::Install, Action::Uninstall] {
            let (_temp, plan, fs) = fixture(platform, true);
            publish(&plan, &fs);
            let mut calls = 0;
            let error = lifecycle(&plan, action, &fs, &mut |_, _| {
                calls += 1;
                if calls == 1 {
                    Ok(reply(
                        0,
                        if platform == Platform::Linux {
                            "LoadState=loaded\nActiveState=active\n"
                        } else {
                            ""
                        },
                    ))
                } else {
                    Err(io_error())
                }
            });
            // Mac install is already loaded and has no mutating call; fail the final observation.
            assert!(error.is_err());
            assert!(
                fs.matches(plan.unit(), plan.files.first().unwrap().1.as_bytes())
                    .unwrap()
            );
        }
    }
    let (_temp, plan, fs) = fixture(Platform::Linux, true);
    fs.publish(plan.unit(), b"unrelated").unwrap();
    let error = lifecycle(&plan, Action::Install, &fs, &mut unavailable).unwrap_err();
    assert_eq!(
        std::mem::discriminant(&error),
        std::mem::discriminant(&Error::Conflict)
    );
}

#[test]
fn state_machine_rejects_malformed_or_ambiguous_replies() {
    for (load, active, expected) in [
        ("not-found", "inactive", Status::Absent),
        ("loaded", "active", Status::Active),
        ("loaded", "activating", Status::Active),
        ("loaded", "reloading", Status::Active),
        ("loaded", "inactive", Status::Inactive),
        ("loaded", "deactivating", Status::Inactive),
        ("loaded", "failed", Status::Failed),
        ("error", "inactive", Status::Failed),
        ("bad-setting", "inactive", Status::Failed),
        ("masked", "inactive", Status::Failed),
    ] {
        assert_eq!(
            parse_state(
                format!("LoadState={load}\nActiveState={active}\n").as_bytes(),
                false
            )
            .unwrap(),
            expected
        );
    }
    assert_eq!(
        parse_state(b"LoadState=not-found\nActiveState=inactive\n", true).unwrap(),
        Status::Unloaded
    );
    for bytes in [
        b"".as_slice(),
        b"LoadState=loaded\n",
        b"ActiveState=active\n",
        b"LoadState=loaded\nLoadState=loaded\n",
        b"ActiveState=active\nActiveState=active\n",
        b"LoadState=loaded\nActiveState=unknown\n",
        b"other=value\n",
        &[0xff],
    ] {
        assert!(parse_state(bytes, false).is_err());
    }
}

#[test]
fn filesystem_failures_reach_lifecycle_callers_without_starting_a_daemon() {
    for step in 0..9 {
        let (_temp, plan, mut fs) = fixture(Platform::Mac, true);
        match step {
            0 => fs.calls.temporary = |_| Err(io_error()),
            1 => {
                fs.calls.metadata = |path| {
                    if path.file_name() == Some(std::ffi::OsStr::new("config.toml")) {
                        Err(io_error())
                    } else {
                        std::fs::symlink_metadata(path)
                    }
                }
            }
            2 => {
                fs.calls.mkdir = |path| {
                    if path.file_name() == Some(std::ffi::OsStr::new("Logs")) {
                        Err(io_error())
                    } else {
                        use std::os::unix::fs::DirBuilderExt;
                        std::fs::DirBuilder::new().mode(0o700).create(path)
                    }
                }
            }
            3 => {
                fs.calls.metadata = |path| {
                    if path.file_name() == Some(std::ffi::OsStr::new("stdout.log")) {
                        Err(io_error())
                    } else {
                        std::fs::symlink_metadata(path)
                    }
                }
            }
            4 => {
                fs.calls.metadata = |path| {
                    if path.file_name() == Some(std::ffi::OsStr::new("stderr.log")) {
                        Err(io_error())
                    } else {
                        std::fs::symlink_metadata(path)
                    }
                }
            }
            5 => {
                std::fs::remove_file(fs.resolve(&plan.binary)).unwrap();
            }
            6 => {
                publish(&plan, &fs);
                fs.calls.remove = |_| Err(io_error());
            }
            7 => {
                publish(&plan, &fs);
                fs.calls.metadata = |path| {
                    if path.file_name() == Some(std::ffi::OsStr::new("config.toml")) {
                        Err(io_error())
                    } else {
                        std::fs::symlink_metadata(path)
                    }
                };
            }
            _ => {
                publish(&plan, &fs);
                fs.calls.chown = chown_failure;
            }
        }
        let action = if step == 6 {
            Action::Uninstall
        } else {
            Action::Install
        };
        if step == 8 {
            // No chown is attempted by a per-user agent, even with a failing chown syscall.
            assert_eq!(
                lifecycle(&plan, action, &fs, &mut |_, args| {
                    if args.first() == Some(&OsString::from("list")) {
                        Ok(reply(0, "\"PID\" = 1;"))
                    } else {
                        Ok(reply(0, ""))
                    }
                })
                .unwrap(),
                Status::Active
            );
        } else {
            assert!(
                lifecycle(&plan, action, &fs, &mut |_, args| {
                    Ok(if args.first() == Some(&OsString::from("list")) {
                        reply(113, "")
                    } else {
                        reply(0, "")
                    })
                })
                .is_err()
            );
        }
    }
}

#[test]
fn mutating_manager_failures_are_not_swallowed() {
    for (platform, action, fail_command) in [
        (Platform::Mac, Action::Install, "bootstrap"),
        (Platform::Mac, Action::Install, "kickstart"),
        (Platform::Mac, Action::Uninstall, "bootout"),
        (Platform::Linux, Action::Install, "enable"),
        (Platform::Linux, Action::Uninstall, "disable"),
        (Platform::Linux, Action::Uninstall, "daemon-reload"),
    ] {
        let (_temp, plan, fs) = fixture(platform, true);
        if fail_command != "bootstrap" {
            publish(&plan, &fs);
        }
        assert!(
            lifecycle(&plan, action, &fs, &mut |_, args| {
                let first = args.iter().find(|value| *value != "--user").unwrap();
                if first == fail_command {
                    return Err(io_error());
                }
                if first == "show" {
                    Ok(reply(0, "LoadState=loaded\nActiveState=active\n"))
                } else if first == "list" {
                    Ok(if fail_command == "bootstrap" {
                        reply(113, "")
                    } else if fail_command == "kickstart" {
                        reply(0, "\"LastExitStatus\" = 1;")
                    } else {
                        reply(0, "\"PID\" = 1;")
                    })
                } else {
                    Ok(reply(0, ""))
                }
            })
            .is_err()
        );
    }
}

#[test]
fn system_installation_failure_keeps_stop_and_secret_ownership_explicit() {
    for step in 0..5 {
        let (_temp, plan, mut fs) = fixture(Platform::Linux, false);
        let meta = std::fs::metadata(&fs.root).unwrap();
        match step {
            0 => fs.calls.chown = chown_failure,
            1 => {
                fs.calls.chown = |file, _, _| {
                    if file.metadata().unwrap().is_file() {
                        Ok(())
                    } else {
                        Err(io_error())
                    }
                }
            }
            _ => {}
        }
        if step == 4 {
            publish(&plan, &fs);
        }
        let action = if step == 4 {
            Action::Uninstall
        } else {
            Action::Install
        };
        assert!(
            lifecycle(&plan, action, &fs, &mut |program, args| {
                if program == "/usr/bin/systemd-sysusers" && step == 2 {
                    return Err(io_error());
                }
                if program == "/usr/bin/udevadm" && step == 4 {
                    return Err(io_error());
                }
                if program == "/usr/bin/getent" {
                    return Ok(reply(
                        0,
                        &format!(
                            "tethercam:x:{}:{}:service:/var/lib/tethercam:/usr/sbin/nologin",
                            meta.uid(),
                            meta.gid()
                        ),
                    ));
                }
                if program == "/usr/bin/id" {
                    return Ok(reply(
                        0,
                        &if args.first() == Some(&OsString::from("-u")) {
                            meta.uid().to_string()
                        } else {
                            meta.gid().to_string()
                        },
                    ));
                }
                if program == "/usr/bin/systemctl" {
                    if args.first() == Some(&OsString::from("show")) {
                        return Ok(reply(
                            0,
                            if step == 4 {
                                "LoadState=loaded\nActiveState=active\n"
                            } else {
                                "LoadState=not-found\nActiveState=inactive\n"
                            },
                        ));
                    }
                    if args.first() == Some(&OsString::from("enable")) && step == 3 {
                        return Err(io_error());
                    }
                }
                Ok(reply(0, ""))
            })
            .is_err()
        );
    }
}

#[test]
fn manager_failure_after_domain_lookup_and_account_collision_propagate() {
    let (_temp, plan, fs) = fixture(Platform::Mac, true);
    assert!(
        observe(&plan, false, &mut |_, args| Ok(reply(
            i32::from(args.first() != Some(&OsString::from("print"))),
            ""
        )))
        .is_err()
    );
    assert!(
        system_user(&fs, &mut |_, args| Ok(reply(
            0,
            if args.first() == Some(&OsString::from("passwd")) {
                "other:x:501:501:service:/var/lib/tethercam:/usr/sbin/nologin"
            } else {
                "501"
            }
        )))
        .is_err()
    );
}

#[test]
fn launchd_dictionary_is_total_and_distinguishes_process_failure() {
    for (text, expected) in [
        (
            "{\n\"PID\" = 123;\n\"LastExitStatus\" = 9;\n}",
            Status::Active,
        ),
        ("{\n\"LastExitStatus\" = 0;\n}", Status::Inactive),
        (
            "{\n\"PID\" = 0;\n\"LastExitStatus\" = -15;\n}",
            Status::Failed,
        ),
    ] {
        assert_eq!(parse_launchd(text.as_bytes()).unwrap(), expected);
    }
    for text in [
        "",
        "\"PID\" = bad;",
        "\"PID\" = 1",
        "\"LastExitStatus\" = bad;",
        "\"LastExitStatus\" = 1",
        "\"PID\" = 1;\n\"PID\" = 2;",
        "\"LastExitStatus\" = 0;\n\"LastExitStatus\" = 1;",
    ] {
        assert!(parse_launchd(text.as_bytes()).is_err());
    }
    assert!(parse_launchd(&[0xff]).is_err());
    assert!(bus_variable(std::ffi::OsStr::new("XDG_RUNTIME_DIR")));
    assert!(bus_variable(std::ffi::OsStr::new(
        "DBUS_SESSION_BUS_ADDRESS"
    )));
    assert!(!bus_variable(std::ffi::OsStr::new("SYNTHETIC_SECRET")));
}

#[test]
fn launchd_registered_inactive_is_kickstarted_without_duplicate_bootstrap() {
    let (_temp, plan, fs) = fixture(Platform::Mac, true);
    publish(&plan, &fs);
    let mut calls = Vec::new();
    let state = lifecycle(&plan, Action::Install, &fs, &mut |program, args| {
        calls.push((program.to_owned(), args.to_vec()));
        Ok(reply(0, "{\n\"LastExitStatus\" = 1;\n}"))
    })
    .unwrap();
    assert_eq!(state, Status::Failed);
    assert_eq!(calls.len(), 5);
    assert_eq!(
        calls.get(2).unwrap().1,
        vec![OsString::from("kickstart"), launch_target(&plan).into()]
    );
}

#[test]
fn account_collision_and_parser_failures_are_not_adopted() {
    for text in [
        "",
        "tethercam:x",
        "tethercam:x:501",
        "tethercam:x:bad:501",
        "tethercam:x:501:bad",
        "other:x:501:501:service:/var/lib/tethercam:/usr/sbin/nologin",
        "tethercam:x:502:501:service:/var/lib/tethercam:/usr/sbin/nologin",
        "tethercam:x:501:502:service:/var/lib/tethercam:/usr/sbin/nologin",
        "tethercam:x:501:501:service:/home/other:/usr/sbin/nologin",
        "tethercam:x:501:501:service:/var/lib/tethercam:/bin/sh",
        "tethercam:x:501:501:service:/var/lib/tethercam:/usr/sbin/nologin:extra",
    ] {
        assert!(validate_account(text.as_bytes(), 501, 501).is_err());
    }
    assert!(validate_account(&[0xff], 501, 501).is_err());
    assert!(
        validate_account(
            b"tethercam:x:501:501:service:/var/lib/tethercam:/usr/sbin/nologin\n",
            501,
            501
        )
        .is_ok()
    );
}

#[test]
fn system_user_failure_stops_before_service_enable() {
    let (_temp, _plan, fs) = fixture(Platform::Linux, false);
    for fail_at in 1..=5 {
        let mut calls = 0;
        assert!(
            system_user(&fs, &mut |_, _| {
                calls += 1;
                if calls == fail_at {
                    Err(io_error())
                } else {
                    Ok(reply(
                        0,
                        if calls == 4 {
                            "tethercam:x:501:501:service:/var/lib/tethercam:/usr/sbin/nologin"
                        } else {
                            "501"
                        },
                    ))
                }
            })
            .is_err()
        );
    }
    for (uid, gid) in [("bad", "501"), ("0", "501"), ("501", "0"), ("501", "bad")] {
        let mut calls = 0;
        assert!(
            system_user(&fs, &mut |_, _| {
                calls += 1;
                Ok(reply(
                    0,
                    match calls {
                        2 => uid,
                        3 => gid,
                        _ => "",
                    },
                ))
            })
            .is_err()
        );
    }
}

impl Manager {
    pub(crate) fn test_status(root: PathBuf, fail: bool) -> Self {
        let (_, plan, _) = fixture(Platform::Linux, true);
        let uid = fs::metadata(&root).unwrap().uid();
        Self {
            plan,
            fs: Files::new(root, uid),
            runner: Box::new(move |_, _| {
                if fail {
                    Err(io_error())
                } else {
                    Ok(reply(0, "LoadState=not-found\nActiveState=inactive\n"))
                }
            }),
        }
    }
}

#[test]
fn public_manager_and_status_output() {
    let temp = tempfile::tempdir().unwrap();
    assert_eq!(
        Manager::test_status(temp.path().into(), false)
            .manage(Action::Status)
            .unwrap(),
        Status::Absent
    );
    for (state, text) in [
        (Status::Absent, "absent"),
        (Status::Inactive, "inactive"),
        (Status::Active, "active"),
        (Status::Failed, "failed"),
        (Status::Unloaded, "installed, not loaded"),
    ] {
        assert_eq!(state.to_string(), text);
    }
    assert!(!Error::Manager.to_string().contains("synthetic secret"));
}

proptest! {
    #[test]
    fn arbitrary_native_replies_are_total(bytes in proptest::collection::vec(any::<u8>(),0..512), present in any::<bool>()) {
        let _ = parse_state(&bytes, present);
        let _ = parse_launchd(&bytes);
        let _ = number(&bytes);
        let _ = validate_account(&bytes, 501, 501);
    }
}

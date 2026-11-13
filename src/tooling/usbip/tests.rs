//! Real temporary sysfs trees, strict command recording and injected syscall failures.
//!
//! References:
//! - <https://docs.kernel.org/usb/usbip_protocol.html>

use super::*;
use std::cell::RefCell;

#[derive(Default)]
struct Calls {
    argv: Vec<(PathBuf, Vec<OsString>)>,
    fail: usize,
    sleeps: usize,
    character: bool,
    character_error: bool,
    uid: String,
    gid: String,
}
thread_local! { static CALLS: RefCell<Calls> = RefCell::default(); }

fn reset() {
    CALLS.with(|c| {
        *c.borrow_mut() = Calls {
            fail: usize::MAX,
            character: true,
            uid: "1001\n".into(),
            gid: "1002\n".into(),
            ..Calls::default()
        }
    });
}

fn exec(program: &Path, args: &[OsString]) -> Result<String, Error> {
    CALLS.with(|c| {
        let mut c = c.borrow_mut();
        c.argv.push((program.to_path_buf(), args.to_vec()));
        if c.argv.len() == c.fail {
            return Err(invalid());
        }
        if program == Path::new("/usr/bin/id") {
            return Ok(if args == argv(&["-u"]) {
                c.uid.clone()
            } else {
                c.gid.clone()
            });
        }
        Ok(String::new())
    })
}
fn pause(_duration: Duration) {
    CALLS.with(|c| c.borrow_mut().sleeps += 1);
}
fn fake_character(_path: &Path) -> Result<bool, Error> {
    CALLS.with(|c| {
        let c = c.borrow();
        if c.character_error {
            Err(invalid())
        } else {
            Ok(c.character)
        }
    })
}
fn host(root: &Path) -> Host<'_> {
    Host {
        root,
        privileged: true,
        canonical,
        entries,
        exec,
        pause,
        character: fake_character,
    }
}

fn put(root: &Path, path: &str, contents: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(path, contents).expect("write");
}

fn tree() -> tempfile::TempDir {
    reset();
    let temp = tempfile::tempdir().expect("temp");
    put(temp.path(), "usr/lib/linux-tools/1/usbip", "");
    put(
        temp.path(),
        "sys/devices/platform/vhci_hcd.0/status",
        "hub port sta speed dev socket busid\nhs 00 6 3 1 0 1-1\n",
    );
    fs::create_dir_all(temp.path().join("sys/bus/usb/devices")).expect("mkdir");
    temp
}

#[cfg(unix)]
fn device(root: &Path, controller: &str) -> PathBuf {
    let actual = root.join(format!("sys/devices/platform/{controller}/usb1/1-1"));
    fs::create_dir_all(&actual).expect("mkdir");
    for (name, value) in [
        ("serial", SERIAL),
        ("idVendor", "054c"),
        ("idProduct", "0c34"),
        ("busnum", "1"),
        ("devnum", "2"),
    ] {
        fs::write(actual.join(name), value).expect("write");
    }
    std::os::unix::fs::symlink(&actual, root.join("sys/bus/usb/devices/1-1")).expect("symlink");
    actual
}

#[test]
fn environment_is_exact_and_fail_closed() {
    let mut env = |key: &str| {
        Some(OsString::from(match key {
            "RUNNER_ENVIRONMENT" => "github-hosted",
            "RUNNER_OS" => "Linux",
            _ => "true",
        }))
    };
    assert!(authorize(&mut env, true).is_ok());
    assert!(authorize(&mut env, false).is_err());
    for bad in [
        None,
        Some(""),
        Some("false"),
        Some("1"),
        Some("self-hosted"),
    ] {
        for key in ["CI", "GITHUB_ACTIONS", "RUNNER_ENVIRONMENT", "RUNNER_OS"] {
            assert!(
                authorize(
                    &mut |name| if name == key {
                        bad.map(OsString::from)
                    } else {
                        env(name)
                    },
                    true
                )
                .is_err()
            );
        }
    }
}

#[test]
fn identifiers_never_become_paths_or_options() {
    for value in [
        "", "../1", "-p", "1/2", "1-2;id", "1-2-3", "1-", "1", "65536-1", "1-1..2",
    ] {
        assert!(busid(value).is_err());
    }
    for value in ["1-1", "12-2.3", "0-0"] {
        busid(value).expect("valid bus id");
    }
    assert!(numeric("65536").is_err());
    for value in ["", "1 2", "4294967296", "-1"] {
        assert!(identity(value).is_err());
    }
    assert_eq!(identity("100000\n").expect("identity"), 100_000);
}

#[test]
fn native_process_outputs_are_not_leaked() {
    assert_eq!(
        command(Path::new("/usr/bin/printf"), &argv(&["ok"])).expect("printf"),
        "ok"
    );
    assert!(command(Path::new("/usr/bin/false"), &[]).is_err());
    assert!(command(Path::new("/nonexistent-tool-fixture"), &[]).is_err());
    assert!(command(Path::new("/usr/bin/printf"), &argv(&["\\377"])).is_err());
    #[cfg(unix)]
    {
        assert!(character(Path::new("/dev/null")).expect("character device"));
        let temp = tempfile::NamedTempFile::new().expect("temp");
        assert!(!character(temp.path()).expect("ordinary file"));
        assert!(character(Path::new("/nonexistent-device-fixture")).is_err());
    }
}

#[test]
fn prepare_argv_and_each_failure() {
    let temp = tree();
    let h = host(temp.path());
    execute(OsStr::new("prepare"), Err(invalid()), &h).expect_err("guard");
    CALLS.with(|c| c.borrow_mut().uid = "0".into());
    assert!(execute(OsStr::new("prepare"), Ok(()), &h).is_err());
    reset();
    execute(OsStr::new("prepare"), Ok(()), &h).expect("authorized");
    reset();
    h.action(OsStr::new("prepare")).expect("prepare");
    CALLS.with(|c| {
        let c = c.borrow();
        assert_eq!(c.argv.len(), 4);
        assert_eq!(
            c.argv[0],
            (
                PathBuf::from("/usr/bin/sudo"),
                argv(&["--", "/usr/bin/apt-get", "update", "-qq"])
            )
        );
        assert_eq!(c.argv[2].1, argv(&["--", "/usr/sbin/modprobe", "vhci_hcd"]));
    });
    for n in 1..=4 {
        reset();
        CALLS.with(|c| c.borrow_mut().fail = n);
        assert!(h.action(OsStr::new("prepare")).is_err());
    }
    reset();
    assert!(h.action(OsStr::new("nonsense")).is_err());
    fs::remove_file(temp.path().join("sys/devices/platform/vhci_hcd.0/status")).expect("remove");
    assert!(h.action(OsStr::new("prepare")).is_err());
    fs::remove_file(temp.path().join("usr/lib/linux-tools/1/usbip")).expect("remove");
    assert!(h.tool().is_err());
    fs::remove_dir_all(temp.path().join("usr/lib/linux-tools")).expect("remove");
    assert!(h.tool().is_err());
}

#[test]
#[cfg(unix)]
fn attach_exact_synthetic_node_and_failure_paths() {
    let temp = tree();
    let actual = device(temp.path(), "vhci_hcd.0");
    let h = host(temp.path());
    h.action(OsStr::new("attach")).expect("attach");
    CALLS.with(|c| {
        let c = c.borrow();
        assert_eq!(c.argv[0].0, Path::new("/usr/bin/timeout"));
        assert_eq!(
            c.argv[0].1[4..],
            argv(&["attach", "-r", "127.0.0.1", "-b", "1-1"])
        );
        assert_eq!(
            c.argv[3],
            (
                PathBuf::from("/usr/bin/sudo"),
                vec![
                    "--".into(),
                    "/usr/bin/chown".into(),
                    "1001:1002".into(),
                    temp.path().join("dev/bus/usb/001/002").into_os_string()
                ]
            )
        );
    });
    for n in 1..=4 {
        reset();
        CALLS.with(|c| c.borrow_mut().fail = n);
        assert!(h.attach().is_err());
    }
    for uid in ["0", "bad"] {
        reset();
        CALLS.with(|c| c.borrow_mut().uid = uid.into());
        assert!(h.attach().is_err());
    }
    reset();
    CALLS.with(|c| c.borrow_mut().gid = "bad".into());
    assert!(h.attach().is_err());
    reset();
    CALLS.with(|c| c.borrow_mut().character = false);
    assert!(h.attach().is_err());
    CALLS.with(|c| assert_eq!(c.borrow().sleeps, 100));
    reset();
    CALLS.with(|c| c.borrow_mut().character_error = true);
    assert!(h.attach().is_err());
    reset();
    for (name, value) in [
        ("busnum", "0"),
        ("busnum", "bad"),
        ("devnum", "0"),
        ("devnum", "bad"),
        ("idVendor", "ffff"),
    ] {
        let old = fs::read(actual.join(name)).expect("old");
        fs::write(actual.join(name), value).expect("change");
        assert!(h.attach().is_err());
        fs::write(actual.join(name), old).expect("restore");
    }
    fs::write(actual.join("serial"), "OTHER").expect("serial");
    assert!(h.attach().is_err());
    fs::remove_file(actual.join("serial")).expect("remove");
    assert!(h.attach().is_err());
    fs::remove_dir_all(temp.path().join("sys/bus/usb/devices")).expect("remove");
    assert!(h.attach().is_err());
}

#[test]
#[cfg(unix)]
fn ancestry_and_detach_are_targeted() {
    let temp = tree();
    let actual = device(temp.path(), "vhci_hcd.0");
    let h = host(temp.path());
    fs::create_dir_all(temp.path().join("sys/devices/platform/unrelated")).expect("mkdir");
    h.action(OsStr::new("detach")).expect("detach");
    CALLS.with(|c| assert_eq!(c.borrow().argv[0].1[4..], argv(&["detach", "-p", "00"])));
    CALLS.with(|c| c.borrow_mut().fail = 2);
    assert!(h.detach().is_err());
    reset();
    let status = temp.path().join("sys/devices/platform/vhci_hcd.0/status");
    for line in ["bad", "hs 00 6 3 1 0 ../../escape", "hs -1 6 3 1 0 1-1"] {
        fs::write(&status, format!("header\n{line}\n")).expect("status");
        assert!(h.detach().is_err());
    }
    fs::write(&status, "header\nhs 00 0 0 0 0 0-0\n").expect("disconnected");
    h.detach().expect("no connected ports");
    assert!(!h.synthetic(temp.path()).expect("outside platform"));
    assert!(h.synthetic(&actual.join("missing")).is_err());
    for controller in ["physical", "vhci_hcd.bad"] {
        let destination = temp
            .path()
            .join(format!("sys/devices/platform/{controller}/usb1/1-1"));
        fs::create_dir_all(&destination).expect("mkdir");
        assert_eq!(
            h.synthetic(&destination).is_err(),
            controller == "vhci_hcd.bad"
        );
    }
    for usb in ["other", "usbbad"] {
        let destination = temp
            .path()
            .join(format!("sys/devices/platform/vhci_hcd.0/{usb}/1-1"));
        fs::create_dir_all(&destination).expect("mkdir");
        assert_eq!(h.synthetic(&destination).is_err(), usb == "usbbad");
    }
    assert!(
        h.synthetic(&temp.path().join("sys/devices/platform"))
            .is_err()
    );
    assert!(
        h.synthetic(&temp.path().join("sys/devices/platform/vhci_hcd.0"))
            .is_err()
    );
    fs::remove_dir_all(temp.path().join("sys/devices/platform")).expect("remove");
    assert!(h.detach().is_err());
    assert!(h.synthetic(temp.path()).is_err());
}

#[path = "failure_tests.rs"]
mod failures;

proptest::proptest! {
    #[test]
    fn arbitrary_identifiers_never_panic(text in ".*") { let _ = busid(&text); let _ = identity(&text); }
}

#[test]
fn preconfigured_local_mode_never_escalates_or_changes_node_ownership() {
    assert!(authorize_local(false, Some(OsStr::new("1")), true).is_ok());
    for (ci, enabled, linux) in [
        (false, Some("1"), false),
        (true, Some("1"), true),
        (false, None, true),
        (false, Some("0"), true),
    ] {
        assert!(authorize_local(ci, enabled.map(OsStr::new), linux).is_err());
    }
    let temp = tree();
    let actual = device(temp.path(), "vhci_hcd.0");
    put(temp.path(), "sys/devices/platform/vhci_hcd.0/attach", "");
    put(temp.path(), "sys/devices/platform/vhci_hcd.0/detach", "");
    put(temp.path(), "dev/bus/usb/001/002", "");
    let mut h = host(temp.path());
    h.privileged = false;
    assert!(h.action(OsStr::new("prepare")).is_err());
    execute(OsStr::new("attach"), Ok(()), &h).expect("preconfigured");
    h.detach().expect("delegated detach");
    CALLS.with(|calls| {
        assert!(
            calls
                .borrow()
                .argv
                .iter()
                .all(|(_, args)| !args.contains(&OsString::from("/usr/bin/sudo")))
        );
    });
    fs::remove_file(temp.path().join("dev/bus/usb/001/002")).expect("remove node");
    assert!(h.attach().is_err());
    fs::remove_file(temp.path().join("sys/devices/platform/vhci_hcd.0/attach"))
        .expect("remove authority");
    assert!(h.usbip(&["attach"]).is_err());
    fs::write(actual.join("idProduct"), "ffff").expect("product");
    assert!(h.synthetic(&actual).is_err());
}

#[test]
#[cfg(unix)]
fn invalid_os_bytes_never_enter_sysfs_names() {
    use std::os::unix::ffi::OsStrExt;
    assert!(name(OsStr::from_bytes(&[255])).is_err());
    assert_eq!(name(OsStr::new("vhci_hcd.0")).expect("name"), "vhci_hcd.0");
    let temp = tree();
    let actual = device(temp.path(), "vhci_hcd.0");
    fs::write(actual.join("serial"), "OTHER").expect("serial");
    host(temp.path()).detach().expect("no unrelated detach");
    CALLS.with(|calls| assert_eq!(calls.borrow().argv, [] as [(PathBuf, Vec<OsString>); 0]));
}

#[test]
#[cfg(unix)]
fn canonical_sysfs_os_byte_failures_and_non_sony_identity_are_targeted() {
    use std::os::unix::ffi::OsStringExt;
    let temp = tree();
    let actual = device(temp.path(), "vhci_hcd.0");
    let mut h = host(temp.path());
    h.canonical = |path| Ok(path.to_path_buf());
    for parent in ["sys/devices/platform", "sys/devices/platform/vhci_hcd.0"] {
        let path = temp.path().join(parent).join(OsString::from_vec(vec![255]));
        assert!(h.synthetic(&path).is_err());
    }
    h.entries = |_| Ok(vec![PathBuf::from(OsString::from_vec(vec![255]))]);
    assert!(h.detach().is_err());
    let h = host(temp.path());
    for (serial, vendor, product, expected) in [
        ("TETHERCAM-ECHO-0001", "ffff", "0001", true),
        ("TETHERCAM-ECHO-0001", "054c", "0001", false),
        ("TETHERCAM-ECHO-0001", "ffff", "0002", false),
        (SERIAL, "054c", "0c03", true),
    ] {
        for (name, value) in [
            ("serial", serial),
            ("idVendor", vendor),
            ("idProduct", product),
        ] {
            fs::write(actual.join(name), value).expect("identity");
        }
        assert_eq!(h.synthetic(&actual).is_ok_and(|found| found), expected);
    }
    assert!(
        crate::dispatch(
            &argv(&["usbip-local", "bad"]),
            &mut Vec::new(),
            &mut std::io::empty()
        )
        .is_err()
    );
}

#[test]
fn a_matching_serial_with_wrong_vendor_never_authorizes_detach() {
    let temp = tree();
    let actual = device(temp.path(), "vhci_hcd.0");
    fs::write(actual.join("idVendor"), "ffff").expect("wrong vendor");
    assert!(host(temp.path()).detach().is_err());
}

//! Filesystem and OS-string failures not reproducible on the hosted synthetic device.

use super::tests::{CALLS, device, host, reset, tree};
use super::*;

#[test]
#[cfg(unix)]
fn every_file_failure_and_non_utf8_component_is_closed() {
    use std::os::unix::ffi::OsStringExt;
    let temp = tree();
    let actual = device(temp.path(), "vhci_hcd.0");
    let h = host(temp.path());
    for name in ["serial", "idVendor", "idProduct", "busnum", "devnum"] {
        let path = actual.join(name);
        let contents = fs::read(&path).expect("read");
        fs::remove_file(&path).expect("remove");
        assert!(h.synthetic(&actual).is_err() || name == "busnum" || name == "devnum");
        assert!(h.attach().is_err());
        fs::write(path, contents).expect("restore");
    }
    let bad_status = temp
        .path()
        .join("sys/devices/platform/vhci_hcd.0/status-extra");
    fs::create_dir(&bad_status).expect("mkdir");
    assert!(h.detach().is_err());
    fs::remove_dir(&bad_status).expect("remove");
    fs::write(temp.path().join("sys/devices/platform/vhci_hcd.9"), "").expect("file controller");
    assert!(h.detach().is_err());
    fs::remove_file(temp.path().join("sys/devices/platform/vhci_hcd.9")).expect("remove");
    fs::write(temp.path().join("sys/devices/platform/vhci_hcd.bad"), "").expect("bad controller");
    assert!(h.detach().is_err());
    reset();
    CALLS.with(|c| c.borrow_mut().fail = 1);
    assert!(execute(OsStr::new("prepare"), Ok(()), &h).is_err());
    reset();
    CALLS.with(|c| c.borrow_mut().uid = "invalid".into());
    assert!(execute(OsStr::new("prepare"), Ok(()), &h).is_err());
    reset();
    fs::remove_file(temp.path().join("usr/lib/linux-tools/1/usbip")).expect("remove");
    assert!(h.usbip(&["version"]).is_err());
    assert!(h.action(OsStr::new("prepare")).is_err());
    assert!(h.action(&OsString::from_vec(vec![255])).is_err());
}

// APFS rejects non-UTF-8 directory names; Linux sysfs accepts OS-byte paths.
#[test]
#[cfg(target_os = "linux")]
fn non_utf8_sysfs_components_fail_closed() {
    use std::os::unix::ffi::OsStringExt;
    let temp = tree();
    let h = host(temp.path());
    let controller = temp
        .path()
        .join("sys/devices/platform")
        .join(OsString::from_vec(vec![255]));
    fs::create_dir(&controller).expect("mkdir");
    assert!(h.synthetic(&controller).is_err());
    assert!(h.detach().is_err());
    fs::remove_dir(&controller).expect("remove");
    let usb = temp
        .path()
        .join("sys/devices/platform/vhci_hcd.0")
        .join(OsString::from_vec(vec![255]));
    fs::create_dir(&usb).expect("mkdir");
    assert!(h.synthetic(&usb).is_err());
    fs::remove_dir(&usb).expect("remove");
}

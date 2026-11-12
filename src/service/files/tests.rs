//! Deterministic syscall failures on isolated files, never host configuration.
//!
//! References: docs/service.md.

use super::*;
use std::os::unix::fs::PermissionsExt;

fn failure() -> io::Error {
    io::Error::other("synthetic syscall failure")
}
fn fixture() -> (tempfile::TempDir, Files) {
    let temp = tempfile::tempdir().unwrap();
    let uid = fs::metadata(temp.path()).unwrap().uid();
    let files = Files::new(temp.path().into(), uid);
    (temp, files)
}

#[test]
fn reads_identity_and_each_protection_failure() {
    let (_temp, mut files) = fixture();
    files.publish(Path::new("/data/file"), b"owned").unwrap();
    for path in [Path::new("/"), Path::new("relative/file")] {
        assert!(files.read(path).is_err());
        assert!(files.publish(path, b"data").is_err());
        assert!(files.remove(path, b"data").is_err());
        assert!(files.executable(path, false).is_err());
        assert!(files.protect_config(path).is_err());
    }
    let calls = files.calls;
    files.calls.metadata = |_| Err(failure());
    assert!(files.directory(Path::new("/data"), true).is_err());
    assert!(files.read(Path::new("/data/file")).is_err());
    assert!(files.executable(Path::new("/binary"), false).is_err());
    files.calls = calls;
    files.uid = u32::MAX;
    assert!(files.read(Path::new("/data/file")).is_err());
    files.uid = fs::metadata(&files.root).unwrap().uid();
    files.calls.read = |_| Err(failure());
    assert!(files.read(Path::new("/data/file")).is_err());
    files.log(Path::new("/data/file")).unwrap(); // existing logs never invoke the content reader
    assert!(files.protect_config(Path::new("/data/file")).is_err());
    assert!(files.remove(Path::new("/data/file"), b"owned").is_err());
    files.calls = calls;
    files.calls.metadata = |path| {
        if path.file_name() == Some(std::ffi::OsStr::new("file")) {
            Err(failure())
        } else {
            fs::symlink_metadata(path)
        }
    };
    assert!(files.read(Path::new("/data/file")).is_err());
    assert!(files.executable(Path::new("/data/file"), false).is_err());
    files.calls = calls;
    assert!((files.calls.sync_directory)(&files.resolve(Path::new("/missing"))).is_err());
}

#[test]
fn publication_fails_safely_at_each_syscall_and_never_clobbers() {
    for step in 0..7 {
        let (_temp, mut files) = fixture();
        match step {
            0 => files.calls.mkdir = |_| Err(failure()),
            1 => files.calls.temporary = |_| Err(failure()),
            2 => files.calls.write = |_, _| Err(failure()),
            3 => files.calls.sync_file = |_| Err(failure()),
            4 => {
                files.calls.persist = |file, _| {
                    Err(PersistError {
                        error: failure(),
                        file,
                    })
                }
            }
            5 => files.calls.sync_directory = |_| Err(failure()),
            _ => {
                files.calls.sync_directory = |path| {
                    if path.file_name() == Some(std::ffi::OsStr::new("data")) {
                        Err(failure())
                    } else {
                        File::open(path)?.sync_all()
                    }
                }
            }
        }
        assert!(files.publish(Path::new("/data/unit"), b"owned").is_err());
        if step == 6 {
            assert!((files.calls.sync_directory)(&files.resolve(Path::new("/missing"))).is_err());
        }
        if step == 0 {
            assert!(files.protect_config(Path::new("/data/cfg")).is_err());
        }
        if step == 1 {
            assert!(files.log(Path::new("/data/log")).is_err());
        }
    }
    let (_temp, files) = fixture();
    // Exercise native no-clobber even if a competitor publishes after the earlier inspection.
    files.directory(Path::new("/data"), true).unwrap();
    let file = (files.calls.temporary)(&files.resolve(Path::new("/data"))).unwrap();
    files
        .publish(Path::new("/data/unit"), b"competitor")
        .unwrap();
    assert!((files.calls.persist)(file, &files.resolve(Path::new("/data/unit"))).is_err());
    assert_eq!(
        files.read(Path::new("/data/unit")).unwrap().unwrap(),
        b"competitor"
    );
}

#[test]
fn large_logs_are_metadata_only_and_control_reads_are_bounded() {
    let (_temp, mut files) = fixture();
    let path = Path::new("/logs/stdout.log");
    files.publish(path, b"").unwrap();
    File::options()
        .write(true)
        .open(files.resolve(path))
        .unwrap()
        .set_len(1 << 30)
        .unwrap();
    // A sparse gigabyte log must not be copied or truncated during reinstall.
    files.calls.read = |_| Err(failure());
    files.log(path).unwrap();
    assert!(files.read(path).is_err());
    assert_eq!(fs::metadata(files.resolve(path)).unwrap().len(), 1 << 30);
    files.calls = Calls::native();
    assert!(files.read(path).is_err());
    let bytes = (files.calls.read)(&files.resolve(path)).unwrap();
    assert_eq!(bytes.len(), usize::from(CONTROL_BYTES) + 1);
    assert!((files.calls.read)(&files.resolve(Path::new("/missing"))).is_err());
    assert!((files.calls.read)(&files.resolve(Path::new("/logs"))).is_err());
}

#[test]
fn executable_permissions_are_checked_for_the_eventual_identity() {
    let (_temp, mut files) = fixture();
    let path = Path::new("/bin/bridge");
    files.publish(path, b"synthetic executable").unwrap();
    fs::set_permissions(files.resolve(path), fs::Permissions::from_mode(0o755)).unwrap();
    // A private root/ancestor is fine for its owner but cannot be assumed traversable by
    // the not-yet-created dedicated system account.
    assert!(files.executable(path, true).is_err());
    fs::set_permissions(&files.root, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(files.executable(path, true).is_err());
    fs::set_permissions(
        files.resolve(Path::new("/bin")),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    for (mode, system, allowed) in [
        (0o700, false, true),
        (0o700, true, false),
        (0o755, true, true),
        (0o744, true, false),
        (0o711, true, false),
        (0o750, true, false),
        (0o705, true, true),
    ] {
        fs::set_permissions(files.resolve(path), fs::Permissions::from_mode(mode)).unwrap();
        assert_eq!(files.executable(path, system).is_ok(), allowed);
    }
    let meta = fs::metadata(files.resolve(path)).unwrap();
    assert!(accessible(&meta, u32::MAX, false, 0o5));
    assert!(files.traverse(&files.resolve(path), false).is_err());
    assert!(
        files
            .traverse(&files.resolve(Path::new("/missing")), false)
            .is_err()
    );
    files.calls.metadata = |_| Err(failure());
    assert!(files.traverse(&files.root, false).is_err());
}

#[test]
fn config_replacement_never_reaches_path_based_ownership_change() {
    let (_temp, mut files) = fixture();
    let path = Path::new("/data/config.toml");
    files.protect_config(path).unwrap();
    files.publish(Path::new("/victim"), b"untouched").unwrap();
    let victim = files.resolve(Path::new("/victim"));
    fs::remove_file(files.resolve(path)).unwrap();
    std::os::unix::fs::symlink(&victim, files.resolve(path)).unwrap();
    // Model an ownership side effect without root or changing any host ownership.
    files.calls.chown = record_chown;
    assert!(files.chown(path, files.uid, 1, false).is_err());
    assert!(OWNED_INODES.with_borrow(Vec::is_empty));
    assert_eq!(fs::read(victim).unwrap(), b"untouched");
}

thread_local! {
    static OWNED_INODES: std::cell::RefCell<Vec<u64>> = const { std::cell::RefCell::new(Vec::new()) };
    static REPLACEMENT: std::cell::RefCell<Option<(PathBuf, PathBuf, u8)>> = const { std::cell::RefCell::new(None) };
}

fn record_chown(file: &File, _: Option<u32>, _: Option<u32>) -> io::Result<()> {
    OWNED_INODES.with_borrow_mut(|inodes| inodes.push(file.metadata().unwrap().ino()));
    file.sync_all()
}

#[test]
fn ownership_uses_validated_open_handles_across_file_and_directory_swaps() {
    // Before open: symlinks and hardlinks are rejected. After open: replacing the
    // directory entry cannot redirect fchown away from the validated original inode.
    for directory in [false, true] {
        for stage in 0..4 {
            let (_temp, mut files) = fixture();
            files
                .protect_config(Path::new("/data/config.toml"))
                .unwrap();
            let path = if directory {
                Path::new("/data")
            } else {
                Path::new("/data/config.toml")
            };
            if directory {
                files.directory(Path::new("/victim"), true).unwrap();
            } else {
                files.publish(Path::new("/victim"), b"untouched").unwrap();
            }
            let original = fs::metadata(files.resolve(path)).unwrap().ino();
            let victim = files.resolve(Path::new("/victim"));
            let victim_inode = fs::metadata(&victim).unwrap().ino();
            REPLACEMENT.with_borrow_mut(|replacement| {
                *replacement = Some((files.resolve(path), victim.clone(), stage));
            });
            OWNED_INODES.with_borrow_mut(Vec::clear);
            files.calls.open = |parent, name, is_directory| {
                let attack = REPLACEMENT.with_borrow_mut(|replacement| {
                    if replacement.as_ref().unwrap().0.file_name() == Some(name) {
                        replacement.take()
                    } else {
                        None
                    }
                });
                if let Some((path, victim, stage)) = attack {
                    let opened = if stage % 2 == 1 {
                        Some((Calls::native().open)(parent, name, is_directory).unwrap())
                    } else {
                        None
                    };
                    fs::rename(&path, path.with_extension("kept")).unwrap();
                    if stage >= 2 && !is_directory {
                        fs::hard_link(victim, &path).unwrap();
                    } else {
                        std::os::unix::fs::symlink(victim, &path).unwrap();
                    }
                    if let Some(opened) = opened {
                        return Ok(opened);
                    }
                }
                (Calls::native().open)(parent, name, is_directory)
            };
            files.calls.chown = record_chown;
            assert_eq!(
                files.chown(path, files.uid, 1, directory).is_ok(),
                stage % 2 == 1
            );
            OWNED_INODES.with_borrow(|inodes| {
                assert!(!inodes.contains(&victim_inode));
                assert_eq!(
                    inodes.as_slice(),
                    if stage % 2 == 1 {
                        std::slice::from_ref(&original)
                    } else {
                        &[]
                    }
                );
            });
            if !directory {
                assert_eq!(fs::read(victim).unwrap(), b"untouched");
            }
        }
    }
}

#[test]
fn replacement_after_opened_metadata_validation_cannot_redirect_chown() {
    for (directory, hardlink) in [(false, false), (false, true), (true, false)] {
        let (_temp, mut files) = fixture();
        files
            .protect_config(Path::new("/data/config.toml"))
            .unwrap();
        let path = if directory {
            Path::new("/data")
        } else {
            Path::new("/data/config.toml")
        };
        let victim = if directory {
            files.directory(Path::new("/victim"), true).unwrap();
            files.resolve(Path::new("/victim"))
        } else {
            files.publish(Path::new("/victim"), b"untouched").unwrap();
            files.resolve(Path::new("/victim"))
        };
        let original = fs::metadata(files.resolve(path)).unwrap().ino();
        REPLACEMENT.with_borrow_mut(|replacement| {
            *replacement = Some((files.resolve(path), victim.clone(), u8::from(hardlink)));
        });
        OWNED_INODES.with_borrow_mut(Vec::clear);
        files.calls.file_metadata = |file| {
            let meta = file.metadata().unwrap();
            let attack = REPLACEMENT.with_borrow_mut(|replacement| {
                if meta.ino()
                    == fs::metadata(&replacement.as_ref().unwrap().0)
                        .unwrap()
                        .ino()
                {
                    replacement.take()
                } else {
                    None
                }
            });
            if let Some((path, victim, hardlink)) = attack {
                fs::rename(&path, path.with_extension("kept")).unwrap();
                if hardlink == 1 {
                    fs::hard_link(victim, path).unwrap();
                } else {
                    std::os::unix::fs::symlink(victim, path).unwrap();
                }
            }
            Ok(meta)
        };
        files.calls.chown = record_chown;
        files.chown(path, files.uid, 1, directory).unwrap();
        OWNED_INODES.with_borrow(|inodes| assert_eq!(inodes.as_slice(), &[original]));
        if !directory {
            assert_eq!(fs::read(victim).unwrap(), b"untouched");
        }
    }
}

#[test]
fn descriptor_ownership_rejects_unsafe_metadata_and_native_failures() {
    let (_temp, mut files) = fixture();
    let path = Path::new("/data/config.toml");
    files.protect_config(path).unwrap();
    let calls = files.calls;
    for mode in [0o640, 0o606] {
        fs::set_permissions(files.resolve(path), fs::Permissions::from_mode(mode)).unwrap();
        assert!(files.chown(path, files.uid, 1, false).is_err());
    }
    fs::set_permissions(files.resolve(path), fs::Permissions::from_mode(0o600)).unwrap();
    assert!(
        files
            .chown(Path::new("/data"), files.uid, 1, false)
            .is_err()
    );
    assert!(files.chown(path, files.uid, 1, true).is_err());
    assert!(
        files
            .chown(Path::new("relative"), files.uid, 1, false)
            .is_err()
    );
    assert!(files.chown(Path::new("/"), files.uid, 1, false).is_err());
    files.calls.open = |_, _, _| Err(failure());
    assert!(files.chown(path, files.uid, 1, false).is_err());
    files.calls = calls;
    files.calls.file_metadata = |_| Err(failure());
    assert!(files.chown(path, files.uid, 1, false).is_err());
    files.calls = calls;
    files.uid = u32::MAX;
    assert!(files.chown(path, files.uid, 1, false).is_err());
    files.uid = fs::metadata(&files.root).unwrap().uid();
    fs::set_permissions(
        files.resolve(Path::new("/data")),
        fs::Permissions::from_mode(0o777),
    )
    .unwrap();
    assert!(files.chown(path, files.uid, 1, false).is_err());
    files.root.push("missing");
    assert!(files.chown(path, files.uid, 1, false).is_err());
}

static DIRECTORY_SYNCS: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

#[test]
fn every_new_ancestor_is_published_and_retry_repairs_parent_syncs() {
    let (_temp, mut files) = fixture();
    files.calls.sync_directory = |path| {
        DIRECTORY_SYNCS.lock().unwrap().push(path.into());
        Ok(())
    };
    let expected = vec![
        files.root.clone(),
        files.root.join("one"),
        files.root.join("one/two"),
    ];
    files.directory(Path::new("/one/two/three"), true).unwrap();
    assert_eq!(*DIRECTORY_SYNCS.lock().unwrap(), expected);
    DIRECTORY_SYNCS.lock().unwrap().clear();
    files.directory(Path::new("/one/two/three"), true).unwrap();
    assert_eq!(*DIRECTORY_SYNCS.lock().unwrap(), expected);
    DIRECTORY_SYNCS.lock().unwrap().clear();
    files.directory(Path::new("/one/two/three"), false).unwrap();
    assert!(DIRECTORY_SYNCS.lock().unwrap().is_empty());
}

#[test]
fn removal_failure_does_not_remove_other_files() {
    for step in 0..2 {
        let (_temp, mut files) = fixture();
        files.publish(Path::new("/data/unit"), b"owned").unwrap();
        if step == 0 {
            files.calls.remove = |_| Err(failure());
        } else {
            files.calls.sync_directory = |_| Err(failure());
        }
        assert!(files.remove(Path::new("/data/unit"), b"owned").is_err());
        assert_eq!(files.resolve(Path::new("/data/unit")).exists(), step == 0);
        if step == 1 {
            assert!(files.remove(Path::new("/data/unit"), b"owned").is_err());
        }
    }
    let (_temp, files) = fixture();
    files.remove(Path::new("/absent/unit"), b"owned").unwrap();
    files.directory(Path::new("/data"), true).unwrap();
    fs::set_permissions(
        files.resolve(Path::new("/data")),
        fs::Permissions::from_mode(0o777),
    )
    .unwrap();
    assert!(files.executable(Path::new("/data/unit"), false).is_err());
}

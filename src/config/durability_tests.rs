use super::{
    Config, Error, FileStorage, Storage, check_supported_permissions, config_parent,
    create_directories, default_path, sync_directory,
};
use std::io;
use std::path::{Path, PathBuf};

struct FailingStorage(u8);

impl Storage for FailingStorage {
    fn new_file(&mut self, parent: &Path) -> io::Result<tempfile::NamedTempFile> {
        if self.0 == 0 {
            Err(io::Error::other("synthetic"))
        } else {
            FileStorage.new_file(parent)
        }
    }
    fn serialize(&mut self, config: &Config) -> Result<String, toml::ser::Error> {
        if self.0 == 1 {
            Err(serde::ser::Error::custom("synthetic"))
        } else {
            FileStorage.serialize(config)
        }
    }
    fn write(&mut self, file: &mut tempfile::NamedTempFile, bytes: &[u8]) -> io::Result<()> {
        if self.0 == 2 {
            Err(io::Error::other("synthetic"))
        } else {
            FileStorage.write(file, bytes)
        }
    }
    fn sync(&mut self, file: &tempfile::NamedTempFile) -> io::Result<()> {
        if self.0 == 3 {
            Err(io::Error::other("synthetic"))
        } else {
            FileStorage.sync(file)
        }
    }
}

#[test]
fn storage_failures_are_propagated() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("config.toml");
    let config = Config::generate()?;
    assert!(matches!(
        config.create_with_storage(&path, &mut sync_directory, false, &mut FileStorage),
        Err(Error::Permissions)
    ));
    for stage in 0..4 {
        assert!(
            config
                .create_with_storage(&path, &mut sync_directory, true, &mut FailingStorage(stage))
                .is_err()
        );
        assert!(!path.exists());
    }
    Ok(())
}

#[test]
fn first_run_failures_are_propagated() {
    let path = Path::new("config.toml");
    let missing = || Error::Io(io::ErrorKind::NotFound.into());
    let exists = || Error::Io(io::ErrorKind::AlreadyExists.into());
    let failure = || io::Error::other("synthetic failure");
    let result = Config::load_or_create_with(
        path,
        &mut |_| Err(missing()),
        &mut || Err(Error::Random),
        &mut |_, _| Ok(()),
        &mut |_| Ok(()),
    );
    assert!(matches!(result, Err(Error::Random)));
    let mut calls = 0;
    let result = Config::load_or_create_with(
        path,
        &mut |_| {
            calls += 1;
            if calls == 1 {
                Err(missing())
            } else {
                Err(Error::Permissions)
            }
        },
        &mut Config::generate,
        &mut |_, _| Err(exists()),
        &mut |_| Ok(()),
    );
    assert!(matches!(result, Err(Error::Permissions)));
    let mut calls = 0;
    let result = Config::load_or_create_with(
        path,
        &mut |_| {
            calls += 1;
            if calls == 1 {
                Err(missing())
            } else {
                Config::generate()
            }
        },
        &mut Config::generate,
        &mut |_, _| Err(exists()),
        &mut |_| Err(failure()),
    );
    assert!(matches!(result, Err(Error::Io(_))));
    let result = Config::load_or_create_with(
        path,
        &mut |_| Config::generate(),
        &mut Config::generate,
        &mut |_, _| Ok(()),
        &mut |_| Err(failure()),
    );
    assert!(matches!(result, Err(Error::Io(_))));
}

#[test]
fn random_failures_are_propagated() {
    let encoded = super::random_hex(1, &mut |bytes| {
        bytes.fill(0xfa);
        Ok(())
    });
    assert!(matches!(encoded, Ok(ref value) if value == "fa"));
    let encoded = super::random_hex(1, &mut |bytes| {
        bytes.fill(0x09);
        Ok(())
    });
    assert!(matches!(encoded, Ok(ref value) if value == "09"));
    for fail_at in 0..3 {
        let mut calls = 0;
        let result = Config::generate_with(&mut |bytes| {
            let fail = calls == fail_at;
            calls += 1;
            if fail {
                Err(getrandom::Error::UNSUPPORTED)
            } else {
                bytes.fill(42);
                Ok(())
            }
        });
        assert!(matches!(result, Err(Error::Random)));
    }
}

#[test]
fn covers_default_and_relative_paths() {
    let _ = default_path();
    assert!(check_supported_permissions(true).is_ok());
    assert!(matches!(
        check_supported_permissions(false),
        Err(Error::Permissions)
    ));
    assert_eq!(config_parent(Path::new("config.toml")), Path::new("."));
}

#[cfg(unix)]
#[test]
fn permission_metadata_failure_is_propagated() {
    assert!(matches!(
        super::check_permission_metadata(&mut || Err(io::Error::other("synthetic"))),
        Err(Error::Io(_))
    ));
}

#[test]
fn directory_created_by_competitor_is_synced() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let directory = temp.path().join("new");
    let mut created = false;
    create_directories(&directory, &mut |path| {
        if !created {
            std::fs::create_dir(&directory)?;
            created = true;
        }
        sync_directory(path)
    })?;
    assert!(directory.is_dir());
    Ok(())
}

#[test]
fn directory_errors_are_propagated() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let file = temp.path().join("file");
    std::fs::write(&file, b"")?;
    assert!(create_directories(&file, &mut sync_directory).is_err());
    assert!(sync_directory(&temp.path().join("missing")).is_err());
    let mut fail = |_: &Path| Err(io::Error::other("synthetic failure"));
    assert!(create_directories(Path::new("relative"), &mut fail).is_err());
    Ok(())
}

#[test]
fn load_or_create_propagates_io_and_validation_failures() -> Result<(), Box<dyn std::error::Error>>
{
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("config.toml");
    let mut config = Config::generate()?;
    config.device_name.clear();
    assert!(matches!(
        config.create(&path),
        Err(Error::Validation { .. })
    ));
    let config = Config::generate()?;
    config.create(&path)?;
    std::fs::write(&path, "invalid = [")?;
    assert!(matches!(
        Config::load_or_create(&path),
        Err(Error::Parse(_))
    ));
    let blocked = temp.path().join("missing").join("config.toml");
    std::fs::write(temp.path().join("missing"), b"")?;
    assert!(matches!(
        Config::load_or_create(&blocked),
        Err(Error::Io(_))
    ));
    let long_path = temp.path().join("new").join("a".repeat(300));
    assert!(matches!(
        Config::load_or_create(&long_path),
        Err(Error::Io(_))
    ));
    let mut invalid = Config::generate()?;
    invalid.device_name.clear();
    std::fs::write(&path, toml::to_string_pretty(&invalid)?)?;
    assert!(matches!(Config::load(&path), Err(Error::Validation { .. })));
    Ok(())
}

#[test]
fn syncs_each_new_directory_parent_and_published_file_parent()
-> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let first = temp.path().join("first");
    let second = first.join("second");
    let path = second.join("config.toml");
    let mut synced = Vec::new();
    Config::generate()?.create_with_sync(&path, &mut |directory| {
        synced.push(directory.to_path_buf());
        sync_directory(directory)
    })?;
    let temp_parent = temp.path().parent().ok_or("missing temp parent")?;
    assert_eq!(
        synced,
        vec![
            temp_parent.to_path_buf(),
            temp.path().to_path_buf(),
            first,
            second
        ]
    );
    Config::load(&path)?;
    Ok(())
}

#[test]
fn propagates_parent_sync_failure_after_publication() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("config.toml");
    let error = Config::generate()?.create_with_sync(&path, &mut |dir| {
        if dir == temp.path() {
            Err(io::Error::other("synthetic sync failure"))
        } else {
            sync_directory(dir)
        }
    });
    assert!(
        matches!(error, Err(Error::Io(ref err)) if err.to_string().contains("synthetic sync failure"))
    );
    assert!(path.is_file());
    Config::load(&path)?;
    Ok(())
}

#[test]
fn propagates_new_directory_sync_failure_before_publication()
-> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("new").join("config.toml");
    let mut synced = Vec::<PathBuf>::new();
    let error = Config::generate()?.create_with_sync(&path, &mut |dir| {
        synced.push(dir.to_path_buf());
        if dir == temp.path() {
            Err(io::Error::other("synthetic sync failure"))
        } else {
            sync_directory(dir)
        }
    });
    assert!(matches!(error, Err(Error::Io(_))));
    assert_eq!(synced.last().map(PathBuf::as_path), Some(temp.path()));
    assert!(!path.exists());
    Ok(())
}

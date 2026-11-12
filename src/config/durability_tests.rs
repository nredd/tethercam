use super::{Config, Error, sync_directory};
use std::io;
use std::path::PathBuf;

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

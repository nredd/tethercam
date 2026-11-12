//! Protected filesystem operations, rooted in `/` or an isolated test directory.
//!
//! References: `std::fs`, `tempfile::NamedTempFile::persist_noclobber`.

use rustix::fs::{Gid, Mode, OFlags, Uid, fchown, openat};
use std::ffi::OsStr;
use std::fs::{self, File, Metadata};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Component, Path, PathBuf};
use tempfile::{NamedTempFile, PersistError};

use super::Error;

// Control files are tiny templates; never treat an unrelated large file as one.
const CONTROL_BYTES: u16 = 65_535;

#[cfg(test)]
mod tests;

/// Concrete syscall table: tests inject one OS failure, not an alternate filesystem model.
#[derive(Clone, Copy)]
pub(super) struct Calls {
    pub metadata: fn(&Path) -> io::Result<Metadata>,
    pub read: fn(&Path) -> io::Result<Vec<u8>>,
    pub mkdir: fn(&Path) -> io::Result<()>,
    pub temporary: fn(&Path) -> io::Result<NamedTempFile>,
    pub write: fn(&mut NamedTempFile, &[u8]) -> io::Result<()>,
    pub sync_file: fn(&NamedTempFile) -> io::Result<()>,
    pub persist: fn(NamedTempFile, &Path) -> Result<File, PersistError>,
    pub sync_directory: fn(&Path) -> io::Result<()>,
    pub remove: fn(&Path) -> io::Result<()>,
    pub open: fn(&File, &OsStr, bool) -> io::Result<File>,
    pub file_metadata: fn(&File) -> io::Result<Metadata>,
    pub chown: fn(&File, Option<u32>, Option<u32>) -> io::Result<()>,
}

impl Calls {
    fn native() -> Self {
        Self {
            metadata: |path| fs::symlink_metadata(path),
            read: |path| {
                let mut bytes = Vec::new();
                File::open(path)?
                    .take(u64::from(CONTROL_BYTES) + 1)
                    .read_to_end(&mut bytes)?;
                Ok(bytes)
            },
            mkdir: |path| fs::DirBuilder::new().mode(0o700).create(path),
            temporary: |path| NamedTempFile::new_in(path),
            write: <NamedTempFile as Write>::write_all,
            sync_file: |file| file.as_file().sync_all(),
            persist: |file, path| file.persist_noclobber(path),
            sync_directory: |path| File::open(path)?.sync_all(),
            remove: |path| fs::remove_file(path),
            open: |directory, name, is_directory| {
                let mut flags =
                    OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
                if is_directory {
                    flags |= OFlags::DIRECTORY;
                }
                Ok(openat(directory, name, flags, Mode::empty())?.into())
            },
            file_metadata: File::metadata,
            chown: |file, uid, gid| {
                fchown(file, uid.map(Uid::from_raw), gid.map(Gid::from_raw)).map_err(Into::into)
            },
        }
    }
}

pub(super) struct Files {
    pub root: PathBuf,
    pub uid: u32,
    pub calls: Calls,
}

impl Files {
    pub(super) fn new(root: PathBuf, uid: u32) -> Self {
        Self {
            root,
            uid,
            calls: Calls::native(),
        }
    }

    pub(super) fn resolve(&self, path: &Path) -> PathBuf {
        self.root.join(path.strip_prefix("/").unwrap_or(path))
    }

    fn inspect(&self, path: &Path, private: bool) -> Result<Option<Metadata>, Error> {
        match (self.calls.metadata)(path) {
            Ok(meta) => {
                let mask = if private { 0o077 } else { 0o022 };
                if meta.file_type().is_symlink()
                    || (meta.uid() != 0 && meta.uid() != self.uid)
                    || meta.mode() & mask != 0
                {
                    return Err(Error::Conflict);
                }
                Ok(Some(meta))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub(super) fn directory(&self, path: &Path, create: bool) -> Result<(), Error> {
        super::templates::path(path)?;
        let mut current = self.root.clone();
        for part in path.components() {
            if let Component::Normal(name) = part {
                let containing = current.clone();
                current.push(name);
                match self.inspect(&current, false)? {
                    Some(meta) if meta.is_dir() => {}
                    Some(_) => return Err(Error::Conflict),
                    None if create => (self.calls.mkdir)(&current)?,
                    None => return Ok(()),
                }
                if create {
                    // Sync existing entries too: a repeat invocation must repair an earlier
                    // mkdir that succeeded just before its parent sync failed.
                    (self.calls.sync_directory)(&containing)?;
                }
            }
        }
        Ok(())
    }

    fn regular(&self, path: &Path) -> Result<Option<Metadata>, Error> {
        self.directory(parent(path)?, false)?;
        match self.inspect(&self.resolve(path), true)? {
            Some(meta) if meta.is_file() && meta.nlink() == 1 => Ok(Some(meta)),
            Some(_) => Err(Error::Conflict),
            None => Ok(None),
        }
    }

    pub(super) fn read(&self, path: &Path) -> Result<Option<Vec<u8>>, Error> {
        if self.regular(path)?.is_none() {
            return Ok(None);
        }
        let bytes = (self.calls.read)(&self.resolve(path))?;
        if bytes.len() > usize::from(CONTROL_BYTES) {
            return Err(Error::Conflict);
        }
        Ok(Some(bytes))
    }

    pub(super) fn matches(&self, path: &Path, expected: &[u8]) -> Result<bool, Error> {
        match self.read(path)? {
            Some(bytes) if bytes == expected => Ok(true),
            Some(_) => Err(Error::Conflict),
            None => Ok(false),
        }
    }

    pub(super) fn publish(&self, path: &Path, expected: &[u8]) -> Result<(), Error> {
        let directory = parent(path)?;
        let exists = self.matches(path, expected)?;
        self.directory(directory, true)?;
        let parent = self.resolve(directory);
        if !exists {
            let mut file = (self.calls.temporary)(&parent)?;
            (self.calls.write)(&mut file, expected)?;
            (self.calls.sync_file)(&file)?;
            (self.calls.persist)(file, &self.resolve(path))
                .map_err(|error| Error::Io(error.error))?;
        }
        // Also repair a previous publication whose rename succeeded before directory sync failed.
        (self.calls.sync_directory)(&parent)?;
        Ok(())
    }

    pub(super) fn remove(&self, path: &Path, expected: &[u8]) -> Result<(), Error> {
        let directory = parent(path)?;
        let exists = self.matches(path, expected)?;
        if exists {
            (self.calls.remove)(&self.resolve(path))?;
        }
        // Retry an interrupted removal's directory sync, without creating a missing directory.
        match (self.calls.sync_directory)(&self.resolve(directory)) {
            Ok(()) => Ok(()),
            Err(error) if !exists && error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    pub(super) fn executable(&self, path: &Path, system: bool) -> Result<(), Error> {
        let directory = parent(path)?;
        self.directory(directory, false)?;
        // The system account may not exist yet. Require other access instead of guessing
        // its future UID/group membership; perform this read-only check before publication.
        let mut current = self.root.clone();
        self.traverse(&current, system)?;
        for part in directory.components() {
            if let Component::Normal(name) = part {
                current.push(name);
                self.traverse(&current, system)?;
            }
        }
        let meta = self
            .inspect(&self.resolve(path), false)?
            .ok_or(Error::Invalid)?;
        if !meta.is_file() || !accessible(&meta, self.uid, system, 0o5) {
            return Err(Error::ExecutableAccess);
        }
        Ok(())
    }

    fn traverse(&self, path: &Path, system: bool) -> Result<(), Error> {
        let meta = self.inspect(path, false)?.ok_or(Error::Invalid)?;
        if !meta.is_dir() || !accessible(&meta, self.uid, system, 0o1) {
            return Err(Error::ExecutableAccess);
        }
        Ok(())
    }

    pub(super) fn protect_config(&self, path: &Path) -> Result<(), Error> {
        self.directory(parent(path)?, true)?;
        let _ = self.regular(path)?;
        let _ = crate::config::Config::load_or_create(&self.resolve(path))?;
        Ok(())
    }

    pub(super) fn chown(
        &self, path: &Path, uid: u32, gid: u32, directory: bool,
    ) -> Result<(), Error> {
        super::templates::path(path)?;
        let (parent, name) = path.parent().zip(path.file_name()).ok_or(Error::Invalid)?;
        // Walk from the trusted root through no-follow directory handles. A service-owned
        // leaf directory may replace its entries, but cannot redirect an opened descriptor.
        let mut containing = File::open(&self.root)?;
        self.opened(&containing, true)?;
        for component in parent.components() {
            if let Component::Normal(name) = component {
                containing = (self.calls.open)(&containing, name, true)?;
                self.opened(&containing, true)?;
            }
        }
        let file = (self.calls.open)(&containing, name, directory)?;
        self.opened(&file, directory)?;
        (self.calls.chown)(&file, Some(uid), Some(gid))?;
        Ok(())
    }

    fn opened(&self, file: &File, directory: bool) -> Result<(), Error> {
        let meta = (self.calls.file_metadata)(file)?;
        let kind = if directory {
            meta.is_dir()
        } else {
            meta.is_file()
        };
        let mask = if directory { 0o022 } else { 0o077 };
        if !kind
            || (meta.uid() != 0 && meta.uid() != self.uid)
            || meta.mode() & mask != 0
            || (meta.is_file() && meta.nlink() != 1)
        {
            return Err(Error::Conflict);
        }
        Ok(())
    }

    pub(super) fn log(&self, path: &Path) -> Result<(), Error> {
        if self.regular(path)?.is_none() {
            self.publish(path, b"")?;
        }
        Ok(())
    }
}

fn accessible(meta: &Metadata, uid: u32, system: bool, required: u32) -> bool {
    let shift = if !system && meta.uid() == uid { 6 } else { 0 };
    (meta.mode() >> shift) & required == required
}

pub(super) fn parent(path: &Path) -> Result<&Path, Error> {
    path.parent().ok_or(Error::Invalid)
}

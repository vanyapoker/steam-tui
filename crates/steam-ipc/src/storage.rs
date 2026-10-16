//! Shared local RemoteStorage cache. All descendants are opened relative to
//! directory descriptors with O_NOFOLLOW. Metadata is never a cloud save.
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::CString,
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{DirBuilderExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

pub const MAX_FILE: usize = 256 * 1024 * 1024;
pub const META: &str = ".steam-tui-cloud-metadata";
static NEXT: AtomicU64 = AtomicU64::new(0);
fn invalid() -> io::Error {
    io::Error::other("invalid RemoteStorage path or data")
}

pub fn name(value: &str) -> io::Result<String> {
    let value = value.replace('\\', "/").to_lowercase();
    if value.is_empty()
        || value.len() >= 260
        || value.starts_with('%')
        || value
            .chars()
            .any(|c| c.is_control() || "*:<>\"|?".contains(c))
        || value
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == ".." || p.starts_with(".steam-tui-cloud-"))
    {
        return Err(invalid());
    }
    Ok(value)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Metadata {
    pub enabled: bool,
    pub forgotten: BTreeSet<String>,
    pub deleted: BTreeSet<String>,
    pub platforms: BTreeMap<String, u32>,
    pub dirty: BTreeSet<String>,
}
impl Default for Metadata {
    fn default() -> Self {
        Self {
            enabled: true,
            forgotten: BTreeSet::new(),
            deleted: BTreeSet::new(),
            platforms: BTreeMap::new(),
            dirty: BTreeSet::new(),
        }
    }
}
impl Metadata {
    pub fn syncs(&self, name: &str) -> bool {
        self.enabled
            && !self.forgotten.contains(name)
            && !self.deleted.contains(name)
            && self.platforms.get(name).copied().unwrap_or(u32::MAX) != 0
    }
}

pub struct Store {
    root: File,
    pub path: PathBuf,
}
pub struct Lock(File);
pub struct ReadSnapshot {
    dir: File,
    file: File,
    temp: CString,
    pub length: usize,
}
impl ReadSnapshot {
    pub fn read_into(&mut self, bytes: &mut [u8]) -> io::Result<()> {
        if bytes.len() < self.length {
            return Err(invalid());
        }
        self.file.seek(SeekFrom::Start(0))?;
        self.file.read_exact(&mut bytes[..self.length])
    }
}
impl Drop for ReadSnapshot {
    fn drop(&mut self) {
        unsafe {
            libc::unlinkat(self.dir.as_raw_fd(), self.temp.as_ptr(), 0);
        }
    }
}
pub struct WriteStream {
    dir: File,
    file: File,
    temp: CString,
    destination: CString,
    length: usize,
    committed: bool,
    failed: bool,
}
impl WriteStream {
    pub fn len(&self) -> usize {
        self.length
    }
    pub fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        if bytes.len() > MAX_FILE.saturating_sub(self.length) || self.committed || self.failed {
            return Err(invalid());
        }
        if let Err(error) = self.file.write_all(bytes) {
            self.failed = true;
            return Err(error);
        }
        self.length += bytes.len();
        Ok(())
    }
    pub fn commit(&mut self) -> io::Result<()> {
        if self.committed || self.length == 0 || self.failed {
            return Err(invalid());
        }
        self.file.sync_all()?;
        if unsafe {
            libc::renameat(
                self.dir.as_raw_fd(),
                self.temp.as_ptr(),
                self.dir.as_raw_fd(),
                self.destination.as_ptr(),
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        self.dir.sync_all()?;
        self.committed = true;
        Ok(())
    }
}
impl Drop for WriteStream {
    fn drop(&mut self) {
        if !self.committed {
            unsafe {
                libc::unlinkat(self.dir.as_raw_fd(), self.temp.as_ptr(), 0);
            }
        }
    }
}
impl Drop for Lock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}
impl Store {
    pub fn snapshot(&self, value: &str, offset: u64, count: usize) -> io::Result<ReadSnapshot> {
        if count > MAX_FILE {
            return Err(invalid());
        }
        let (dir, name) = self.parent(value, false)?;
        let mut source = self.open_at(
            &dir,
            name.to_str().map_err(|_| invalid())?,
            libc::O_RDONLY | libc::O_NONBLOCK,
            0,
        )?;
        let before = source.metadata()?;
        if !before.is_file() || before.len() > MAX_FILE as u64 || offset > before.len() {
            return Err(invalid());
        }
        source.seek(SeekFrom::Start(offset))?;
        let temp = format!(
            ".steam-tui-cloud-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let file = self.open_at(
            &self.root,
            &temp,
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
            0o600,
        )?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut snapshot = ReadSnapshot {
            dir: self.root.try_clone()?,
            file,
            temp: CString::new(temp).unwrap(),
            length: 0,
        };
        let limit = (before.len() - offset).min(count as u64);
        snapshot.length = io::copy(
            &mut Read::by_ref(&mut source).take(limit),
            &mut snapshot.file,
        )? as usize;
        let after = source.metadata()?;
        if snapshot.length as u64 != limit
            || before.len() != after.len()
            || before.modified()? != after.modified()?
        {
            return Err(io::Error::other("save changed during async read"));
        }
        Ok(snapshot)
    }
    pub fn stream(&self, value: &str) -> io::Result<WriteStream> {
        let (dir, destination) = self.parent(value, true)?;
        match self.stat(value) {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let temp = format!(
            ".steam-tui-cloud-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let file = self.open_at(
            &dir,
            &temp,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
            0o600,
        )?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(WriteStream {
            dir,
            file,
            temp: CString::new(temp).unwrap(),
            destination,
            length: 0,
            committed: false,
            failed: false,
        })
    }
    pub fn open(root: &Path) -> io::Result<Self> {
        if !root.is_absolute() {
            return Err(invalid());
        }
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(root)?;
        Ok(Self {
            root: OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(root)?,
            path: root.to_owned(),
        })
    }
    pub fn lock(&self) -> io::Result<Lock> {
        self.acquire_lock(true)
    }
    pub fn lock_wait(&self) -> io::Result<Lock> {
        self.acquire_lock(false)
    }
    fn acquire_lock(&self, nonblocking: bool) -> io::Result<Lock> {
        let file = self.open_at(
            &self.root,
            ".steam-tui-cloud-lock",
            libc::O_RDWR | libc::O_CREAT,
            0o600,
        )?;
        if unsafe {
            libc::flock(
                file.as_raw_fd(),
                libc::LOCK_EX | if nonblocking { libc::LOCK_NB } else { 0 },
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Lock(file))
    }
    fn open_at(&self, dir: &File, name: &str, flags: i32, mode: u32) -> io::Result<File> {
        let name = CString::new(name).map_err(|_| invalid())?;
        let fd = unsafe {
            libc::openat(
                dir.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                mode,
            )
        };
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(unsafe { File::from_raw_fd(fd) })
        }
    }
    fn parent(&self, value: &str, create: bool) -> io::Result<(File, CString)> {
        let value = name(value)?;
        let mut parts = value.split('/').peekable();
        let mut dir = self.root.try_clone()?;
        while let Some(part) = parts.next() {
            if parts.peek().is_none() {
                return Ok((dir, CString::new(part).map_err(|_| invalid())?));
            }
            if create {
                let component = CString::new(part).map_err(|_| invalid())?;
                if unsafe { libc::mkdirat(dir.as_raw_fd(), component.as_ptr(), 0o700) } != 0
                    && io::Error::last_os_error().kind() != io::ErrorKind::AlreadyExists
                {
                    return Err(io::Error::last_os_error());
                }
            }
            dir = self.open_at(&dir, part, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
        }
        Err(invalid())
    }
    pub fn read(&self, value: &str) -> io::Result<Vec<u8>> {
        self.read_range(value, 0, MAX_FILE)
    }

    pub fn read_range(&self, value: &str, offset: u64, count: usize) -> io::Result<Vec<u8>> {
        let (dir, file) = self.parent(value, false)?;
        let mut file = self.open_at(
            &dir,
            file.to_str().map_err(|_| invalid())?,
            libc::O_RDONLY | libc::O_NONBLOCK,
            0,
        )?;
        if !file.metadata()?.is_file() || file.metadata()?.len() > MAX_FILE as u64 {
            return Err(invalid());
        }
        if offset > file.metadata()?.len() || count > MAX_FILE {
            return Err(invalid());
        }
        file.seek(SeekFrom::Start(offset))?;
        let mut bytes = Vec::new();
        file.take(count as u64).read_to_end(&mut bytes)?;
        if bytes.len() > MAX_FILE {
            return Err(invalid());
        }
        Ok(bytes)
    }
    pub fn stat(&self, value: &str) -> io::Result<fs::Metadata> {
        let (dir, file) = self.parent(value, false)?;
        let file = self.open_at(
            &dir,
            file.to_str().map_err(|_| invalid())?,
            libc::O_RDONLY | libc::O_NONBLOCK,
            0,
        )?;
        let meta = file.metadata()?;
        if !meta.is_file() {
            return Err(invalid());
        }
        Ok(meta)
    }
    fn replace_at(&self, dir: &File, destination: &CString, bytes: &[u8]) -> io::Result<()> {
        let temp = format!(
            ".steam-tui-cloud-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let temp_c = CString::new(temp.as_str()).unwrap();
        let result = (|| {
            let mut file = self.open_at(
                dir,
                &temp,
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
                0o600,
            )?;
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                return Err(io::Error::last_os_error());
            }
            file.write_all(bytes)?;
            file.sync_all()?;
            if unsafe {
                libc::renameat(
                    dir.as_raw_fd(),
                    temp_c.as_ptr(),
                    dir.as_raw_fd(),
                    destination.as_ptr(),
                )
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
            dir.sync_all()
        })();
        if result.is_err() {
            unsafe {
                libc::unlinkat(dir.as_raw_fd(), temp_c.as_ptr(), 0);
            }
        }
        result
    }
    pub fn write(&self, value: &str, bytes: &[u8]) -> io::Result<()> {
        if bytes.len() > MAX_FILE {
            return Err(invalid());
        }
        let (dir, dest) = self.parent(value, true)?;
        // Refuse existing symlink/nonregular entries before replacing them.
        match self.stat(value) {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        self.replace_at(&dir, &dest, bytes)
    }
    pub fn delete(&self, value: &str) -> io::Result<()> {
        self.stat(value)?;
        let (dir, file) = self.parent(value, false)?;
        if unsafe { libc::unlinkat(dir.as_raw_fd(), file.as_ptr(), 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
        dir.sync_all()
    }
    pub fn metadata(&self) -> io::Result<Metadata> {
        let file = match self.open_at(&self.root, META, libc::O_RDONLY | libc::O_NONBLOCK, 0) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Metadata::default()),
            Err(e) => return Err(e),
        };
        if !file.metadata()?.is_file() || file.metadata()?.len() > 1024 * 1024 {
            return Err(invalid());
        }
        let mut bytes = Vec::new();
        file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
        serde_json::from_slice(&bytes).map_err(io::Error::other)
    }
    pub fn save_metadata(&self, metadata: &Metadata) -> io::Result<()> {
        self.replace_at(
            &self.root,
            &CString::new(META).unwrap(),
            &serde_json::to_vec(metadata)?,
        )
    }
    pub fn list(&self) -> io::Result<Vec<(String, u64)>> {
        let mut found = Vec::new();
        self.walk(&self.root, "", 0, &mut found)?;
        found.sort();
        Ok(found)
    }
    /// Recover abandoned stages after a crash. Advisory locks on live stage
    /// descriptors avoid PID/namespace guesses and protect active streams.
    pub fn cleanup_stages(&self) -> io::Result<usize> {
        let _lock = self.lock()?;
        self.clean(&self.root, 0, &mut 0)
    }
    fn clean(&self, dir: &File, depth: usize, visited: &mut usize) -> io::Result<usize> {
        if depth >= 16 {
            return Err(invalid());
        }
        let mut removed = 0;
        for entry in fs::read_dir(format!("/proc/self/fd/{}", dir.as_raw_fd()))? {
            *visited += 1;
            if *visited > 20000 {
                return Err(invalid());
            }
            let entry = entry?;
            let Some(part) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let file = match self.open_at(dir, &part, libc::O_RDONLY | libc::O_NONBLOCK, 0) {
                Ok(file) => file,
                Err(_) => continue,
            };
            let meta = file.metadata()?;
            if meta.is_dir() {
                removed += self.clean(&file, depth + 1, visited)?;
            } else if meta.is_file()
                && part
                    .strip_prefix(".steam-tui-cloud-")
                    .is_some_and(|s| s.as_bytes().first().is_some_and(u8::is_ascii_digit))
            {
                if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                    continue;
                }
                let part = CString::new(part).map_err(|_| invalid())?;
                if unsafe { libc::unlinkat(dir.as_raw_fd(), part.as_ptr(), 0) } != 0 {
                    return Err(io::Error::last_os_error());
                }
                removed += 1;
            }
        }
        if removed > 0 {
            dir.sync_all()?;
        }
        Ok(removed)
    }
    fn walk(
        &self,
        dir: &File,
        prefix: &str,
        depth: usize,
        found: &mut Vec<(String, u64)>,
    ) -> io::Result<()> {
        if depth >= 16 {
            return Err(invalid());
        }
        // /proc refers to our open directory, so a parent rename cannot redirect
        // traversal. Every child is still opened with O_NOFOLLOW.
        for entry in fs::read_dir(format!("/proc/self/fd/{}", dir.as_raw_fd()))? {
            let entry = entry?;
            let Some(part) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if part.starts_with(".steam-tui-cloud-") {
                continue;
            }
            let file = match self.open_at(dir, &part, libc::O_RDONLY | libc::O_NONBLOCK, 0) {
                Ok(f) => f,
                Err(_) => continue,
            };
            let meta = file.metadata()?;
            let path = format!("{prefix}{part}");
            if meta.is_dir() {
                self.walk(&file, &format!("{path}/"), depth + 1, found)?;
            } else if meta.is_file() {
                if found.len() >= 5000 {
                    return Err(invalid());
                }
                // Ambiguous mixed-case cache entries must be reconciled explicitly.
                if !name(&path).is_ok_and(|name| name == path) {
                    return Err(invalid());
                }
                found.push((path, meta.len()));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!(
                "steam-tui-cache-tests-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn descriptor_roots_survive_parent_renames_and_reject_symlinks() {
        let temp = Temp::new();
        let root = temp.0.join("remote");
        let store = Store::open(&root).unwrap();
        store.write("saves/slot", b"save").unwrap();
        fs::rename(&root, temp.0.join("moved")).unwrap();
        fs::create_dir_all(temp.0.join("outside/saves")).unwrap();
        fs::write(temp.0.join("outside/saves/slot"), b"secret").unwrap();
        symlink(temp.0.join("outside"), &root).unwrap();
        assert_eq!(store.read("saves/slot").unwrap(), b"save");
        store.write("saves/slot", b"updated").unwrap();
        assert_eq!(
            fs::read(temp.0.join("outside/saves/slot")).unwrap(),
            b"secret"
        );
        assert!(Store::open(&root).is_err());
        symlink(temp.0.join("outside"), temp.0.join("moved/link")).unwrap();
        assert!(store.read("link/saves/slot").is_err());
        assert!(store.write("link/saves/slot", b"bad").is_err());
        assert!(store.delete("link/saves/slot").is_err());
        assert!(store.write("../escape", b"bad").is_err());
    }
    #[test]
    fn staged_writes_are_invisible_until_commit_and_cancel_leaves_no_debris() {
        let temp = Temp::new();
        let store = Store::open(&temp.0).unwrap();
        store.write("slot", b"old").unwrap();
        {
            let mut stream = store.stream("slot").unwrap();
            stream.append(b"uncommitted").unwrap();
            assert_eq!(store.read("slot").unwrap(), b"old");
        }
        assert_eq!(store.list().unwrap(), [("slot".into(), 3)]);
        let mut stream = store.stream("slot").unwrap();
        stream.append(b"new").unwrap();
        stream.commit().unwrap();
        assert_eq!(store.read("slot").unwrap(), b"new");
        assert!(stream.commit().is_err());
        assert!(!fs::read_dir(&temp.0).unwrap().any(|e| {
            e.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".steam-tui-cloud-")
        }));
    }
    #[test]
    fn locks_serialize_metadata_and_corruption_is_never_discarded() {
        let temp = Temp::new();
        let store = Store::open(&temp.0).unwrap();
        let lock = store.lock().unwrap();
        assert!(store.lock().is_err());
        drop(lock);
        assert!(store.lock().is_ok());
        let mut metadata = Metadata::default();
        metadata.deleted.insert("slot".into());
        metadata.dirty.insert("other".into());
        store.save_metadata(&metadata).unwrap();
        assert!(!store.metadata().unwrap().syncs("slot"));
        assert_eq!(store.list().unwrap().len(), 0);
        fs::write(temp.0.join(META), b"broken").unwrap();
        assert!(store.metadata().is_err());
    }
    #[test]
    fn orphan_cleanup_preserves_locked_live_streams_and_snapshots() {
        let temp = Temp::new();
        let store = Store::open(&temp.0).unwrap();
        store.write("slot", b"old").unwrap();
        let mut stream = store.stream("slot").unwrap();
        stream.append(b"new").unwrap();
        let mut snapshot = store.snapshot("slot", 0, 3).unwrap();
        fs::write(temp.0.join(".steam-tui-cloud-99999999-123"), b"orphan").unwrap();
        assert_eq!(store.cleanup_stages().unwrap(), 1);
        stream.commit().unwrap();
        let mut bytes = [0; 3];
        snapshot.read_into(&mut bytes).unwrap();
        assert_eq!(&bytes, b"old");
        assert_eq!(store.read("slot").unwrap(), b"new");
        drop(snapshot);
        drop(stream);
        assert_eq!(store.cleanup_stages().unwrap(), 0);
    }

    #[test]
    fn reads_are_bounded_and_names_normalize_windows_separators() {
        let temp = Temp::new();
        let store = Store::open(&temp.0).unwrap();
        store.write("SAVES\\Slot.Dat", b"abcdef").unwrap();
        assert_eq!(store.read_range("saves/slot.dat", 2, 2).unwrap(), b"cd");
        assert_eq!(store.read_range("saves/slot.dat", 6, 2).unwrap(), b"");
        assert!(store.read_range("saves/slot.dat", 7, 1).is_err());
        for value in [
            "",
            "..",
            "/root",
            "a//b",
            "a/./b",
            "a\\..\\b",
            "bad:name",
            ".steam-tui-cloud-lock",
            "%Root%save",
        ] {
            assert!(name(value).is_err(), "{value}");
        }
    }
}

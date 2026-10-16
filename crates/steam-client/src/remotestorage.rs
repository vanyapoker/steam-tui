//! Local Steam RemoteStorage cache. The parent synchronizes this cache; no account
//! credentials or network upload capability are given to the game.
use super::*;
use std::{ffi::CString, io, path::PathBuf, time::UNIX_EPOCH};
use steam_ipc::storage::{self, Store};

const INVALID_STREAM: u64 = u64::MAX;
#[derive(Default)]
struct Pending {
    next: u64,
    streams: BTreeMap<u64, (String, Store, storage::WriteStream)>,
    reads: BTreeMap<u64, (storage::ReadSnapshot, usize)>,
    reading: std::collections::BTreeSet<u64>,
    read_reserved: usize,
    names: Vec<CString>,
    writes: std::collections::BTreeSet<String>,
}
static PENDING: Mutex<Pending> = Mutex::new(Pending {
    next: 0,
    streams: BTreeMap::new(),
    reads: BTreeMap::new(),
    reading: std::collections::BTreeSet::new(),
    read_reserved: 0,
    names: Vec::new(),
    writes: std::collections::BTreeSet::new(),
});
fn pending() -> std::sync::MutexGuard<'static, Pending> {
    PENDING.lock().unwrap_or_else(|e| e.into_inner())
}
fn store() -> io::Result<Store> {
    let path = std::env::var_os("STEAM_TUI_REMOTE_STORAGE")
        .ok_or_else(|| io::Error::other("RemoteStorage is not configured by the launcher"))?;
    Store::open(&PathBuf::from(path))
}
unsafe fn filename(value: *const c_char) -> io::Result<String> {
    if value.is_null() {
        return Err(io::Error::other("null file name"));
    }
    // Steam filenames are bounded, including the terminator.
    let mut bytes = Vec::new();
    for index in 0..260 {
        let byte = unsafe { value.add(index).read() } as u8;
        if byte == 0 {
            return storage::name(std::str::from_utf8(&bytes).map_err(io::Error::other)?);
        }
        bytes.push(byte);
    }
    Err(io::Error::other("file name is too long"))
}
unsafe fn input<'a>(data: *const c_void, size: i32) -> Option<&'a [u8]> {
    if size <= 0 || size as usize > storage::MAX_FILE || data.is_null() {
        return None;
    }
    Some(unsafe { std::slice::from_raw_parts(data.cast(), size as usize) })
}
fn quota() -> u64 {
    std::env::var("STEAM_TUI_REMOTE_QUOTA")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1024 * 1024 * 1024)
}
fn max_files() -> usize {
    std::env::var("STEAM_TUI_REMOTE_MAX_FILES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5000)
        .min(5000)
}
fn check_quota(
    store: &Store,
    name: &str,
    size: usize,
    quota: u64,
    max_files: usize,
) -> io::Result<()> {
    let files = store.list()?;
    let old = files.iter().find(|(n, _)| n == name).map(|(_, s)| *s);
    let used: u64 = files.iter().map(|(_, s)| *s).sum();
    if used
        .saturating_sub(old.unwrap_or(0))
        .saturating_add(size as u64)
        > quota
        || (old.is_none() && files.len() >= max_files)
    {
        return Err(io::Error::other("RemoteStorage quota exceeded"));
    }
    Ok(())
}
fn written(store: &Store, name: &str) -> io::Result<()> {
    let mut meta = store.metadata()?;
    meta.deleted.remove(name);
    meta.forgotten.remove(name);
    meta.dirty.insert(name.to_owned());
    store.save_metadata(&meta)
}
fn write_with(
    store: &Store,
    name: &str,
    bytes: &[u8],
    quota: u64,
    max_files: usize,
) -> io::Result<()> {
    let _lock = store.lock_wait()?;
    check_quota(store, name, bytes.len(), quota, max_files)?;
    store.metadata()?; // A corrupt metadata file must not be silently discarded.
    store.write(name, bytes)?;
    written(store, name)
}
struct AsyncWrite {
    store: Store,
    name: String,
    bytes: Vec<u8>,
    quota: u64,
    max_files: usize,
    call: u64,
    epoch: u64,
}
static WRITE_BYTES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
struct AsyncRead {
    store: Store,
    name: String,
    offset: u32,
    count: u32,
    call: u64,
    epoch: u64,
}
enum Job {
    Write(AsyncWrite),
    Read(AsyncRead),
}
fn worker() -> Option<&'static std::sync::mpsc::SyncSender<Job>> {
    static WORKER: OnceLock<Option<std::sync::mpsc::SyncSender<Job>>> = OnceLock::new();
    WORKER
        .get_or_init(|| {
            let (tx, rx) = std::sync::mpsc::sync_channel::<Job>(64);
            std::thread::Builder::new()
                .name("steam-cloud-io".into())
                .spawn(move || {
                    while let Ok(job) = rx.recv() {
                        match job {
                            Job::Write(job) => {
                                let result = write_with(
                                    &job.store,
                                    &job.name,
                                    &job.bytes,
                                    job.quota,
                                    job.max_files,
                                );
                                WRITE_BYTES.fetch_sub(
                                    job.bytes.len(),
                                    std::sync::atomic::Ordering::AcqRel,
                                );
                                let mut p = pending();
                                p.writes.remove(&job.name);
                                if EPOCH.load(std::sync::atomic::Ordering::Acquire) == job.epoch {
                                    callresults::complete_for(
                                        job.call,
                                        1331,
                                        (if result.is_ok() { 1i32 } else { 2i32 })
                                            .to_le_bytes()
                                            .to_vec(),
                                    );
                                }
                            }
                            Job::Read(job) => {
                                if EPOCH.load(std::sync::atomic::Ordering::Acquire) != job.epoch {
                                    continue;
                                }
                                let result = job.store.snapshot(
                                    &job.name,
                                    u64::from(job.offset),
                                    job.count as usize,
                                );
                                let mut p = pending();
                                if EPOCH.load(std::sync::atomic::Ordering::Acquire) != job.epoch {
                                    continue;
                                }
                                p.reading.remove(&job.call);
                                let mut payload = job.call.to_le_bytes().to_vec();
                                payload.extend(
                                    (if result.is_ok() { 1i32 } else { 2i32 }).to_le_bytes(),
                                );
                                payload.extend(job.offset.to_le_bytes());
                                payload.extend(
                                    (result.as_ref().map_or(0, |s| s.length) as u32).to_le_bytes(),
                                );
                                match result {
                                    Ok(snapshot) => {
                                        p.reads.insert(job.call, (snapshot, job.count as usize));
                                    }
                                    Err(_) => {
                                        p.read_reserved =
                                            p.read_reserved.saturating_sub(job.count as usize)
                                    }
                                }
                                callresults::complete_for(job.call, 1332, payload);
                            }
                        }
                    }
                })
                .ok()
                .map(|_| tx)
        })
        .as_ref()
}
pub(super) fn shutdown() {
    let mut p = pending();
    EPOCH.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    p.streams.clear();
    p.reads.clear();
    p.reading.clear();
    p.read_reserved = 0;
    p.names.clear();
    // Accepted async writes finish on their captured cache. Read snapshots and
    // stale callbacks belong to the old SDK session and are released here.
}
unsafe extern "C" fn f0(
    _: *mut c_void,
    name: *const c_char,
    data: *const c_void,
    size: i32,
) -> bool {
    let guard = pending();
    unsafe { filename(name) }
        .ok()
        .zip(unsafe { input(data, size) })
        .is_some_and(|(name, bytes)| {
            !guard.writes.contains(&name)
                && !guard.streams.values().any(|(n, _, _)| n == &name)
                && store().is_ok_and(|s| write_with(&s, &name, bytes, quota(), max_files()).is_ok())
        })
}
unsafe extern "C" fn f1(_: *mut c_void, name: *const c_char, data: *mut c_void, size: i32) -> i32 {
    if size < 0 || (size > 0 && data.is_null()) {
        return 0;
    }
    let result = (|| {
        let name = unsafe { filename(name) }?;
        store()?.read_range(&name, 0, (size as usize).min(storage::MAX_FILE))
    })();
    let Ok(bytes) = result else { return 0 };
    let count = bytes.len().min(size as usize);
    if count > 0 {
        unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), data.cast(), count) };
    }
    count as i32
}
#[allow(deprecated)] // fetch_update supports the workspace Rust 1.88 MSRV.
unsafe extern "C" fn f2(
    this: *mut c_void,
    name: *const c_char,
    data: *const c_void,
    size: u32,
) -> u64 {
    if size == 0
        || size as usize > storage::MAX_FILE
        || data.is_null()
        || unsafe { filename(name) }.is_err()
    {
        return 0;
    }
    let _ = this;
    let Ok(name) = (unsafe { filename(name) }) else {
        return 0;
    };
    let Ok(store) = store() else { return 0 };
    let Some(worker) = worker() else { return 0 };
    let mut p = pending();
    if p.writes.contains(&name) || p.streams.values().any(|(n, _, _)| n == &name) {
        return 0;
    }
    if WRITE_BYTES
        .fetch_update(
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
            |used| {
                used.checked_add(size as usize)
                    .filter(|n| *n <= storage::MAX_FILE)
            },
        )
        .is_err()
    {
        return 0;
    }
    let call = callresults::reserve();
    p.writes.insert(name.clone());
    let job = AsyncWrite {
        store,
        name: name.clone(),
        bytes: unsafe { std::slice::from_raw_parts(data.cast::<u8>(), size as usize) }.to_vec(),
        quota: quota(),
        max_files: max_files(),
        call,
        epoch: EPOCH.load(std::sync::atomic::Ordering::Acquire),
    };
    if worker.try_send(Job::Write(job)).is_err() {
        WRITE_BYTES.fetch_sub(size as usize, std::sync::atomic::Ordering::AcqRel);
        p.writes.remove(&name);
        return 0;
    }
    call
}
unsafe extern "C" fn f3(_: *mut c_void, name: *const c_char, offset: u32, count: u32) -> u64 {
    let Ok(name) = (unsafe { filename(name) }) else {
        return 0;
    };
    let Ok(store) = store() else { return 0 };
    let Some(worker) = worker() else { return 0 };
    let mut p = pending();
    if p.reads.len() + p.reading.len() >= 64
        || count as usize > storage::MAX_FILE.saturating_sub(p.read_reserved)
    {
        return 0;
    }
    let call = callresults::reserve();
    let job = Job::Read(AsyncRead {
        store,
        name,
        offset,
        count,
        call,
        epoch: EPOCH.load(std::sync::atomic::Ordering::Acquire),
    });
    p.read_reserved += count as usize;
    p.reading.insert(call);
    if worker.try_send(job).is_err() {
        p.read_reserved -= count as usize;
        p.reading.remove(&call);
        return 0;
    }
    call
}
unsafe extern "C" fn f4(_: *mut c_void, call: u64, data: *mut c_void, size: u32) -> bool {
    let mut p = pending();
    let Some((snapshot, reserved)) = p.reads.get_mut(&call) else {
        return false;
    };
    if (size as usize) < snapshot.length || (snapshot.length > 0 && data.is_null()) {
        return false;
    }
    if snapshot.length > 0 {
        let bytes = unsafe { std::slice::from_raw_parts_mut(data.cast(), snapshot.length) };
        if snapshot.read_into(bytes).is_err() {
            return false;
        }
    }
    let reserved = *reserved;
    p.reads.remove(&call);
    p.read_reserved -= reserved;
    true
}

fn alter(name: &str, delete: bool) -> io::Result<()> {
    let guard = pending();
    if guard.writes.contains(name) || guard.streams.values().any(|(n, _, _)| n == name) {
        return Err(io::Error::other("file operation in progress"));
    }
    let store = store()?;
    let _lock = store.lock_wait()?;
    store.stat(name)?;
    let mut meta = store.metadata()?;
    if delete {
        store.delete(name)?;
        meta.deleted.insert(name.to_owned());
    } else {
        meta.forgotten.insert(name.to_owned());
    }
    store.save_metadata(&meta)?;
    Ok(())
}
unsafe extern "C" fn f5(_: *mut c_void, name: *const c_char) -> bool {
    unsafe { filename(name) }.is_ok_and(|n| alter(&n, false).is_ok())
}
unsafe extern "C" fn f6(_: *mut c_void, name: *const c_char) -> bool {
    unsafe { filename(name) }.is_ok_and(|n| alter(&n, true).is_ok())
}
unsafe extern "C" fn f7(_: *mut c_void, _: *const c_char) -> u64 {
    0
} // Workshop file sharing is separate.
unsafe extern "C" fn f8(_: *mut c_void, name: *const c_char, platforms: u32) -> bool {
    (|| -> io::Result<()> {
        let name = unsafe { filename(name) }?;
        let store = store()?;
        let _lock = store.lock_wait()?;
        store.stat(&name)?;
        let mut meta = store.metadata()?;
        meta.platforms.insert(name, platforms);
        store.save_metadata(&meta)
    })()
    .is_ok()
}
unsafe extern "C" fn f9(_: *mut c_void, name: *const c_char) -> u64 {
    let Ok(name) = (unsafe { filename(name) }) else {
        return INVALID_STREAM;
    };
    let Ok(store) = store() else {
        return INVALID_STREAM;
    };
    let mut p = pending();
    if p.streams.len() >= 16
        || p.writes.contains(&name)
        || p.streams.values().any(|(n, _, _)| n == &name)
    {
        return INVALID_STREAM;
    }
    let Ok(stream) = store.stream(&name) else {
        return INVALID_STREAM;
    };
    p.next = p.next.wrapping_add(1).max(1);
    let handle = p.next;
    p.streams.insert(handle, (name, store, stream));
    handle
}
unsafe extern "C" fn f10(_: *mut c_void, handle: u64, data: *const c_void, size: i32) -> bool {
    let Some(bytes) = (unsafe { input(data, size) }) else {
        return false;
    };
    let mut p = pending();
    p.streams
        .get_mut(&handle)
        .is_some_and(|(_, _, stream)| stream.append(bytes).is_ok())
}
unsafe extern "C" fn f11(_: *mut c_void, handle: u64) -> bool {
    let mut p = pending();
    let Some((name, store, stream)) = p.streams.get_mut(&handle) else {
        return false;
    };
    let result = (|| -> io::Result<()> {
        let _lock = store.lock_wait()?;
        check_quota(store, name, stream.len(), quota(), max_files())?;
        store.metadata()?;
        stream.commit()?;
        written(store, name)
    })();
    if result.is_err() {
        return false;
    }
    p.streams.remove(&handle);
    true
}

unsafe extern "C" fn f12(_: *mut c_void, handle: u64) -> bool {
    pending().streams.remove(&handle).is_some()
}
unsafe extern "C" fn f13(_: *mut c_void, name: *const c_char) -> bool {
    unsafe { filename(name) }.is_ok_and(|n| store().and_then(|s| s.stat(&n)).is_ok())
}
unsafe extern "C" fn f14(this: *mut c_void, name: *const c_char) -> bool {
    std::env::var("STEAM_TUI_REMOTE_CLOUD").as_deref() != Ok("0")
        && (unsafe { f13(this, name) })
        && unsafe { filename(name) }.is_ok_and(|n| {
            store()
                .and_then(|s| s.metadata())
                .is_ok_and(|m| m.syncs(&n) && !m.dirty.contains(&n))
        })
}
unsafe extern "C" fn f15(_: *mut c_void, name: *const c_char) -> i32 {
    unsafe { filename(name) }
        .ok()
        .and_then(|n| store().ok()?.stat(&n).ok())
        .and_then(|m| i32::try_from(m.len()).ok())
        .unwrap_or(0)
}
unsafe extern "C" fn f16(_: *mut c_void, name: *const c_char) -> i64 {
    unsafe { filename(name) }
        .ok()
        .and_then(|n| {
            store()
                .ok()?
                .stat(&n)
                .ok()?
                .modified()
                .ok()?
                .duration_since(UNIX_EPOCH)
                .ok()
        })
        .map_or(0, |d| d.as_secs().min(i64::MAX as u64) as i64)
}
unsafe extern "C" fn f17(_: *mut c_void, name: *const c_char) -> u32 {
    unsafe { filename(name) }
        .ok()
        .and_then(|n| {
            let s = store().ok()?;
            s.stat(&n).ok()?;
            Some(
                s.metadata()
                    .ok()?
                    .platforms
                    .get(&n)
                    .copied()
                    .unwrap_or(u32::MAX),
            )
        })
        .unwrap_or(0)
}
unsafe extern "C" fn f18(_: *mut c_void) -> i32 {
    store().and_then(|s| s.list()).map_or(0, |l| l.len() as i32)
}
unsafe extern "C" fn f19(_: *mut c_void, index: i32, size: *mut i32) -> *const c_char {
    if !size.is_null() {
        unsafe { size.write(0) };
    }
    if index < 0 {
        return c"".as_ptr();
    }
    let Ok(files) = store().and_then(|s| s.list()) else {
        return c"".as_ptr();
    };
    let Some((name, length)) = files.get(index as usize) else {
        return c"".as_ptr();
    };
    let mut p = pending();
    if p.names.len() != files.len()
        || p.names
            .iter()
            .zip(&files)
            .any(|(a, (b, _))| a.to_bytes() != b.as_bytes())
    {
        p.names = files
            .iter()
            .map(|(n, _)| CString::new(n.as_str()).unwrap())
            .collect();
    }
    let _ = name;
    if !size.is_null() {
        unsafe { size.write((*length).min(i32::MAX as u64) as i32) };
    }
    p.names[index as usize].as_ptr()
}
unsafe extern "C" fn f20(_: *mut c_void, total: *mut u64, available: *mut u64) -> bool {
    if total.is_null() || available.is_null() {
        return false;
    }
    unsafe {
        total.write(0);
        available.write(0);
    }
    let Ok(files) = store().and_then(|s| s.list()) else {
        return false;
    };
    unsafe {
        total.write(quota());
        available.write(quota().saturating_sub(files.iter().map(|(_, s)| *s).sum()));
    }
    true
}
unsafe extern "C" fn f21(_: *mut c_void) -> bool {
    store().is_ok()
}
unsafe extern "C" fn f22(_: *mut c_void) -> bool {
    std::env::var("STEAM_TUI_REMOTE_CLOUD").as_deref() != Ok("0")
        && store().and_then(|s| s.metadata()).is_ok_and(|m| m.enabled)
}
unsafe extern "C" fn f23(_: *mut c_void, enabled: bool) {
    let _ = (|| -> io::Result<()> {
        let s = store()?;
        let _lock = s.lock_wait()?;
        let mut m = s.metadata()?;
        m.enabled = enabled;
        s.save_metadata(&m)
    })();
}
macro_rules! stubs { ($($name:ident => $label:literal, $ret:expr;)*) => { $(unsafe extern "C" fn $name() -> usize { trace(concat!("remotestorage.", $label, " (unavailable)")); $ret })* }; }
stubs! {
 f24=>"UGCDownload",0; f25=>"GetUGCDownloadProgress",0; f26=>"GetUGCDetails",0; f27=>"UGCRead",0;
 f28=>"GetCachedUGCCount",0; f29=>"GetCachedUGCHandle",u64::MAX as usize;
 f30=>"PublishWorkshopFile",0; f31=>"CreatePublishedFileUpdateRequest",0; f32=>"UpdatePublishedFileFile",0;
 f33=>"UpdatePublishedFilePreviewFile",0; f34=>"UpdatePublishedFileTitle",0; f35=>"UpdatePublishedFileDescription",0;
 f36=>"UpdatePublishedFileVisibility",0; f37=>"UpdatePublishedFileTags",0; f38=>"CommitPublishedFileUpdate",0;
 f39=>"GetPublishedFileDetails",0; f40=>"DeletePublishedFile",0; f41=>"EnumerateUserPublishedFiles",0;
 f42=>"SubscribePublishedFile",0; f43=>"EnumerateUserSubscribedFiles",0; f44=>"UnsubscribePublishedFile",0;
 f45=>"UpdatePublishedFileSetChangeDescription",0; f46=>"GetPublishedItemVoteDetails",0; f47=>"UpdateUserPublishedItemVote",0;
 f48=>"GetUserPublishedItemVoteDetails",0; f49=>"EnumerateUserSharedWorkshopFiles",0; f50=>"PublishVideo",0;
 f51=>"SetUserPublishedFileAction",0; f52=>"EnumeratePublishedFilesByUserAction",0; f53=>"EnumeratePublishedWorkshopFiles",0;
 f54=>"UGCDownloadToLocation",0;
}
unsafe extern "C" fn f55(_: *mut c_void) -> i32 {
    0
} // No in-session cloud downloads.
unsafe extern "C" fn f56(
    _: *mut c_void,
    _: i32,
    change: *mut i32,
    path_type: *mut i32,
) -> *const c_char {
    if !change.is_null() {
        unsafe { change.write(0) };
    }
    if !path_type.is_null() {
        unsafe { path_type.write(0) };
    }
    c"".as_ptr()
}
unsafe extern "C" fn f57(_: *mut c_void) -> bool {
    store().is_ok()
}
unsafe extern "C" fn f58(_: *mut c_void) -> bool {
    store().is_ok()
}
static TABLE: VTable<59> = VTable([
    f0 as *const (),
    f1 as *const (),
    f2 as *const (),
    f3 as *const (),
    f4 as *const (),
    f5 as *const (),
    f6 as *const (),
    f7 as *const (),
    f8 as *const (),
    f9 as *const (),
    f10 as *const (),
    f11 as *const (),
    f12 as *const (),
    f13 as *const (),
    f14 as *const (),
    f15 as *const (),
    f16 as *const (),
    f17 as *const (),
    f18 as *const (),
    f19 as *const (),
    f20 as *const (),
    f21 as *const (),
    f22 as *const (),
    f23 as *const (),
    f24 as *const (),
    f25 as *const (),
    f26 as *const (),
    f27 as *const (),
    f28 as *const (),
    f29 as *const (),
    f30 as *const (),
    f31 as *const (),
    f32 as *const (),
    f33 as *const (),
    f34 as *const (),
    f35 as *const (),
    f36 as *const (),
    f37 as *const (),
    f38 as *const (),
    f39 as *const (),
    f40 as *const (),
    f41 as *const (),
    f42 as *const (),
    f43 as *const (),
    f44 as *const (),
    f45 as *const (),
    f46 as *const (),
    f47 as *const (),
    f48 as *const (),
    f49 as *const (),
    f50 as *const (),
    f51 as *const (),
    f52 as *const (),
    f53 as *const (),
    f54 as *const (),
    f55 as *const (),
    f56 as *const (),
    f57 as *const (),
    f58 as *const (),
]);
pub(super) static OBJECT: Object = Object {
    table: TABLE.0.as_ptr(),
};

//! Image handles for `ISteamUtils::GetImageSize/GetImageRGBA`: achievement icons
//! today. The backend downloads and decodes; this table keeps a bounded set of
//! decoded images for the game to read.
use super::*;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use steam_ipc::{ImageOp, Operation, Response};

const MAX_IMAGES: usize = 64;
const MAX_BYTES: usize = 32 << 20;

struct Image {
    width: u32,
    height: u32,
    rgba: Arc<Vec<u8>>,
}

#[derive(Default)]
struct Table {
    next: i32,
    by_url: HashMap<String, i32>,
    images: HashMap<i32, Image>,
    order: VecDeque<i32>,
    bytes: usize,
}

fn table() -> std::sync::MutexGuard<'static, Table> {
    static TABLE: OnceLock<Mutex<Table>> = OnceLock::new();
    TABLE
        .get_or_init(|| Mutex::new(Table::default()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// The handle of an already fetched image.
pub(super) fn cached(url: &str) -> Option<i32> {
    table().by_url.get(url).copied()
}

fn insert(url: &str, width: u32, height: u32, rgba: Vec<u8>) -> i32 {
    let mut table = table();
    if let Some(&handle) = table.by_url.get(url) {
        return handle;
    }
    while table.images.len() >= MAX_IMAGES || table.bytes + rgba.len() > MAX_BYTES {
        let Some(oldest) = table.order.pop_front() else {
            break;
        };
        if let Some(image) = table.images.remove(&oldest) {
            table.bytes -= image.rgba.len();
        }
        table.by_url.retain(|_, handle| *handle != oldest);
    }
    table.next += 1;
    let handle = table.next;
    table.bytes += rgba.len();
    table.order.push_back(handle);
    table.by_url.insert(url.to_owned(), handle);
    table.images.insert(
        handle,
        Image {
            width,
            height,
            rgba: Arc::new(rgba),
        },
    );
    handle
}

/// Downloads `url` through the backend (blocking) and returns its handle.
pub(super) fn fetch(url: &str) -> Option<i32> {
    if let Some(handle) = cached(url) {
        return Some(handle);
    }
    let response = bridge::request(
        current_app_id(),
        Operation::Image(ImageOp::Fetch {
            url: url.to_owned(),
        }),
    );
    match response {
        Ok(Response::Image {
            width,
            height,
            rgba,
        }) => {
            let rgba = steam_ipc::hex_decode(&rgba)?;
            (rgba.len() as u64 == u64::from(width) * u64::from(height) * 4)
                .then(|| insert(url, width, height, rgba))
        }
        _ => None,
    }
}

pub(super) fn size(handle: i32) -> Option<(u32, u32)> {
    table()
        .images
        .get(&handle)
        .map(|image| (image.width, image.height))
}

/// Copies the RGBA pixels into `out` when `capacity` fits them all.
pub(super) unsafe fn copy(handle: i32, out: *mut u8, capacity: i32) -> bool {
    let image = table().images.get(&handle).map(|i| Arc::clone(&i.rgba));
    let Some(rgba) = image else {
        return false;
    };
    if out.is_null() || capacity < 0 || (capacity as usize) < rgba.len() {
        return false;
    }
    // SAFETY: the caller's buffer holds `capacity >= rgba.len()` bytes.
    unsafe { std::ptr::copy_nonoverlapping(rgba.as_ptr(), out, rgba.len()) };
    true
}

/// Fetches `url` on a worker thread, then runs `done(handle)` (0 on failure).
/// A URL already being fetched is not requested twice.
pub(super) fn fetch_async(url: String, done: impl FnOnce(i32) + Send + 'static) {
    static PENDING: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    const MAX_PENDING: usize = 16;
    let pending = PENDING.get_or_init(Default::default);
    {
        let mut set = pending.lock().unwrap_or_else(|e| e.into_inner());
        if set.len() >= MAX_PENDING || !set.insert(url.clone()) {
            return;
        }
    }
    let worker = url.clone();
    let spawned = std::thread::Builder::new()
        .name("steam-image".into())
        .spawn(move || {
            let handle = fetch(&worker).unwrap_or(0);
            pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&worker);
            done(handle);
        });
    if spawned.is_err() {
        pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&url);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_are_stable_bounded_and_evicted_oldest_first() {
        let first = insert("https://x/a", 1, 1, vec![0; 4]);
        assert_eq!(insert("https://x/a", 1, 1, vec![0; 4]), first);
        assert_eq!(cached("https://x/a"), Some(first));
        for n in 0..MAX_IMAGES {
            insert(&format!("https://x/{n}"), 1, 1, vec![0; 4]);
        }
        assert_eq!(size(first), None);
        assert_eq!(cached("https://x/a"), None);
        assert!(table().images.len() <= MAX_IMAGES);
    }

    #[test]
    fn copy_needs_a_big_enough_buffer() {
        let handle = insert("https://x/pix", 2, 1, vec![1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(size(handle), Some((2, 1)));
        let mut small = [0u8; 4];
        let mut big = [0u8; 8];
        unsafe {
            assert!(!copy(handle, small.as_mut_ptr(), 4));
            assert!(copy(handle, big.as_mut_ptr(), 8));
            assert!(!copy(9999, big.as_mut_ptr(), 8));
        }
        assert_eq!(big, [1, 2, 3, 4, 5, 6, 7, 8]);
    }
}

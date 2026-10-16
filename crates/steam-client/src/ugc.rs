//! STEAMUGC_INTERFACE_VERSION017. Queries (by id, all-UGC and per-user lists) are
//! real; the rest of the Workshop is in `workshop.rs`; item creation and
//! updates report failure. Overloads
//! keep header order (Itanium ABI).
use super::*;

macro_rules! stubs {
    ($($name:ident => $label:literal;)*) => {
        $(unsafe extern "C" fn $name() -> usize {
            trace(concat!("ugc.", $label, " (publishing Workshop items is not supported)"));
            0
        })*
    };
}
stubs! {
    f43 => "SetItemTitle";
    f44 => "SetItemDescription";
    f45 => "SetItemUpdateLanguage";
    f46 => "SetItemMetadata";
    f47 => "SetItemVisibility";
    f48 => "SetItemTags";
    f49 => "SetItemContent";
    f50 => "SetItemPreview";
    f51 => "SetAllowLegacyUpload";
    f52 => "RemoveAllItemKeyValueTags";
    f53 => "RemoveItemKeyValueTags";
    f54 => "AddItemKeyValueTag";
    f55 => "AddItemPreviewFile";
    f56 => "AddItemPreviewVideo";
    f57 => "UpdateItemPreviewFile";
    f58 => "UpdateItemPreviewVideo";
    f59 => "RemoveItemPreview";
    f60 => "AddContentDescriptor";
    f61 => "RemoveContentDescriptor";
    f76 => "BInitWorkshopForGameServer";
    f87 => "ShowWorkshopEULA";
}
pub(super) static TABLE: VTable<89> = VTable([
    q_user as *const (),
    q_all_page as *const (),
    q_all_cursor as *const (),
    q_create as *const (),
    q_send as *const (),
    q_result as *const (),
    q_num_tags as *const (),
    q_tag as *const (),
    q_tag_display as *const (),
    q_preview_url as *const (),
    q_metadata as *const (),
    q_children as *const (),
    q_statistic as *const (),
    q_num_previews as *const (),
    q_preview as *const (),
    q_num_kv as *const (),
    q_kv_by_index as *const (),
    q_kv_by_key as *const (),
    q_content_descriptors as *const (),
    q_release as *const (),
    q_required_tag as *const (),
    q_tag_group as *const (),
    q_excluded_tag as *const (),
    q_only_ids as *const (),
    q_return_kv as *const (),
    q_long_description as *const (),
    q_return_metadata as *const (),
    q_return_children as *const (),
    q_return_previews as *const (),
    q_total_only as *const (),
    q_playtime_stats as *const (),
    q_language as *const (),
    q_cached as *const (),
    q_cloud_filename as *const (),
    q_match_any as *const (),
    q_search_text as *const (),
    q_trend_days as *const (),
    q_created_range as *const (),
    q_updated_range as *const (),
    q_required_kv as *const (),
    workshop::request_details as *const (),
    workshop::create_item as *const (),
    workshop::start_item_update as *const (),
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
    f59 as *const (),
    f60 as *const (),
    f61 as *const (),
    workshop::submit_item_update as *const (),
    workshop::item_update_progress as *const (),
    workshop::set_vote as *const (),
    workshop::get_vote as *const (),
    workshop::add_favorite as *const (),
    workshop::remove_favorite as *const (),
    workshop::subscribe as *const (),
    workshop::unsubscribe as *const (),
    workshop::num_subscribed as *const (),
    workshop::subscribed_items as *const (),
    workshop::item_state as *const (),
    workshop::install_info as *const (),
    workshop::download_info as *const (),
    workshop::download_item as *const (),
    f76 as *const (),
    workshop::suspend_downloads as *const (),
    workshop::start_playtime as *const (),
    workshop::stop_playtime as *const (),
    workshop::stop_playtime_all as *const (),
    workshop::add_dependency as *const (),
    workshop::remove_dependency as *const (),
    workshop::add_app_dependency as *const (),
    workshop::remove_app_dependency as *const (),
    workshop::get_app_dependencies as *const (),
    workshop::delete_item as *const (),
    f87 as *const (),
    workshop::eula_status as *const (),
]);
pub(super) static OBJECT: Object = Object {
    table: TABLE.0.as_ptr(),
};

// SDK 1.53a's UGC016 lacks GetQueryUGCContentDescriptors and the two
// Add/RemoveContentDescriptor update methods. All other signatures match 017.
// Source: ValveSoftware/Proton proton_9.0/lsteamclient/steamworks_sdk_153a/isteamugc.h.
const fn table016() -> VTable<86> {
    let mut slots = [std::ptr::null(); 86];
    let mut src = 0;
    let mut dst = 0;
    while src < TABLE.0.len() {
        if src != 18 && src != 60 && src != 61 {
            slots[dst] = TABLE.0[src];
            dst += 1;
        }
        src += 1;
    }
    VTable(slots)
}
static TABLE016: VTable<86> = table016();
pub(super) static OBJECT016: Object = Object {
    table: TABLE016.0.as_ptr(),
};

// ---- queries ---------------------------------------------------------------

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use steam_ipc::{Operation, Response, UgcItem, UgcOp, UgcQuery};

/// k_iSteamUGCCallbacks = 3400.
const UGC_QUERY_COMPLETED: i32 = 3401;
const MAX_QUERIES: usize = 64;
/// SDK 1.60 (UGC 020 and later) appended m_ulTotalFilesSize to SteamUGCDetails_t.
static WIDE_DETAILS: AtomicBool = AtomicBool::new(false);
static NEXT_QUERY: AtomicU64 = AtomicU64::new(1);
/// Longest tag/key/value text copied from the game (bytes).
const MAX_TEXT: usize = 1024;
const MAX_LIST: usize = 64;

pub(super) fn note_wide_details() {
    WIDE_DETAILS.store(true, Ordering::Release);
}

#[derive(Default)]
struct Query {
    /// Published file ids of a details query (empty for searches).
    ids: Vec<u64>,
    search: UgcQuery,
    results: Vec<UgcItem>,
    total: u32,
}

fn queries() -> std::sync::MutexGuard<'static, HashMap<u64, Query>> {
    static QUERIES: OnceLock<Mutex<HashMap<u64, Query>>> = OnceLock::new();
    QUERIES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Text from the game, bounded; `None` for null or invalid UTF-8.
unsafe fn cstr(text: *const c_char) -> Option<String> {
    if text.is_null() {
        return None;
    }
    // SAFETY: the SDK passes NUL-terminated strings.
    let bytes = unsafe { CStr::from_ptr(text) }.to_bytes();
    (bytes.len() <= MAX_TEXT)
        .then(|| std::str::from_utf8(bytes).ok().map(str::to_owned))
        .flatten()
}

fn register(query: Query) -> u64 {
    let mut map = queries();
    if map.len() >= MAX_QUERIES {
        trace("ugc: too many open queries (ReleaseQueryUGCRequest them)");
        return 0;
    }
    let handle = NEXT_QUERY.fetch_add(1, Ordering::Relaxed);
    map.insert(handle, query);
    handle
}

/// CreateQueryUserUGCRequest(accountId, listType, matchingType, sortOrder, creatorApp, consumerApp, page).
unsafe extern "C" fn q_user(
    _: *mut c_void,
    account: u32,
    list: u32,
    matching: i32,
    sort: u32,
    creator: u32,
    consumer: u32,
    page: u32,
) -> u64 {
    trace(format_args!("ugc.CreateQueryUserUGCRequest list {list} page {page}"));
    register(Query {
        search: UgcQuery {
            user_list: Some(list),
            account,
            sort_order: sort,
            matching,
            creator_app: creator,
            consumer_app: consumer,
            page,
            ..Default::default()
        },
        ..Default::default()
    })
}

/// CreateQueryAllUGCRequest(queryType, matchingType, creatorApp, consumerApp, page).
unsafe extern "C" fn q_all_page(
    _: *mut c_void,
    query_type: u32,
    matching: i32,
    creator: u32,
    consumer: u32,
    page: u32,
) -> u64 {
    trace(format_args!("ugc.CreateQueryAllUGCRequest type {query_type} page {page}"));
    register(Query {
        search: UgcQuery {
            query_type,
            matching,
            creator_app: creator,
            consumer_app: consumer,
            page,
            ..Default::default()
        },
        ..Default::default()
    })
}

/// CreateQueryAllUGCRequest(queryType, matchingType, creatorApp, consumerApp, cursor).
unsafe extern "C" fn q_all_cursor(
    _: *mut c_void,
    query_type: u32,
    matching: i32,
    creator: u32,
    consumer: u32,
    cursor: *const c_char,
) -> u64 {
    let cursor = unsafe { cstr(cursor) }.unwrap_or_default();
    trace(format_args!("ugc.CreateQueryAllUGCRequest type {query_type} (cursor)"));
    register(Query {
        search: UgcQuery {
            query_type,
            matching,
            creator_app: creator,
            consumer_app: consumer,
            page: 1,
            cursor,
            ..Default::default()
        },
        ..Default::default()
    })
}

unsafe extern "C" fn q_create(_: *mut c_void, ids: *const u64, count: u32) -> u64 {
    if ids.is_null() || count == 0 || count > 1000 {
        trace(format_args!("ugc.CreateQueryUGCDetailsRequest rejected ({count} ids)"));
        return 0;
    }
    let ids = unsafe { std::slice::from_raw_parts(ids, count as usize) }.to_vec();
    trace(format_args!("ugc.CreateQueryUGCDetailsRequest {} ids", ids.len()));
    register(Query {
        ids,
        ..Default::default()
    })
}

/// Applies `change` to the query; false for an unknown handle.
fn edit(handle: u64, change: impl FnOnce(&mut Query)) -> bool {
    match queries().get_mut(&handle) {
        Some(query) => {
            change(query);
            true
        }
        None => false,
    }
}

unsafe extern "C" fn q_required_tag(_: *mut c_void, handle: u64, tag: *const c_char) -> bool {
    let Some(tag) = (unsafe { cstr(tag) }) else {
        return false;
    };
    edit(handle, |q| {
        if q.search.required_tags.len() < MAX_LIST {
            q.search.required_tags.push(tag);
        }
    })
}
/// AddRequiredTagGroup(handle, const SteamParamStringArray_t*): match any tag of the group.
unsafe extern "C" fn q_tag_group(_: *mut c_void, handle: u64, group: *const [usize; 2]) -> bool {
    if group.is_null() {
        return false;
    }
    // SteamParamStringArray_t { const char **m_ppStrings; int32 m_nNumStrings; }
    let (strings, count) = unsafe {
        let raw = (*group)[0] as *const *const c_char;
        let count = ((*group)[1] & 0xffff_ffff) as u32 as i32;
        (raw, count)
    };
    if strings.is_null() || !(0..=MAX_LIST as i32).contains(&count) {
        return false;
    }
    let tags: Vec<String> = (0..count as usize)
        .filter_map(|i| unsafe { cstr(*strings.add(i)) })
        .collect();
    edit(handle, |q| {
        if q.search.tag_groups.len() < MAX_LIST {
            q.search.tag_groups.push(tags);
        }
    })
}
unsafe extern "C" fn q_excluded_tag(_: *mut c_void, handle: u64, tag: *const c_char) -> bool {
    let Some(tag) = (unsafe { cstr(tag) }) else {
        return false;
    };
    edit(handle, |q| {
        if q.search.excluded_tags.len() < MAX_LIST {
            q.search.excluded_tags.push(tag);
        }
    })
}
unsafe extern "C" fn q_only_ids(_: *mut c_void, handle: u64, on: bool) -> bool {
    edit(handle, |q| q.search.ids_only = on)
}
unsafe extern "C" fn q_return_kv(_: *mut c_void, handle: u64, on: bool) -> bool {
    edit(handle, |q| q.search.key_value_tags = on)
}
unsafe extern "C" fn q_long_description(_: *mut c_void, handle: u64, on: bool) -> bool {
    edit(handle, |q| q.search.long_description = on)
}
unsafe extern "C" fn q_return_metadata(_: *mut c_void, handle: u64, on: bool) -> bool {
    edit(handle, |q| q.search.metadata = on)
}
unsafe extern "C" fn q_return_children(_: *mut c_void, handle: u64, on: bool) -> bool {
    edit(handle, |q| q.search.children = on)
}
unsafe extern "C" fn q_return_previews(_: *mut c_void, handle: u64, on: bool) -> bool {
    edit(handle, |q| q.search.previews = on)
}
unsafe extern "C" fn q_total_only(_: *mut c_void, handle: u64, on: bool) -> bool {
    edit(handle, |q| q.search.total_only = on)
}
unsafe extern "C" fn q_playtime_stats(_: *mut c_void, handle: u64, days: u32) -> bool {
    edit(handle, |q| q.search.playtime_days = days)
}
unsafe extern "C" fn q_language(_: *mut c_void, handle: u64, language: *const c_char) -> bool {
    let Some(language) = (unsafe { cstr(language) }) else {
        return false;
    };
    edit(handle, |q| q.search.language = language)
}
unsafe extern "C" fn q_cached(_: *mut c_void, handle: u64, seconds: u32) -> bool {
    edit(handle, |q| q.search.cache_age = seconds)
}
unsafe extern "C" fn q_cloud_filename(_: *mut c_void, handle: u64, name: *const c_char) -> bool {
    let Some(name) = (unsafe { cstr(name) }) else {
        return false;
    };
    edit(handle, |q| q.search.cloud_filename = name)
}
unsafe extern "C" fn q_match_any(_: *mut c_void, handle: u64, on: bool) -> bool {
    edit(handle, |q| q.search.match_any_tag = on)
}
unsafe extern "C" fn q_search_text(_: *mut c_void, handle: u64, text: *const c_char) -> bool {
    let Some(text) = (unsafe { cstr(text) }) else {
        return false;
    };
    edit(handle, |q| q.search.search = text)
}
unsafe extern "C" fn q_trend_days(_: *mut c_void, handle: u64, days: u32) -> bool {
    edit(handle, |q| q.search.trend_days = days)
}
unsafe extern "C" fn q_created_range(_: *mut c_void, handle: u64, start: u32, end: u32) -> bool {
    edit(handle, |q| q.search.created = (start, end))
}
unsafe extern "C" fn q_updated_range(_: *mut c_void, handle: u64, start: u32, end: u32) -> bool {
    edit(handle, |q| q.search.updated = (start, end))
}
unsafe extern "C" fn q_required_kv(
    _: *mut c_void,
    handle: u64,
    key: *const c_char,
    value: *const c_char,
) -> bool {
    let (Some(key), Some(value)) = (unsafe { cstr(key) }, unsafe { cstr(value) }) else {
        return false;
    };
    edit(handle, |q| {
        if q.search.kv_tags.len() < MAX_LIST {
            q.search.kv_tags.push((key, value));
        }
    })
}

fn completed(handle: u64, result: i32, found: u32, total: u32, cursor: &str) -> Vec<u8> {
    // SteamUGCQueryCompleted_t (pack 4): handle, result, returned, total, cached, cursor[256].
    let mut payload = handle.to_le_bytes().to_vec();
    payload.extend(result.to_le_bytes());
    payload.extend(found.to_le_bytes());
    payload.extend(total.to_le_bytes());
    payload.resize(21, 0);
    let bytes = cursor.as_bytes();
    payload.extend(&bytes[..bytes.len().min(255)]);
    payload.resize(280, 0);
    payload
}

unsafe extern "C" fn q_send(_: *mut c_void, handle: u64) -> u64 {
    let request = queries()
        .get(&handle)
        .map(|q| (q.ids.clone(), q.search.clone()));
    let failure = completed(handle, 2, 0, 0, "");
    let Some((ids, search)) = request else {
        trace(format_args!("ugc.SendQueryUGCRequest unknown handle {handle}"));
        return callresults::complete(UGC_QUERY_COMPLETED, failure);
    };
    if std::env::var_os("STEAM_TUI_BACKEND_SOCKET").is_none() {
        return callresults::complete(UGC_QUERY_COMPLETED, failure);
    }
    trace("ugc.SendQueryUGCRequest");
    userstats::spawn_call(UGC_QUERY_COMPLETED, failure.clone(), move || {
        let operation = if ids.is_empty() {
            UgcOp::Query(Box::new(search.clone()))
        } else {
            UgcOp::Details {
                ids,
                long_description: search.long_description,
            }
        };
        match bridge::request(current_app_id(), Operation::Ugc(operation)) {
            Ok(Response::UgcDetails { result: 1, items }) => {
                let found = items.len() as u32;
                if let Some(query) = queries().get_mut(&handle) {
                    query.results = items;
                    query.total = found;
                }
                completed(handle, 1, found, found, "")
            }
            Ok(Response::UgcQuery {
                result,
                total,
                cursor,
                items,
            }) => {
                let found = items.len() as u32;
                if let Some(query) = queries().get_mut(&handle) {
                    query.results = items;
                    query.total = total;
                }
                completed(handle, result, found, total, &cursor)
            }
            _ => failure,
        }
    })
}

fn put(out: *mut u8, offset: usize, bytes: &[u8]) {
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), out.add(offset), bytes.len()) };
}
fn put_text(out: *mut u8, offset: usize, capacity: usize, text: &str) {
    let bytes = text.as_bytes();
    let n = bytes.len().min(capacity - 1);
    put(out, offset, &bytes[..n]);
}
/// Copies `text` NUL-terminated into the game's buffer; false when it has no room.
unsafe fn copy_out(out: *mut c_char, capacity: u32, text: &str) -> bool {
    if out.is_null() || capacity == 0 {
        return false;
    }
    let n = text.len().min(capacity as usize - 1);
    unsafe {
        std::ptr::copy_nonoverlapping(text.as_ptr(), out.cast::<u8>(), n);
        out.add(n).write(0);
    }
    true
}

/// Runs `read` on result `index` of query `handle`.
fn with_item<R>(handle: u64, index: u32, read: impl FnOnce(&UgcItem) -> R) -> Option<R> {
    let map = queries();
    map.get(&handle)?.results.get(index as usize).map(read)
}

/// Writes `SteamUGCDetails_t` (pack 4) to `out`: 9764 bytes, 9772 with m_ulTotalFilesSize
/// (SDK 1.60+). Returns the size written.
pub(super) unsafe fn write_details(out: *mut u8, item: &UgcItem) -> usize {
    let size = if WIDE_DETAILS.load(Ordering::Acquire) {
        9772
    } else {
        9764
    };
    unsafe { std::ptr::write_bytes(out, 0, size) };
    put(out, 0, &item.id.to_le_bytes());
    put(out, 8, &item.result.to_le_bytes());
    put(out, 12, &item.file_type.to_le_bytes());
    put(out, 16, &item.creator_app.to_le_bytes());
    put(out, 20, &item.consumer_app.to_le_bytes());
    put_text(out, 24, 129, &item.title);
    put_text(out, 153, 8000, &item.description);
    put(out, 8156, &item.owner.to_le_bytes());
    put(out, 8164, &item.created.to_le_bytes());
    put(out, 8168, &item.updated.to_le_bytes());
    put(out, 8176, &item.visibility.to_le_bytes());
    put(out, 8180, &[u8::from(item.banned), 1, 0]);
    put_text(out, 8183, 1025, &item.tags);
    put(out, 9208, &item.file_handle.to_le_bytes());
    put(out, 9216, &item.preview_handle.to_le_bytes());
    put_text(out, 9224, 260, &item.file_name);
    put(out, 9484, &item.file_size.to_le_bytes());
    put(out, 9488, &item.preview_size.to_le_bytes());
    put_text(out, 9492, 256, &item.url);
    put(out, 9748, &item.votes_up.to_le_bytes());
    put(out, 9752, &item.votes_down.to_le_bytes());
    put(out, 9756, &item.score.to_le_bytes());
    put(out, 9760, &item.children.to_le_bytes());
    if size == 9772 {
        put(out, 9764, &(item.file_size.max(0) as u64).to_le_bytes());
    }
    size
}

/// SteamUGCRequestUGCDetailsResult_t: the details followed by `m_bCachedData`.
pub(super) fn details_result(item: &UgcItem, cached: bool) -> Vec<u8> {
    let mut buffer = vec![0u8; 9772 + 8];
    let size = unsafe { write_details(buffer.as_mut_ptr(), item) };
    buffer[size] = u8::from(cached);
    buffer.truncate(size + 4);
    buffer
}

unsafe extern "C" fn q_result(_: *mut c_void, handle: u64, index: u32, out: *mut u8) -> bool {
    if out.is_null() {
        return false;
    }
    let map = queries();
    let Some(item) = map.get(&handle).and_then(|q| q.results.get(index as usize)) else {
        return false;
    };
    unsafe { write_details(out, item) };
    true
}

unsafe extern "C" fn q_num_tags(_: *mut c_void, handle: u64, index: u32) -> u32 {
    with_item(handle, index, |i| i.tag_list.len() as u32).unwrap_or(0)
}
unsafe extern "C" fn q_tag(
    _: *mut c_void,
    handle: u64,
    index: u32,
    tag: u32,
    out: *mut c_char,
    capacity: u32,
) -> bool {
    with_item(handle, index, |i| i.tag_list.get(tag as usize).map(|t| t.0.clone()))
        .flatten()
        .is_some_and(|text| unsafe { copy_out(out, capacity, &text) })
}
unsafe extern "C" fn q_tag_display(
    _: *mut c_void,
    handle: u64,
    index: u32,
    tag: u32,
    out: *mut c_char,
    capacity: u32,
) -> bool {
    with_item(handle, index, |i| {
        i.tag_list.get(tag as usize).map(|t| {
            if t.1.is_empty() { t.0.clone() } else { t.1.clone() }
        })
    })
    .flatten()
    .is_some_and(|text| unsafe { copy_out(out, capacity, &text) })
}
unsafe extern "C" fn q_preview_url(
    _: *mut c_void,
    handle: u64,
    index: u32,
    out: *mut c_char,
    capacity: u32,
) -> bool {
    with_item(handle, index, |i| i.preview_url.clone())
        .filter(|url| !url.is_empty())
        .is_some_and(|url| unsafe { copy_out(out, capacity, &url) })
}

unsafe extern "C" fn q_metadata(
    _: *mut c_void,
    handle: u64,
    index: u32,
    out: *mut u8,
    capacity: u32,
) -> bool {
    if out.is_null() || capacity == 0 {
        return false;
    }
    let map = queries();
    let Some(item) = map.get(&handle).and_then(|q| q.results.get(index as usize)) else {
        return false;
    };
    if item.metadata.is_empty() {
        return false;
    }
    let n = item.metadata.len().min(capacity as usize - 1);
    unsafe {
        std::ptr::copy_nonoverlapping(item.metadata.as_ptr(), out, n);
        out.add(n).write(0);
    }
    true
}

unsafe extern "C" fn q_children(
    _: *mut c_void,
    handle: u64,
    index: u32,
    out: *mut u64,
    max: u32,
) -> bool {
    let Some(children) = with_item(handle, index, |i| i.child_ids.clone()) else {
        return false;
    };
    if out.is_null() {
        return false;
    }
    for (i, id) in children.iter().take(max as usize).enumerate() {
        unsafe { out.add(i).write(*id) };
    }
    true
}

unsafe extern "C" fn q_statistic(
    _: *mut c_void,
    handle: u64,
    index: u32,
    kind: u32,
    out: *mut u64,
) -> bool {
    let value = with_item(handle, index, |i| {
        i.stats.iter().find(|(k, _)| *k == kind).map(|(_, v)| *v)
    })
    .flatten();
    match (value, out.is_null()) {
        (Some(value), false) => {
            unsafe { out.write(value) };
            true
        }
        _ => false,
    }
}

unsafe extern "C" fn q_num_previews(_: *mut c_void, handle: u64, index: u32) -> u32 {
    with_item(handle, index, |i| i.previews.len() as u32).unwrap_or(0)
}
/// GetQueryUGCAdditionalPreview(handle, index, previewIndex, url, urlSize, originalName, nameSize, EItemPreviewType*).
unsafe extern "C" fn q_preview(
    _: *mut c_void,
    handle: u64,
    index: u32,
    preview: u32,
    url: *mut c_char,
    url_size: u32,
    original: *mut c_char,
    original_size: u32,
    kind: *mut u32,
) -> bool {
    let Some(Some(found)) = with_item(handle, index, |i| i.previews.get(preview as usize).cloned())
    else {
        return false;
    };
    unsafe {
        copy_out(url, url_size, &found.url);
        copy_out(original, original_size, &found.original);
        if !kind.is_null() {
            kind.write(found.kind);
        }
    }
    true
}

unsafe extern "C" fn q_num_kv(_: *mut c_void, handle: u64, index: u32) -> u32 {
    with_item(handle, index, |i| i.kv_tags.len() as u32).unwrap_or(0)
}
/// GetQueryUGCKeyValueTag(handle, index, kvIndex, key, keySize, value, valueSize).
unsafe extern "C" fn q_kv_by_index(
    _: *mut c_void,
    handle: u64,
    index: u32,
    kv: u32,
    key: *mut c_char,
    key_size: u32,
    value: *mut c_char,
    value_size: u32,
) -> bool {
    let Some(Some((k, v))) = with_item(handle, index, |i| i.kv_tags.get(kv as usize).cloned())
    else {
        return false;
    };
    unsafe {
        copy_out(key, key_size, &k);
        copy_out(value, value_size, &v);
    }
    true
}
/// GetQueryUGCKeyValueTag(handle, index, key, value, valueSize): the first value of `key`.
unsafe extern "C" fn q_kv_by_key(
    _: *mut c_void,
    handle: u64,
    index: u32,
    key: *const c_char,
    value: *mut c_char,
    value_size: u32,
) -> bool {
    let Some(key) = (unsafe { cstr(key) }) else {
        return false;
    };
    let found = with_item(handle, index, |i| {
        i.kv_tags.iter().find(|(k, _)| *k == key).map(|(_, v)| v.clone())
    })
    .flatten();
    found.is_some_and(|v| unsafe { copy_out(value, value_size, &v) })
}
unsafe extern "C" fn q_content_descriptors(
    _: *mut c_void,
    _handle: u64,
    _index: u32,
    _out: *mut u32,
    _max: u32,
) -> u32 {
    0
}

unsafe extern "C" fn q_release(_: *mut c_void, handle: u64) -> bool {
    queries().remove(&handle).is_some()
}

// ---- generated: additional interface versions ----

// STEAMUGC_INTERFACE_VERSION018 (SDK 159 header layout): slots matched to the base table by method name; the rest are unavailable.
new_slots! {
    n_018_89 => "ugc.GetUserContentDescriptorPreferences";
}
static TABLE_018: VTable<90> = VTable([
    TABLE.0[0],
    TABLE.0[1],
    TABLE.0[2],
    TABLE.0[3],
    TABLE.0[4],
    TABLE.0[5],
    TABLE.0[6],
    TABLE.0[7],
    TABLE.0[8],
    TABLE.0[9],
    TABLE.0[10],
    TABLE.0[11],
    TABLE.0[12],
    TABLE.0[13],
    TABLE.0[14],
    TABLE.0[15],
    TABLE.0[16],
    TABLE.0[17],
    TABLE.0[18],
    TABLE.0[19],
    TABLE.0[20],
    TABLE.0[21],
    TABLE.0[22],
    TABLE.0[23],
    TABLE.0[24],
    TABLE.0[25],
    TABLE.0[26],
    TABLE.0[27],
    TABLE.0[28],
    TABLE.0[29],
    TABLE.0[30],
    TABLE.0[31],
    TABLE.0[32],
    TABLE.0[33],
    TABLE.0[34],
    TABLE.0[35],
    TABLE.0[36],
    TABLE.0[37],
    TABLE.0[38],
    TABLE.0[39],
    TABLE.0[40],
    TABLE.0[41],
    TABLE.0[42],
    TABLE.0[43],
    TABLE.0[44],
    TABLE.0[45],
    TABLE.0[46],
    TABLE.0[47],
    TABLE.0[48],
    TABLE.0[49],
    TABLE.0[50],
    TABLE.0[51],
    TABLE.0[52],
    TABLE.0[53],
    TABLE.0[54],
    TABLE.0[55],
    TABLE.0[56],
    TABLE.0[57],
    TABLE.0[58],
    TABLE.0[59],
    TABLE.0[60],
    TABLE.0[61],
    TABLE.0[62],
    TABLE.0[63],
    TABLE.0[64],
    TABLE.0[65],
    TABLE.0[66],
    TABLE.0[67],
    TABLE.0[68],
    TABLE.0[69],
    TABLE.0[70],
    TABLE.0[71],
    TABLE.0[72],
    TABLE.0[73],
    TABLE.0[74],
    TABLE.0[75],
    TABLE.0[76],
    TABLE.0[77],
    TABLE.0[78],
    TABLE.0[79],
    TABLE.0[80],
    TABLE.0[81],
    TABLE.0[82],
    TABLE.0[83],
    TABLE.0[84],
    TABLE.0[85],
    TABLE.0[86],
    TABLE.0[87],
    TABLE.0[88],
    n_018_89 as *const (),
]);
pub(super) static OBJECT_018: Object = Object {
    table: TABLE_018.0.as_ptr(),
};

// STEAMUGC_INTERFACE_VERSION020 (SDK 161 header layout): slots matched to the base table by method name; the rest are unavailable.
new_slots! {
    n_020_18 => "ugc.GetNumSupportedGameVersions";
    n_020_19 => "ugc.GetSupportedGameVersionData";
    n_020_35 => "ugc.SetAdminQuery";
    n_020_65 => "ugc.SetRequiredGameVersions";
    n_020_93 => "ugc.GetUserContentDescriptorPreferences";
}
static TABLE_020: VTable<94> = VTable([
    TABLE.0[0],
    TABLE.0[1],
    TABLE.0[2],
    TABLE.0[3],
    TABLE.0[4],
    TABLE.0[5],
    TABLE.0[6],
    TABLE.0[7],
    TABLE.0[8],
    TABLE.0[9],
    TABLE.0[10],
    TABLE.0[11],
    TABLE.0[12],
    TABLE.0[13],
    TABLE.0[14],
    TABLE.0[15],
    TABLE.0[16],
    TABLE.0[17],
    n_020_18 as *const (),
    n_020_19 as *const (),
    TABLE.0[18],
    TABLE.0[19],
    TABLE.0[20],
    TABLE.0[21],
    TABLE.0[22],
    TABLE.0[23],
    TABLE.0[24],
    TABLE.0[25],
    TABLE.0[26],
    TABLE.0[27],
    TABLE.0[28],
    TABLE.0[29],
    TABLE.0[30],
    TABLE.0[31],
    TABLE.0[32],
    n_020_35 as *const (),
    TABLE.0[33],
    TABLE.0[34],
    TABLE.0[35],
    TABLE.0[36],
    TABLE.0[37],
    TABLE.0[38],
    TABLE.0[39],
    TABLE.0[40],
    TABLE.0[41],
    TABLE.0[42],
    TABLE.0[43],
    TABLE.0[44],
    TABLE.0[45],
    TABLE.0[46],
    TABLE.0[47],
    TABLE.0[48],
    TABLE.0[49],
    TABLE.0[50],
    TABLE.0[51],
    TABLE.0[52],
    TABLE.0[53],
    TABLE.0[54],
    TABLE.0[55],
    TABLE.0[56],
    TABLE.0[57],
    TABLE.0[58],
    TABLE.0[59],
    TABLE.0[60],
    TABLE.0[61],
    n_020_65 as *const (),
    TABLE.0[62],
    TABLE.0[63],
    TABLE.0[64],
    TABLE.0[65],
    TABLE.0[66],
    TABLE.0[67],
    TABLE.0[68],
    TABLE.0[69],
    TABLE.0[70],
    TABLE.0[71],
    TABLE.0[72],
    TABLE.0[73],
    TABLE.0[74],
    TABLE.0[75],
    TABLE.0[76],
    TABLE.0[77],
    TABLE.0[78],
    TABLE.0[79],
    TABLE.0[80],
    TABLE.0[81],
    TABLE.0[82],
    TABLE.0[83],
    TABLE.0[84],
    TABLE.0[85],
    TABLE.0[86],
    TABLE.0[87],
    TABLE.0[88],
    n_020_93 as *const (),
]);
pub(super) static OBJECT_020: Object = Object {
    table: TABLE_020.0.as_ptr(),
};

// STEAMUGC_INTERFACE_VERSION021 (SDK 165 header layout): slots matched to the base table by method name; the rest are unavailable.
new_slots! {
    n_021_18 => "ugc.GetNumSupportedGameVersions";
    n_021_19 => "ugc.GetSupportedGameVersionData";
    n_021_35 => "ugc.SetAdminQuery";
    n_021_65 => "ugc.SetRequiredGameVersions";
    n_021_93 => "ugc.GetUserContentDescriptorPreferences";
    n_021_94 => "ugc.SetItemsDisabledLocally";
    n_021_95 => "ugc.SetSubscriptionsLoadOrder";
    n_021_96 => "ugc.MarkDownloadedItemAsUnused";
    n_021_97 => "ugc.GetNumDownloadedItems";
    n_021_98 => "ugc.GetDownloadedItems";
}
static TABLE_021: VTable<99> = VTable([
    TABLE.0[0],
    TABLE.0[1],
    TABLE.0[2],
    TABLE.0[3],
    TABLE.0[4],
    TABLE.0[5],
    TABLE.0[6],
    TABLE.0[7],
    TABLE.0[8],
    TABLE.0[9],
    TABLE.0[10],
    TABLE.0[11],
    TABLE.0[12],
    TABLE.0[13],
    TABLE.0[14],
    TABLE.0[15],
    TABLE.0[16],
    TABLE.0[17],
    n_021_18 as *const (),
    n_021_19 as *const (),
    TABLE.0[18],
    TABLE.0[19],
    TABLE.0[20],
    TABLE.0[21],
    TABLE.0[22],
    TABLE.0[23],
    TABLE.0[24],
    TABLE.0[25],
    TABLE.0[26],
    TABLE.0[27],
    TABLE.0[28],
    TABLE.0[29],
    TABLE.0[30],
    TABLE.0[31],
    TABLE.0[32],
    n_021_35 as *const (),
    TABLE.0[33],
    TABLE.0[34],
    TABLE.0[35],
    TABLE.0[36],
    TABLE.0[37],
    TABLE.0[38],
    TABLE.0[39],
    TABLE.0[40],
    TABLE.0[41],
    TABLE.0[42],
    TABLE.0[43],
    TABLE.0[44],
    TABLE.0[45],
    TABLE.0[46],
    TABLE.0[47],
    TABLE.0[48],
    TABLE.0[49],
    TABLE.0[50],
    TABLE.0[51],
    TABLE.0[52],
    TABLE.0[53],
    TABLE.0[54],
    TABLE.0[55],
    TABLE.0[56],
    TABLE.0[57],
    TABLE.0[58],
    TABLE.0[59],
    TABLE.0[60],
    TABLE.0[61],
    n_021_65 as *const (),
    TABLE.0[62],
    TABLE.0[63],
    TABLE.0[64],
    TABLE.0[65],
    TABLE.0[66],
    TABLE.0[67],
    TABLE.0[68],
    TABLE.0[69],
    TABLE.0[70],
    TABLE.0[71],
    TABLE.0[72],
    TABLE.0[73],
    TABLE.0[74],
    TABLE.0[75],
    TABLE.0[76],
    TABLE.0[77],
    TABLE.0[78],
    TABLE.0[79],
    TABLE.0[80],
    TABLE.0[81],
    TABLE.0[82],
    TABLE.0[83],
    TABLE.0[84],
    TABLE.0[85],
    TABLE.0[86],
    TABLE.0[87],
    TABLE.0[88],
    n_021_93 as *const (),
    n_021_94 as *const (),
    n_021_95 as *const (),
    n_021_96 as *const (),
    n_021_97 as *const (),
    n_021_98 as *const (),
]);
pub(super) static OBJECT_021: Object = Object {
    table: TABLE_021.0.as_ptr(),
};

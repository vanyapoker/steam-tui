//! STEAMUGC_INTERFACE_VERSION017. Queries by published file id (details and
//! metadata) are real; every other Workshop call reports failure. Overloads
//! keep header order (Itanium ABI).
use super::*;

macro_rules! stubs {
    ($($name:ident => $label:literal;)*) => {
        $(unsafe extern "C" fn $name() -> usize {
            trace(concat!("ugc.", $label, " (no workshop support)"));
            0
        })*
    };
}
stubs! {
    f0 => "CreateQueryUserUGCRequest";
    f1 => "CreateQueryAllUGCRequest";
    f2 => "CreateQueryAllUGCRequest";
    f6 => "GetQueryUGCNumTags";
    f7 => "GetQueryUGCTag";
    f8 => "GetQueryUGCTagDisplayName";
    f9 => "GetQueryUGCPreviewURL";
    f11 => "GetQueryUGCChildren";
    f12 => "GetQueryUGCStatistic";
    f13 => "GetQueryUGCNumAdditionalPreviews";
    f14 => "GetQueryUGCAdditionalPreview";
    f15 => "GetQueryUGCNumKeyValueTags";
    f16 => "GetQueryUGCKeyValueTag";
    f17 => "GetQueryUGCKeyValueTag";
    f18 => "GetQueryUGCContentDescriptors";
    f40 => "RequestUGCDetails";
    f41 => "CreateItem";
    f42 => "StartItemUpdate";
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
    f62 => "SubmitItemUpdate";
    f63 => "GetItemUpdateProgress";
    f64 => "SetUserItemVote";
    f65 => "GetUserItemVote";
    f66 => "AddItemToFavorites";
    f67 => "RemoveItemFromFavorites";
    f68 => "SubscribeItem";
    f69 => "UnsubscribeItem";
    f70 => "GetNumSubscribedItems";
    f71 => "GetSubscribedItems";
    f72 => "GetItemState";
    f73 => "GetItemInstallInfo";
    f74 => "GetItemDownloadInfo";
    f75 => "DownloadItem";
    f76 => "BInitWorkshopForGameServer";
    f77 => "SuspendDownloads";
    f78 => "StartPlaytimeTracking";
    f79 => "StopPlaytimeTracking";
    f80 => "StopPlaytimeTrackingForAllItems";
    f81 => "AddDependency";
    f82 => "RemoveDependency";
    f83 => "AddAppDependency";
    f84 => "RemoveAppDependency";
    f85 => "GetAppDependencies";
    f86 => "DeleteItem";
    f87 => "ShowWorkshopEULA";
    f88 => "GetWorkshopEULAStatus";
}
pub(super) static TABLE: VTable<89> = VTable([
    f0 as *const (),
    f1 as *const (),
    f2 as *const (),
    q_create as *const (),
    q_send as *const (),
    q_result as *const (),
    f6 as *const (),
    f7 as *const (),
    f8 as *const (),
    f9 as *const (),
    q_metadata as *const (),
    f11 as *const (),
    f12 as *const (),
    f13 as *const (),
    f14 as *const (),
    f15 as *const (),
    f16 as *const (),
    f17 as *const (),
    f18 as *const (),
    q_release as *const (),
    q_setter as *const (),
    q_setter as *const (),
    q_setter as *const (),
    q_setter as *const (),
    q_setter as *const (),
    q_flag_long_description as *const (),
    q_setter as *const (),
    q_setter as *const (),
    q_setter as *const (),
    q_setter as *const (),
    q_setter as *const (),
    q_setter as *const (),
    q_setter as *const (),
    q_setter as *const (),
    q_setter as *const (),
    q_setter as *const (),
    q_setter as *const (),
    q_setter as *const (),
    q_setter as *const (),
    q_setter as *const (),
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
    f59 as *const (),
    f60 as *const (),
    f61 as *const (),
    f62 as *const (),
    f63 as *const (),
    f64 as *const (),
    f65 as *const (),
    f66 as *const (),
    f67 as *const (),
    f68 as *const (),
    f69 as *const (),
    f70 as *const (),
    f71 as *const (),
    f72 as *const (),
    f73 as *const (),
    f74 as *const (),
    f75 as *const (),
    f76 as *const (),
    f77 as *const (),
    f78 as *const (),
    f79 as *const (),
    f80 as *const (),
    f81 as *const (),
    f82 as *const (),
    f83 as *const (),
    f84 as *const (),
    f85 as *const (),
    f86 as *const (),
    f87 as *const (),
    f88 as *const (),
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

// ---- queries by published file id -------------------------------------------

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use steam_ipc::{Operation, Response, UgcItem, UgcOp};

/// k_iSteamUGCCallbacks = 3400.
const UGC_QUERY_COMPLETED: i32 = 3401;
const MAX_QUERIES: usize = 64;
/// SDK 1.60 (UGC 020 and later) appended m_ulTotalFilesSize to SteamUGCDetails_t.
static WIDE_DETAILS: AtomicBool = AtomicBool::new(false);
static NEXT_QUERY: AtomicU64 = AtomicU64::new(1);

pub(super) fn note_wide_details() {
    WIDE_DETAILS.store(true, Ordering::Release);
}

#[derive(Default)]
struct Query {
    ids: Vec<u64>,
    long_description: bool,
    results: Vec<UgcItem>,
}

fn queries() -> std::sync::MutexGuard<'static, HashMap<u64, Query>> {
    static QUERIES: OnceLock<Mutex<HashMap<u64, Query>>> = OnceLock::new();
    QUERIES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

unsafe extern "C" fn q_create(_: *mut c_void, ids: *const u64, count: u32) -> u64 {
    if ids.is_null() || count == 0 || count > 256 {
        return 0;
    }
    let mut map = queries();
    if map.len() >= MAX_QUERIES {
        return 0;
    }
    let handle = NEXT_QUERY.fetch_add(1, Ordering::Relaxed);
    let ids = unsafe { std::slice::from_raw_parts(ids, count as usize) }.to_vec();
    trace(format_args!(
        "ugc.CreateQueryUGCDetailsRequest {} ids",
        ids.len()
    ));
    map.insert(
        handle,
        Query {
            ids,
            ..Default::default()
        },
    );
    handle
}

unsafe extern "C" fn q_setter() -> bool {
    true
}

unsafe extern "C" fn q_flag_long_description(_: *mut c_void, handle: u64, on: bool) -> bool {
    match queries().get_mut(&handle) {
        Some(query) => {
            query.long_description = on;
            true
        }
        None => false,
    }
}

fn completed(handle: u64, result: i32, found: u32) -> Vec<u8> {
    // SteamUGCQueryCompleted_t (pack 4): handle, result, returned, total, cached, cursor[256].
    let mut payload = handle.to_le_bytes().to_vec();
    payload.extend(result.to_le_bytes());
    payload.extend(found.to_le_bytes());
    payload.extend(found.to_le_bytes());
    payload.resize(280, 0);
    payload
}

unsafe extern "C" fn q_send(_: *mut c_void, handle: u64) -> u64 {
    let request = queries()
        .get(&handle)
        .map(|q| (q.ids.clone(), q.long_description));
    let failure = completed(handle, 2, 0);
    let Some((ids, long_description)) = request else {
        let call = callresults::reserve();
        callresults::complete_for(call, UGC_QUERY_COMPLETED, failure);
        return call;
    };
    if std::env::var_os("STEAM_TUI_BACKEND_SOCKET").is_none() {
        let call = callresults::reserve();
        callresults::complete_for(call, UGC_QUERY_COMPLETED, failure);
        return call;
    }
    trace("ugc.SendQueryUGCRequest");
    userstats::spawn_call(UGC_QUERY_COMPLETED, failure.clone(), move || {
        let response = bridge::request(
            current_app_id(),
            Operation::Ugc(UgcOp::Details {
                ids,
                long_description,
            }),
        );
        match response {
            Ok(Response::UgcDetails { result: 1, items }) => {
                let found = items.len() as u32;
                if let Some(query) = queries().get_mut(&handle) {
                    query.results = items;
                }
                completed(handle, 1, found)
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

unsafe extern "C" fn q_result(_: *mut c_void, handle: u64, index: u32, out: *mut u8) -> bool {
    if out.is_null() {
        return false;
    }
    let map = queries();
    let Some(item) = map.get(&handle).and_then(|q| q.results.get(index as usize)) else {
        return false;
    };
    // SteamUGCDetails_t (pack 4): 9764 bytes, 9772 with m_ulTotalFilesSize (SDK 1.60+).
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
    true
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

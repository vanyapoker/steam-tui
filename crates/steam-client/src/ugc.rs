//! STEAMUGC_INTERFACE_VERSION017 without Workshop support: queries and updates
//! return invalid handles (0) and every other call reports failure. Overloads
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
    f3 => "CreateQueryUGCDetailsRequest";
    f4 => "SendQueryUGCRequest";
    f5 => "GetQueryUGCResult";
    f6 => "GetQueryUGCNumTags";
    f7 => "GetQueryUGCTag";
    f8 => "GetQueryUGCTagDisplayName";
    f9 => "GetQueryUGCPreviewURL";
    f10 => "GetQueryUGCMetadata";
    f11 => "GetQueryUGCChildren";
    f12 => "GetQueryUGCStatistic";
    f13 => "GetQueryUGCNumAdditionalPreviews";
    f14 => "GetQueryUGCAdditionalPreview";
    f15 => "GetQueryUGCNumKeyValueTags";
    f16 => "GetQueryUGCKeyValueTag";
    f17 => "GetQueryUGCKeyValueTag";
    f18 => "GetQueryUGCContentDescriptors";
    f19 => "ReleaseQueryUGCRequest";
    f20 => "AddRequiredTag";
    f21 => "AddRequiredTagGroup";
    f22 => "AddExcludedTag";
    f23 => "SetReturnOnlyIDs";
    f24 => "SetReturnKeyValueTags";
    f25 => "SetReturnLongDescription";
    f26 => "SetReturnMetadata";
    f27 => "SetReturnChildren";
    f28 => "SetReturnAdditionalPreviews";
    f29 => "SetReturnTotalOnly";
    f30 => "SetReturnPlaytimeStats";
    f31 => "SetLanguage";
    f32 => "SetAllowCachedResponse";
    f33 => "SetCloudFileNameFilter";
    f34 => "SetMatchAnyTag";
    f35 => "SetSearchText";
    f36 => "SetRankedByTrendDays";
    f37 => "SetTimeCreatedDateRange";
    f38 => "SetTimeUpdatedDateRange";
    f39 => "AddRequiredKeyValueTag";
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

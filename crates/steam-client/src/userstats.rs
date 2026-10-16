//! SteamUserStats012 for offline play: no stats, achievements or leaderboards
//! exist, so every query reports failure (0 / false / invalid call handle /
//! empty string) and nothing is stored. Overloads keep header order (Itanium ABI).
use super::*;

macro_rules! stubs {
    ($($name:ident => $label:literal, $ret:expr;)*) => {
        $(unsafe extern "C" fn $name() -> usize {
            trace(concat!("userstats.", $label, " (no stats support)"));
            $ret
        })*
    };
}
stubs! {
    f0 => "RequestCurrentStats", 0;
    f1 => "GetStat", 0;
    f2 => "GetStat", 0;
    f3 => "SetStat", 0;
    f4 => "SetStat", 0;
    f5 => "UpdateAvgRateStat", 0;
    f6 => "GetAchievement", 0;
    f7 => "SetAchievement", 0;
    f8 => "ClearAchievement", 0;
    f9 => "GetAchievementAndUnlockTime", 0;
    f10 => "StoreStats", 0;
    f11 => "GetAchievementIcon", 0;
    f12 => "GetAchievementDisplayAttribute", c"".as_ptr() as usize;
    f13 => "IndicateAchievementProgress", 0;
    f14 => "GetNumAchievements", 0;
    f15 => "GetAchievementName", c"".as_ptr() as usize;
    f16 => "RequestUserStats", 0;
    f17 => "GetUserStat", 0;
    f18 => "GetUserStat", 0;
    f19 => "GetUserAchievement", 0;
    f20 => "GetUserAchievementAndUnlockTime", 0;
    f21 => "ResetAllStats", 0;
    f22 => "FindOrCreateLeaderboard", 0;
    f23 => "FindLeaderboard", 0;
    f24 => "GetLeaderboardName", c"".as_ptr() as usize;
    f25 => "GetLeaderboardEntryCount", 0;
    f26 => "GetLeaderboardSortMethod", 0;
    f27 => "GetLeaderboardDisplayType", 0;
    f28 => "DownloadLeaderboardEntries", 0;
    f29 => "DownloadLeaderboardEntriesForUsers", 0;
    f30 => "GetDownloadedLeaderboardEntry", 0;
    f31 => "UploadLeaderboardScore", 0;
    f32 => "AttachLeaderboardUGC", 0;
    f33 => "GetNumberOfCurrentPlayers", 0;
    f34 => "RequestGlobalAchievementPercentages", 0;
    f35 => "GetMostAchievedAchievementInfo", 0;
    f36 => "GetNextMostAchievedAchievementInfo", 0;
    f37 => "GetAchievementAchievedPercent", 0;
    f38 => "RequestGlobalStats", 0;
    f39 => "GetGlobalStat", 0;
    f40 => "GetGlobalStat", 0;
    f41 => "GetGlobalStatHistory", 0;
    f42 => "GetGlobalStatHistory", 0;
    f43 => "GetAchievementProgressLimits", 0;
    f44 => "GetAchievementProgressLimits", 0;
}
static TABLE: VTable<45> = VTable([
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
]);
pub(super) static OBJECT: Object = Object {
    table: TABLE.0.as_ptr(),
};

// ---- generated: additional interface versions ----

// STEAMUSERSTATS_INTERFACE_VERSION013 (SDK 165 header layout): slots matched to the base table by method name; the rest are unavailable.
static TABLE_013: VTable<44> = VTable([
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
]);
pub(super) static OBJECT_013: Object = Object {
    table: TABLE_013.0.as_ptr(),
};

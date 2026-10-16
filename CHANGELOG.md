# Changelog

## Unreleased

### Added
- **Achievement icons:** `GetAchievementIcon` and `ISteamUtils::GetImageSize/GetImageRGBA` return real images from Valve's image CDN (allow-listed hosts, size-bounded, JPEG and PNG), announced with `UserAchievementIconFetched_t`.
- **Workshop:** all-UGC and per-user queries with their filters and getters (tags, previews, key-value tags, children, statistics, cursors), subscribe and unsubscribe, item state, install and download info, `DownloadItem`, votes, favorites, playtime tracking, dependencies, deletion and `RequestUGCDetails`.
- **Workshop downloads:** subscribed items are downloaded in the background (SteamPipe items and legacy single-file items), verified, swapped in atomically and recorded in `appworkshop_<app>.acf`; the game gets `ItemInstalled_t` and `DownloadItemResult_t`.
- `ugc-probe`, `ugc-write-probe` and `workshop-probe` examples for live checks.

### Fixed
- `AddRequiredTag` on a Workshop query released the query instead of adding the tag.
- Backpack Battles: the opponent query asks for details of 1000 Workshop items at once; queries above 256 were rejected, so the game fell back to offline opponents.

### Known limits
- Publishing Workshop items is not supported (CreateItem, SubmitItemUpdate and the SetItem calls report failure).
- Voted-on user lists are not supported; friend avatars are not provided.

## 2.0.0 - 2026-10-16

The built-in Steam API library (`libsteamclient.so`) is now far more complete, so many more games start without Valve's client.

### Added
- **Steam API versions:** the library serves SteamClient 006-021 and 023, plus the modern (SDK 1.50+) and older (pre-1.50) versions of the Apps, Friends, Input, Matchmaking, Remote Play, Remote Storage, Timeline, UGC, UserStats and Utils interfaces. Layouts come from Valve's SDK headers.
- **Stats, achievements and leaderboards:** `ISteamUserStats` is implemented over the Steam connection. This covers stats, achievements, leaderboards (find, download, upload score and UGC), current player counts and global achievement percentages. Global stats are partial.
- **Workshop (UGC) queries by published file id,** including item details and metadata.
- `stats-probe`, a read-only example that checks stats and leaderboards against a live account.

### Changed
- **Downloads of many small files are much faster:** chunks are batched across files and download concurrency is raised from 8 to 32.
- The friends list reconnects when its feed ends, and waits while a game is running or installing.
- The library log suppresses identical lines after three repeats.

### Fixed
- Games whose own binary lacks the execute bit (for example Backpack Battles) now launch.
- The friends list no longer stays on "Unknown" after a long game session.

### Known limits
- Publishing Workshop items, subscriptions and Workshop downloads are not implemented.
- Achievement icons are not provided.
- Stat and score writes go to your real account, as with Valve's client.
- Unofficial: not affiliated with or endorsed by Valve.

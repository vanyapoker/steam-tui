# Changelog

## Unreleased

## 0.3.2 - 2026-10-19

### Fixed
- The commit phase of an install or update took hours on large games (about 6 files per second for Path of Exile 2 on btrfs) because every file move fsynced its directories. Moves are now made durable with one filesystem barrier per phase, the same for rollback and recovery, so the phase takes seconds to minutes. The write-ahead journal and its ordering guarantees are unchanged. The Committing phase also shows how many steps are done.

### Changed
- Transcoded media databases now live directly in `steamapps/shadercache/<app>`, the layout Valve's client uses, so either client reuses the other's download. Files from the old `fozmediav1` location are moved or removed, and a database already in place is adopted when its SHA-1 matches the bucket manifest.

## 0.3.1 - 2026-10-19

### Added
- **Launch options:** apps that offer selectable launch entries (e.g. Moonlighter 2's DirectX 11 / DirectX 12) can be switched with `O` in the library or `steam-tui content launch-option APPID [INDEX|default]`. The choice is remembered per game.

### Fixed (stutter)
- The Steam backend served one request at a time, so a slow stats or ownership call stalled the game's per-frame polls (seconds per ten-second window in Moonlighter 2). Requests are now served concurrently with a lock per service, and queue-only polls no longer wait on the connection probe.
- P2P signaling no longer makes a blocking backend round trip from the game thread every ten milliseconds; a background thread does it.
- The game log now reports every ten seconds which backend calls blocked the caller, when it is long enough to be felt.

### Fixed (Proton media)
- Proton games now get Valve's per-app transcoded video and audio databases (`transcoded_video.foz`, `transcoded_audio.foz`), downloaded and verified at launch into `steamapps/shadercache/<app>/fozmediav1` and passed to Proton through `STEAM_COMPAT_TRANSCODED_MEDIA_PATH`. Without them Proton cannot decode a game's videos, which showed up as lag in Moonlighter 2.

### Changed
- Panels use square corners like btop.
- Launches get persistent per-game GPU caches in `steamapps/shadercache/<app>` (DXVK state cache, Mesa and NVIDIA shader caches, NVIDIA cleanup disabled), the layout Valve's client uses, so pipelines stop being recompiled on every launch. Variables you already export are kept.

### Fixed
- Apps with only selectable launch options and no default entry (Moonlighter 2) launched with an error; the lowest plain option, which Steam's dialog preselects, is now used. Beta, DLC and account-conditional options remain unsupported.

## 0.3.0 - 2026-10-19

### Added
- **Updates and queue:** a persistent, account-scoped content queue (`J`) with pause, retry, cancel, priorities, download windows and update-before-launch policies; content properties (`P`) for branch, language and DLC, including protected branches; optional verified depot delta patches with ordinary chunk fallback; safe removal of obsolete managed files.
- **DLC:** DLC ownership and state APIs served through the authenticated backend, with safe optional-content install transactions.
- **Launch dependencies:** missing Proton, container runtimes and additional dependencies install automatically, and verified first-run prerequisites run in the game's Proton prefix; supported Linux setup actions run in the game tree.
- **Shader pre-caching (opt-in):** press `Z` on an installed game to download its Vulkan FOZ v6 pre-cache; games with a cache are refreshed hourly and processed before launch with a pinned standalone Fossilize helper (`S` skips). Nothing is downloaded unless requested. `shaders` CLI commands manage budget, clearing and the master switch.

### Changed
- Proton games are marked with `⊞` in the OS column instead of `⚛`, which rendered as an oversized block in common terminal fonts.
- The prebuilt archive now includes the Fossilize replay helper and its licenses.

### Fixed
- Launch-time shader processing no longer freezes ("Paused processing Vulkan shaders") while another game process exists.
- First-run setup no longer fails launches when the Proton prefix already has a Visual C++ 14.x runtime; the Microsoft redistributable installer does not complete under Wine.
- Backend callbacks are polled on a background thread, so slow backend operations cannot stall the game thread.

## 0.2.1 - 2026-10-17

### Added
- **Achievements viewer:** press `a` on a game for a modal listing its achievements with unlocked state and date, global rarity and descriptions. `s` cycles the sort (status, name, rarity, date), `S` reverses it, `f` filters all / unlocked / locked.
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

## 0.2.0 - 2026-10-16

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

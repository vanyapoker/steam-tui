# Changelog

## Unreleased

## 0.4.0 - 2026-10-21

### Added
- Steam app icons beside library names, with background loading for visible rows, local asset reuse, a persistent cache and an offline fallback.
- Ctrl+V pastes clipboard images as local chat attachments; Enter sends through Steam, and Ctrl+Delete removes the attachment. Text paste remains supported.
- Inline previews for Steam-hosted chat images, with native terminal graphics, a colored block fallback, background loading and a bounded memory cache.
- Select friends with arrows and press Enter for direct-message chat, with unread badges, live messages, read acknowledgements, typing indicators, paginated history and retained drafts. Chat uses the TUI's own account connection.
- Load all visible notification history retained by Steam, including read entries, then update the TUI feed live. Notifications show game names when available, unread markers, and type-specific text, including playtest invitations.
- `steam-tui notifications` lists account notification history without opening the TUI.

### Fixed
- Accept Steam's numeric HTTPS flags in image upload responses instead of rejecting successful begin-upload requests. Show upload error codes and image-download HTTP status.
- Report live service-notification overflow instead of silently losing messages.
- Preserve notification titles, bodies and timestamps across partial updates, and merge live changes with late history responses. Hidden and expired entries are excluded, history does not create new-arrival badges, and the previous 50-per-push and 100-entry limits are removed.
- Correct the clip-download and send-to-phone notification type labels.

## 0.3.4 - 2026-10-19

### Added
- **Virtual gamepad.** Games on the built-in backend now see one stable virtual Xbox 360 pad (USB id 28de:11ff, `Microsoft X-Box 360 pad 0`, like Valve's Steam Input) instead of the raw controller, which is hidden from Wine/SDL with `SDL_GAMECONTROLLER_IGNORE_DEVICES`. Input is forwarded unchanged (no remapping), rumble is forwarded back, and the virtual pad stays alive, at rest, when a Bluetooth pad drops out and is re-attached when it returns. Supports Xbox-compatible pads (xpad layout and the Bluetooth HID layout), DualSense and DualShock 4; needs write access to `/dev/uinput`, otherwise nothing changes. `STEAM_TUI_NO_VIRTUAL_GAMEPAD=1` disables it.
- **Steam Input device presence.** With a virtual gamepad, `ISteamInput` and `ISteamController` report one connected device (real type: Xbox 360/One, PS4, PS5) in gamepad slot 0, with the device-connected and gamepad-slot callbacks; `ISteamInput::Init` succeeds only in that case. Action data and motion data are inactive (the calls used to abort the game). `SteamInput001`/`002` now also serve `Init` and `RunFrame`. `STEAM_TUI_NO_GAMEPAD_PRESENCE=1` turns the presence reporting off.

### Known issues
- DualSense and DualShock 4 support follows the Linux drivers' axis layouts but has not been tried on real hardware. Touchpad, gyro and lightbar are not forwarded.
- Games that rely on Steam Input actions (rather than XInput/SDL) see a connected device but get no input from it; set `STEAM_TUI_NO_GAMEPAD_PRESENCE=1` if one misbehaves. Action sets and remapping are not implemented.

## 0.3.3 - 2026-10-19

### Fixed
- Path of Exile 2 could not log in on the built-in backend: it requests an encrypted app ticket, which was answered with "not supported", so the login never completed. `RequestEncryptedAppTicket` and `GetEncryptedAppTicket` now go through the Steam connection: the request returns a call result immediately, the ticket is fetched in the background, and the game receives Steam's `EncryptedAppTicket` message serialized exactly like Valve's client does. A refusal keeps its `EResult` and carries no ticket.
- Choosing a login server (or any text field) crashed games that ask for the gamepad text keyboard: `ShowGamepadTextInput`, `ShowFloatingGamepadTextInput`, `DismissFloatingGamepadTextInput` and the entered-text getters aborted the game. Without an overlay they now answer what Steam answers when it cannot show the keyboard (`false` / empty), so the game uses its own text entry.

### Known issues
- Steam Input is not implemented (no virtual gamepad, no remapping). Games see the physical pad through Wine's XInput, and Path of Exile 2 disconnected itself ("your controller device was disconnected") right after entering the game.

## 0.3.2 - 2026-10-19

### Fixed
- The commit phase of an install or update took hours on large games (about 6 files per second for Path of Exile 2 on btrfs) because every file move fsynced its directories. Moves are now made durable with one filesystem barrier per phase, the same for rollback and recovery, so the phase takes seconds to minutes. The write-ahead journal and its ordering guarantees are unchanged. The Committing phase also shows how many steps are done.

### Changed
- Panels, modals and overlays use btop's rounded corners again, and the title and hint segments on their borders are drawn as rounded notches (`╭─╮Library╭─╮`, `╰─╯news╰─╯`). The update queue and content overlays now share the other panels' style, with their key hints on the bottom border.
- State labels are short words. The library State column shows `new`, `installed`, `update` or `blocked` (the old `? installed` and `! unavailable` are folded into `installed` and `blocked`), and live `download 42%`, `verify 97%`, `install 100%`, `paused 42%`. The Install panel names its phases Prepare, Download, Verify, Install and Done, keeps the phase visible while paused (`⏸ Download`), counts files rather than mixing files with bytes, and no longer colours a calm commit message as a warning. While files are being moved the panel says that cancel waits for the install. The update queue prints plain words (`queued`, `running`, `done`; policy `auto`, `on launch`, `manual`) instead of Rust debug names.
- **Hotkeys were remapped; the old letters do nothing.** One table now drives key handling, the footers and the help screen. No letter has an upper-case twin with another meaning any more: `Enter` launches an installed game and installs a missing one (`i` is gone), `l` launches the installed version without updating (was `L` / Shift+Enter), `m` manages content (was `P`), `d` opens downloads (was `J`), `o` cycles launch options (was `O`), `z` pre-caches shaders (was `Z`), `y` syncs saves (was `C`), and `c` cancels the active job or, while a game is launching, skips shader processing (was `S`). `1`-`4` choose a sort and the active column's number reverses it (was `S`). Movement keys act on the focused pane; `o` (offline friends) and `x` (clear feed) only act in their own pane. Every overlay closes with `Esc`, `q` or its own key (`q` no longer quits from inside one) and shares `↑↓`/`jk`, `PgUp`/`PgDn` and `Home`/`End`. Footers show only the keys that apply to the selected game, and the status bar keeps `? help` and `q quit` on narrow terminals.
- Installs and updates no longer re-hash every file after the download. Like Valve's client, the check before commit is a quick scan: every chunk must have been verified and staged (downloaded and checked, validated from disk by the resume scan, or copied from a hash-verified old file) and each staged file must exist with exactly its manifest size. Per-file fsyncs are replaced by one filesystem barrier. Set `STEAM_TUI_FULL_VERIFY=1` for the previous whole-file SHA1 check, which then runs as each file completes while later files still download.
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

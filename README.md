# steam-tui

Lightweight, fast TUI Steam client for Linux (built for Omarchy). Browse, install, update and launch your library without the Steam client or its browser processes.

Early state: some games won't work without original steam client.

![steam-tui](assets/screenshot.png)

- Starts in ~2 ms and uses ~30 MB of memory.
- Install and update games (native and Proton), with a persistent queue, pause/resume, priorities and download windows.
- Check for updates before launch, with an explicit option to launch the installed version.
- Select branches, languages and optional DLC; protected branch keys stay in the keyring.
- Optional Vulkan shader pre-caches, requested per game with `Z`, processed before launch with progress and skip.
- Missing Proton and runtime dependencies install automatically before launch.
- Shared redistributables install per library; verified Windows prerequisites run in the Proton prefix and supported Linux setup actions run in the game tree.
- DLC enumeration, ownership, installed state, downloads, and optional removal through the game's Steamworks APIs.
- Friends, live notifications, game changelogs.
- Steam Cloud saves, with a side-by-side local/cloud conflict picker.
- Games run on our own Steam backend, so no Steam process is needed.

![save conflict picker](assets/conflict.png)

## Installation

Built for Omarchy (Arch, x86_64). Windows games need Proton, and a Secret Service keyring (GNOME Keyring or KWallet) stores your login.

**Prebuilt (fastest):**

```sh
curl -L https://github.com/vanyapoker/steam-tui/releases/latest/download/steam-tui-x86_64-linux.tar.gz | tar -xz -C ~/.local
steam-tui login --account YOUR_STEAM_NAME   # first run only
steam-tui
```

Make sure `~/.local/bin` is in your `PATH`.

**From source** (needs Rust, Git, Python 3, CMake, make, Perl and a C++ compiler; the first build takes a few minutes):

```sh
git clone https://github.com/vanyapoker/steam-tui && cd steam-tui
./install.sh
```

`install.sh` installs to `~/.local` without sudo. Use `PREFIX=/some/path ./install.sh` to change that. It builds and packages a pinned standalone `fossilize-replay`; no Steam installation or Vulkan capture layer is needed for shader processing. Vulkan drivers are needed at runtime. For development builds, set `STEAM_TUI_FOSSILIZE_REPLAY` to your standalone helper or place it beside the executable.

## Hotkeys

Press `?` in the app for the full list.

| Key | Action |
| --- | --- |
| Enter / double-click | Check/update, then launch |
| Shift+Enter / L | Explicitly launch the installed version |
| J | Persistent content/shader queue |
| P | Branch, language and DLC properties |
| O | Cycle the selected game's launch option (e.g. DirectX 11 / 12) |
| Z | Download a shader pre-cache for the selected game (opt-in) |
| S while launching | Skip shader processing and continue launch |
| Ctrl+Enter | Launch through Valve's Steam client |
| i / u | Install / update |
| p / c | Pause / cancel the download |
| C | Steam Cloud sync |
| n / right-click | Changelog |
| a | Achievements of the selected game (s / S sort, f filter, r reload) |
| / | Filter |
| f | Pin a game |
| v | All / installed / not installed |
| 1-4, s, S | Sort by column, cycle sort, reverse |
| Tab | Switch pane |
| r | Refresh |
| b | Backend traffic |
| q | Quit |

## Updates and shader caches

The queue runs while the TUI is open or `jobs run` is active. Closing it retains pending work; there is no background daemon. Background work pauses during games by default. Explicit foreground content downloads can override that pause. Normal launch checks fresh game metadata; an unavailable check requires the explicit installed-version choice.

```sh
steam-tui jobs list
steam-tui jobs run
steam-tui updates policy APPID automatic  # or launch-only / manual
steam-tui updates window 23:00 06:00
steam-tui content show APPID
steam-tui content launch-option APPID [INDEX|default]   # list or choose a launch entry (TUI: O)
steam-tui content unlock APPID BRANCH    # hidden password prompt; remembers returned key
steam-tui shaders add APPID              # opt in: queue a download (TUI: Z); then jobs run
steam-tui shaders show APPID
steam-tui shaders enabled off            # disables downloads and processing
steam-tui shaders clear APPID            # reclaim this game's generated cache
```

Shader pre-caching is opt-in per game: nothing is downloaded until you press `Z` (or run `shaders add APPID`); only games with a downloaded cache are refreshed afterwards. Vulkan FOZ v6 archives have their own freshness checks and verified staging. Compilation uses two threads, low CPU priority and a 2 GiB address-space limit per helper process. Failed/skipped processing never marks a context complete and never prevents launching the game. TUI `S` skips; CLI launch/process uses Ctrl+C to skip this optional stage. Shader archives have an 8 GiB account budget including staging, adjustable with `shaders budget MIB`; old generations and unused replay contexts are pruned. The budget excludes driver-created cache data. State and caches live under `$XDG_STATE_HOME/steam-tui/updates-ACCOUNT` (`~/.local/state` by default).

See [the update audit](docs/steam-update-gap-audit.md) for evidence, supported formats and validation limits.

## Steamworks coverage

| Works | Partial | Not yet |
| --- | --- | --- |
| User, Utils, Apps, Friends (basics) | Matchmaking and lobbies | Publishing Workshop items |
| Game Coordinator relay | Networking sockets, P2P and relay | Inventory |
| Steam Cloud | Auth tickets | Input, Screenshots |
| Stats, achievements (with icons), leaderboards | Global stats | Friend avatars |
| Workshop: queries, subscriptions, downloads, votes, favorites | | |

Interface versions from SDK 0.99 to 1.65 are served, laid out from Valve's SDK headers: SteamClient 006-021 and 023 (not 022), and every version Valve's public headers ever shipped for User, Utils, Apps, Friends, Matchmaking, Servers, UserStats, Networking, RemoteStorage, Screenshots, HTTP, HTMLSurface, Controller, Inventory, UGC and Input. Where an old version's call has no matching implementation, the slot logs and returns zero. Stats, achievements and leaderboard scores are read from and written to your real Steam account, like the normal client. Workshop items you subscribe to are downloaded in the background into the game's library (`steamapps/workshop/content/<app>/<item>`, recorded in Valve's `appworkshop_<app>.acf`) while the game runs; subscribing, voting, favoriting and the other Workshop calls act on your real account. Publishing items (CreateItem, SubmitItemUpdate) is not supported and reports failure, and so are the voted-on user lists. Unsupported calls log and abort rather than pretend to work.

## Limitations

- VAC-secured games run through Valve's Steam client without its web helpers. It must be signed in to the same account, and costs about 200 MB extra. Enter picks this automatically.
- Online matchmaking and relay networking are unfinished.
- In-game DLC changes preserve the parent build; a parent update or ambiguous metadata requires closing/updating the game. Removal retains modified files and user configuration. Windows setup scripts run at the next launch. `SetDlcContext` usage reporting is unavailable.
- Shader download/replay currently covers Vulkan FOZ v6. Legacy GPU-specific binary caches, media transcode caches, and capture/upload are not implemented. Real-driver replay performance has not been measured.
- Linux only.

## Not implemented yet

Chat, game invites, publishing Workshop items, store and community pages, trading.

## License

[MIT](LICENSE). Vendored and third-party code keeps its own license, see [THIRD_PARTY.md](THIRD_PARTY.md).


> Not affiliated with or endorsed by Valve; Steam and VAC are Valve trademarks. steam-tui signs in to your real Steam account through its own client, which Valve's terms may not allow. Use it at your own risk.

Steam announcement regression corpus, fetched on 2026-10-22.

Mycopunk reproduces the reported `<3` parsing failure in “1.0 IS HERE!!!”.
The other five games were chosen with Python's `random.SystemRandom().sample`
from local game manifests, excluding Mycopunk, Proton, runtimes, and redistributables.
The JSON files contain the ten latest full announcements for each game, unchanged
from the public ISteamNews API. Tests use these files offline and never launch games
or the Steam client.

To preview a fixture through the real TUI renderer without opening a window:

```sh
STEAM_TUI_SCREENSHOT=/tmp/mycopunk-news.svg \
STEAM_TUI_SCREENSHOT_VIEW=news-fixture \
STEAM_TUI_SCREENSHOT_NEWS_APP=3247750 \
STEAM_TUI_SCREENSHOT_NEWS_ARTICLE=3 \
STEAM_TUI_SCREENSHOT_NEWS_ANCHOR='The Supporter Pack' \
cargo test -p steam-tui readme_screenshot -- --ignored
rsvg-convert /tmp/mycopunk-news.svg -o /tmp/mycopunk-news.png
```

Article indexes start at zero. Omit the anchor to show the article from the top;
`STEAM_TUI_SCREENSHOT_SIZE=80x24` checks a smaller terminal. Image placeholders are
shown by default; `STEAM_TUI_SCREENSHOT_DOWNLOAD_NEWS=1` enables CDN downloads.

Sources (game announcement text and images belong to their respective publishers):

- [Mycopunk](https://api.steampowered.com/ISteamNews/GetNewsForApp/v2/?appid=3247750&count=10&maxlength=0&feeds=steam_community_announcements)
- [Deep Rock Galactic: Survivor](https://api.steampowered.com/ISteamNews/GetNewsForApp/v2/?appid=2321470&count=10&maxlength=0&feeds=steam_community_announcements)
- [Deadlock](https://api.steampowered.com/ISteamNews/GetNewsForApp/v2/?appid=1422450&count=10&maxlength=0&feeds=steam_community_announcements)
- [Backpack Battles](https://api.steampowered.com/ISteamNews/GetNewsForApp/v2/?appid=2427700&count=10&maxlength=0&feeds=steam_community_announcements)
- [Far Far West](https://api.steampowered.com/ISteamNews/GetNewsForApp/v2/?appid=3124540&count=10&maxlength=0&feeds=steam_community_announcements)
- [Team Fortress 2](https://api.steampowered.com/ISteamNews/GetNewsForApp/v2/?appid=440&count=10&maxlength=0&feeds=steam_community_announcements)

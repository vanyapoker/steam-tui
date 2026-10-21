#!/usr/bin/env bash
# Render mock sessions through the real TUI, without opening a terminal or signing in.
set -euo pipefail
repo_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
for tool in cargo rsvg-convert; do
  command -v "$tool" >/dev/null 2>&1 || { printf 'Missing screenshot dependency: %s\n' "$tool" >&2; exit 1; }
done
output_dir="$repo_dir/target/readme"
mkdir -p "$output_dir"
export TZ=UTC
export STEAM_TUI_SCREENSHOT_SIZE=132x36
# A caller can supply another Alacritty palette; otherwise use the current theme if present.
theme="${XDG_STATE_HOME:-$HOME/.local/state}/omarchy/current/theme/alacritty.toml"
if [[ -z "${STEAM_TUI_SCREENSHOT_THEME:-}" && -f "$theme" ]]; then
  export STEAM_TUI_SCREENSHOT_THEME="$theme"
fi
# Existing cached game artwork is reused. Set STEAM_TUI_SCREENSHOT_DOWNLOAD_ICONS=1
# to allow public CDN requests for games whose metadata is already cached.
for view in library conflict chat; do
  export STEAM_TUI_SCREENSHOT="$output_dir/$view.svg"
  export STEAM_TUI_SCREENSHOT_VIEW="$view"
  cargo test --locked -p steam-tui --manifest-path "$repo_dir/Cargo.toml" readme_screenshot -- --ignored
  filename="$view-0.4"
  rsvg-convert -z 2 "$STEAM_TUI_SCREENSHOT" -o "$repo_dir/assets/$filename.png"
done

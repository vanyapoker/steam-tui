#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != Linux ]]; then
  printf 'steam-tui supports Linux only.\n' >&2
  exit 1
fi
if ! command -v cargo >/dev/null 2>&1; then
  printf 'Cargo is required. Install a Rust toolchain, then rerun install.sh.\n' >&2
  exit 1
fi

repo_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
prefix="${PREFIX:-${HOME:?HOME must be set}/.local}"
bin_dir="$prefix/bin"

cargo build --locked --release -p steam-tui -p steam-client \
  --manifest-path "$repo_dir/Cargo.toml" --target-dir "$repo_dir/target"
# Build before replacing installed files, so missing compiler dependencies leave
# the existing installation usable. No Vulkan process or game is launched.
bash "$repo_dir/tools/build-fossilize.sh"
mkdir -p -- "$bin_dir"
staged="$(mktemp "$bin_dir/.steam-tui.XXXXXX")"
trap 'rm -f -- "$staged"' EXIT
install -m 0755 -- "$repo_dir/target/release/steam-tui" "$staged"
mv -f -- "$staged" "$bin_dir/steam-tui"
printf 'Installed %s\n' "$bin_dir/steam-tui"

# The built-in Steam API library used when the Valve Steam host is disabled (H).
lib_dir="$prefix/lib/steam-tui"
mkdir -p -- "$lib_dir"
install -m 0755 -- "$repo_dir/target/release/libsteamclient.so" "$lib_dir/libsteamclient.so"
printf 'Installed %s\n' "$lib_dir/libsteamclient.so"
fossilize_build="${STEAM_TUI_FOSSILIZE_BUILD:-$repo_dir/target/fossilize-build}"
fossilize_source="${STEAM_TUI_FOSSILIZE_SOURCE:-$repo_dir/target/fossilize-source}"
install -m 0755 -- "$fossilize_build/cli/fossilize-replay" "$lib_dir/fossilize-replay"
license_dir="$prefix/share/licenses/steam-tui/fossilize"
mkdir -p -- "$license_dir"
install -m 0644 -- "$fossilize_source/LICENSE" "$license_dir/LICENSE"
install -m 0644 -- "$fossilize_source/miniz/LICENSE" "$license_dir/miniz-LICENSE"
install -m 0644 -- "$fossilize_source/rapidjson/license.txt" "$license_dir/rapidjson-LICENSE"
install -m 0644 -- "$fossilize_source/cli/volk/LICENSE.md" "$license_dir/volk-LICENSE"
install -m 0644 -- "$fossilize_source/cli/SPIRV-Headers/LICENSE" "$license_dir/SPIRV-Headers-LICENSE"
printf 'Installed %s\n' "$lib_dir/fossilize-replay"

case ":${PATH:-}:" in
  *":$bin_dir:"*) printf 'Run steam-tui to open the TUI.\n' ;;
  *) printf 'Add %s to your shell PATH, then run steam-tui.\n' "$bin_dir" ;;
esac

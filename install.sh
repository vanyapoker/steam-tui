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

case ":${PATH:-}:" in
  *":$bin_dir:"*) printf 'Run steam-tui to open the TUI.\n' ;;
  *) printf 'Add %s to your shell PATH, then run steam-tui.\n' "$bin_dir" ;;
esac

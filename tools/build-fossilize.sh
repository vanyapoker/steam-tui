#!/usr/bin/env bash
# Standalone helper, built from the official project and pinned submodules.
# Does not install a Vulkan capture layer or depend on a Steam installation.
set -euo pipefail
repo_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
revision=556da9ff7d7616704dda515756219ab1dab693ec
source_dir="${STEAM_TUI_FOSSILIZE_SOURCE:-$repo_dir/target/fossilize-source}"
build_dir="${STEAM_TUI_FOSSILIZE_BUILD:-$repo_dir/target/fossilize-build}"
for tool in git cmake c++ python3; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    printf 'Shader helper build requires %s. Install it, then rerun install.sh.\n' "$tool" >&2
    exit 1
  fi
done
if [[ ! -d "$source_dir/.git" ]]; then
  git clone --filter=blob:none --no-checkout https://github.com/ValveSoftware/Fossilize.git "$source_dir"
fi
# Never resolve a floating branch or substitute a system/Valve Steam binary.
git -C "$source_dir" fetch --depth 1 https://github.com/ValveSoftware/Fossilize.git "$revision"
git -C "$source_dir" checkout --detach "$revision"
git -C "$source_dir" submodule update --init --depth 1 rapidjson cli/volk cli/SPIRV-Cross cli/SPIRV-Tools cli/SPIRV-Headers
cmake -S "$source_dir" -B "$build_dir" \
  -DCMAKE_BUILD_TYPE=Release -DFOSSILIZE_VULKAN_LAYER=OFF \
  -DFOSSILIZE_TESTS=OFF -DFOSSILIZE_REPLAYER_SPIRV_VAL=OFF -DSPIRV_SKIP_TESTS=ON
cmake --build "$build_dir" --target fossilize-replay -j "${STEAM_TUI_BUILD_JOBS:-2}"
printf 'Built standalone replay helper: %s/cli/fossilize-replay\n' "$build_dir"

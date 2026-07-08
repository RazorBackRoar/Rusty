#!/bin/zsh
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd -P)"
icons_dir="$repo_root/src-tauri/icons"
source_svg="$icons_dir/icon-source.svg"

export PATH="$repo_root/.cargo/bin:$PATH"

[[ -f "$source_svg" ]] || {
    print -u2 "Missing icon source: $source_svg"
    exit 1
}

print "Generating Rusty icons from $source_svg"
cd "$repo_root"
cargo tauri icon "$source_svg" -o "$icons_dir"

# Keep a project-named icns alongside icon.icns for DMG volume-icon lookup parity
# with the Python apps (universal-build.sh prefers <App>.icns).
cp -f "$icons_dir/icon.icns" "$icons_dir/Rusty.icns"

print "Icons updated in $icons_dir"

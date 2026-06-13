#!/bin/zsh
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd -P)"
cd "$repo_root"

export CARGO_TARGET_DIR="$repo_root/target"
export PATH="$repo_root/.cargo/bin:$PATH"

mkdir -p "$repo_root/build-logs"
timestamp="$(date +%Y%m%d-%H%M%S)"
log_path="$repo_root/build-logs/release-build-$timestamp.log"

exec > >(tee -a "$log_path") 2>&1

print "Rusty release build"
print "Repository: $repo_root"
print "Log: $log_path"
print "Started: $(date)"
print ""

"$repo_root/scripts/release-preflight.zsh" --check-config-only
print ""

cargo tauri build
print ""

# Build the DMG using the universal workspace layout
dmg_settings="$repo_root/../.razorcore/dmg-settings.py"
app_abs="$repo_root/target/release/bundle/macos/Rusty.app"
version="$(sed -nE 's/.*"version"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/p' "$repo_root/src-tauri/tauri.conf.json" | head -n 1)"
[[ -n "$version" ]] || {
    print "Error: could not read the Tauri app version."
    exit 1
}
dmg_path="$repo_root/target/release/bundle/dmg/Rusty_${version}_aarch64.dmg"
mkdir -p "$(dirname "$dmg_path")"
rm -f "$dmg_path"

if [[ -d "/Volumes/Rusty" ]]; then
    hdiutil detach "/Volumes/Rusty" -force -quiet 2>/dev/null || true
fi

print "Building DMG with universal layout..."
uvx --from dmgbuild dmgbuild -s "$dmg_settings" -D "app=$app_abs" -D "app_name=Rusty" "Rusty" "$dmg_path"

if ! uv run --no-project --python 3.14 python "$repo_root/../.razorcore/verify-dmg-layout.py" "$dmg_path" "Rusty"; then
    print "Error: DMG failed layout verification."
    exit 1
fi
print "DMG built successfully."
print ""

"$repo_root/scripts/release-preflight.zsh" --verify-log "$log_path" --verify-bundles

print ""
print "Finished: $(date)"
print "Release log: $log_path"

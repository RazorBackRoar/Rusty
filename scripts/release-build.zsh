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

if [[ -x "$repo_root/scripts/generate-icons.zsh" ]]; then
    "$repo_root/scripts/generate-icons.zsh"
    print ""
fi

cargo tauri build
print ""

# Build the DMG with the same shared dmgbuild layout as the Python apps.
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

vol_icns="$app_abs/Contents/Resources/Rusty.icns"
if [[ ! -f "$vol_icns" ]]; then
    vol_icns="$app_abs/Contents/Resources/icon.icns"
fi

dmg_defines=(-D "app=$app_abs" -D "app_name=Rusty")
if [[ -f "$vol_icns" ]]; then
    dmg_defines+=(-D "vol_icon=$vol_icns")
fi

if ! command -v uvx >/dev/null 2>&1; then
    print "Error: uvx is required to build the DMG with the shared layout."
    exit 1
fi

dmg_ok=0
for attempt in 1 2 3; do
    if [[ -d "/Volumes/Rusty" ]]; then
        hdiutil detach "/Volumes/Rusty" -force -quiet 2>/dev/null || true
    fi
    rm -f "$dmg_path"
    print "Building DMG with universal layout (attempt ${attempt}/3)..."
    if uvx --from dmgbuild dmgbuild -s "$dmg_settings" "${dmg_defines[@]}" "Rusty" "$dmg_path"; then
        dmg_ok=1
        break
    fi
    print "Warning: DMG build attempt ${attempt}/3 failed; retrying..."
    sleep 2
done

if [[ $dmg_ok -ne 1 ]]; then
    print "Error: DMG build failed after 3 attempts."
    exit 1
fi

if ! uv run --no-project --python 3.14 python "$repo_root/../.razorcore/verify-dmg-layout.py" "$dmg_path" "Rusty"; then
    print "Error: DMG failed layout verification."
    exit 1
fi
print "DMG built successfully."
print "  $dmg_path"
print ""

# Stage the final DMG into dist/ and remove the intermediate .app bundle and
# any extra DMGs so the workspace app folder contains exactly one DMG.
print "Staging final artifact into dist/..."
dist_dir="$repo_root/dist"
mkdir -p "$dist_dir"
rm -rf "$dist_dir/Rusty.app" "$dist_dir/Rusty.dmg"
cp -f "$dmg_path" "$dist_dir/Rusty.dmg"
rm -rf "$app_abs"
rm -f "$dmg_path"
print "  $dist_dir/Rusty.dmg"

print ""
"$repo_root/scripts/release-preflight.zsh" --verify-log "$log_path" --verify-bundles

print ""
print "Finished: $(date)"
print "Release log: $log_path"

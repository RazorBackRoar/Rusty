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

# Dynamic-year RazorBackRoar copyright, then shared locked-layout DMG packager.
app_abs="$repo_root/target/release/bundle/macos/Rusty.app"
"$repo_root/../.razorcore/patch-app-branding.sh" "$app_abs"
# Re-sign after plist branding so the gate + DMG see a coherent bundle.
codesign --force --deep --sign - "$app_abs"

version="$(sed -nE 's/.*"version"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/p' "$repo_root/src-tauri/tauri.conf.json" | head -n 1)"
[[ -n "$version" ]] || {
    print "Error: could not read the Tauri app version."
    exit 1
}
dmg_path="$repo_root/target/release/bundle/dmg/Rusty_${version}_aarch64.dmg"
mkdir -p "$(dirname "$dmg_path")"

"$repo_root/../.razorcore/package-dmg.sh" \
  --app "$app_abs" \
  --dmg "$dmg_path" \
  --app-name "Rusty" \
  --volname "Rusty"
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

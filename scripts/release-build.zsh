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

"$repo_root/scripts/release-preflight.zsh" --verify-log "$log_path" --verify-bundles

print ""
print "Finished: $(date)"
print "Release log: $log_path"

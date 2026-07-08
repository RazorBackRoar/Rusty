#!/bin/zsh
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
preflight="$repo_root/scripts/release-preflight.zsh"
release_build="$repo_root/scripts/release-build.zsh"
tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/rusty-release-preflight.XXXXXX")"
trap 'rm -rf "$tmp_dir"' EXIT
version="$(sed -nE 's/.*"version"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/p' "$repo_root/src-tauri/tauri.conf.json" | head -n 1)"

run_expect_pass() {
    local name="$1"
    shift
    if ! "$@" >"$tmp_dir/$name.out" 2>"$tmp_dir/$name.err"; then
        print -u2 "FAIL expected pass: $name"
        print -u2 "stdout:"
        cat "$tmp_dir/$name.out" >&2
        print -u2 "stderr:"
        cat "$tmp_dir/$name.err" >&2
        exit 1
    fi
}

run_expect_fail() {
    local name="$1"
    shift
    if "$@" >"$tmp_dir/$name.out" 2>"$tmp_dir/$name.err"; then
        print -u2 "FAIL expected failure: $name"
        print -u2 "stdout:"
        cat "$tmp_dir/$name.out" >&2
        exit 1
    fi
}

stale_log="$tmp_dir/stale.log"
cat >"$stale_log" <<'EOF'
Compiling rustydups v0.2.0 (/Users/home/Workspace/Apps/RustyDups/src-tauri)
Built application at: /Users/home/Workspace/Apps/RustyDups/target/release/rustydups
Signing /Users/home/Workspace/Apps/RustyDups/target/release/bundle/macos/Rusty.app/Contents/MacOS/rustydups
EOF

good_log="$tmp_dir/good.log"
cat >"$good_log" <<EOF
Rusty release build
Repository: $repo_root
Log: $good_log
Compiling rusty v$version ($repo_root/src-tauri)
Built application at: $repo_root/target/release/rusty
Finished 1 bundle at:
    $repo_root/target/release/bundle/macos/Rusty.app
Building DMG with universal layout (attempt 1/3)...
DMG built successfully.
  $repo_root/target/release/bundle/dmg/Rusty_${version}_aarch64.dmg
EOF

wrong_root_log="$tmp_dir/wrong-root.log"
cat >"$wrong_root_log" <<EOF
Rusty release build
Repository: /Users/home/Workspace/OtherRusty
Log: $wrong_root_log
Compiling rusty v$version (/Users/home/Workspace/OtherRusty/src-tauri)
Built application at: /Users/home/Workspace/OtherRusty/target/release/rusty
Finished 1 bundle at:
    /Users/home/Workspace/OtherRusty/target/release/bundle/macos/Rusty.app
Building DMG with universal layout (attempt 1/3)...
DMG built successfully.
  /Users/home/Workspace/OtherRusty/target/release/bundle/dmg/Rusty_${version}_aarch64.dmg
EOF

portable_root="$tmp_dir/Rusty"
mkdir -p "$portable_root/src-tauri"
cat >"$portable_root/Cargo.toml" <<'EOF'
[workspace]
members = ["src-tauri"]
EOF
cat >"$portable_root/src-tauri/Cargo.toml" <<'EOF'
[package]
name = "rusty"
version = "0.0.0"
edition = "2021"

[lib]
path = "src/lib.rs"
EOF
mkdir -p "$portable_root/src-tauri/src"
print 'pub fn placeholder() {}' >"$portable_root/src-tauri/src/lib.rs"
cat >"$portable_root/src-tauri/tauri.conf.json" <<'EOF'
{
  "productName": "Rusty",
  "bundle": {
    "active": true,
    "targets": ["app"],
    "macOS": {
      "signingIdentity": "-",
      "dmg": {
        "windowSize": { "width": 500, "height": 360 },
        "appPosition": { "x": 130, "y": 160 },
        "applicationFolderPosition": { "x": 370, "y": 160 }
      }
    }
  }
}
EOF

run_expect_pass config "$preflight" --check-config-only
run_expect_fail wrong-root "$preflight" --repo-root /Users/home/Workspace/Apps/RustyDups --check-config-only
run_expect_fail redirected-target env CARGO_TARGET_DIR=/Users/home/Workspace/RustyDups/target \
    "$preflight" --check-config-only
run_expect_pass release-target-lock grep -Fq \
    'export CARGO_TARGET_DIR="$repo_root/target"' "$release_build"
run_expect_pass portable-config "$preflight" --allow-other-root --repo-root "$portable_root" --check-config-only

sed -i '' 's/"width": 500/"width": 501/' "$portable_root/src-tauri/tauri.conf.json"
run_expect_fail changed-dmg-lock "$preflight" --allow-other-root --repo-root "$portable_root" --check-config-only

run_expect_fail stale-log "$preflight" --verify-log "$stale_log"
run_expect_fail wrong-log-root "$preflight" --verify-log "$wrong_root_log"
run_expect_pass good-log "$preflight" --verify-log "$good_log"

print "release preflight tests passed"

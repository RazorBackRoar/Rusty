#!/bin/zsh
set -euo pipefail

expected_root="/Users/home/Workspace/Apps/Rusty"
repo_root="$(cd "$(dirname "$0")/.." && pwd -P)"
check_config=false
verify_bundles=false
verify_log=""
allow_other_root=false

fail() {
    print -u2 "release preflight failed: $*"
    exit 1
}

usage() {
    cat <<'EOF'
Usage: scripts/release-preflight.zsh [options]

Options:
  --check-config-only      verify this checkout is the active Rusty release source
  --verify-log PATH        reject build logs that contain stale RustyDups paths/binaries
  --verify-bundles         verify Rusty.app and Rusty_*.dmg landed under this checkout
  --allow-other-root       allow CI/test checkouts outside the canonical local path
  --repo-root PATH         override repo root for tests
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --check-config-only)
            check_config=true
            shift
            ;;
        --verify-log)
            [[ $# -ge 2 ]] || fail "--verify-log requires a path"
            verify_log="$2"
            shift 2
            ;;
        --verify-bundles)
            verify_bundles=true
            shift
            ;;
        --allow-other-root)
            allow_other_root=true
            shift
            ;;
        --repo-root)
            [[ $# -ge 2 ]] || fail "--repo-root requires a path"
            repo_root="$(cd "$2" 2>/dev/null && pwd -P)" || repo_root="$2"
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            fail "unknown option: $1"
            ;;
    esac
done

if [[ "$check_config" == false && -z "$verify_log" && "$verify_bundles" == false ]]; then
    check_config=true
fi

reject_stale_path() {
    local value="$1"
    local label="$2"
    [[ "$value" != *"/RustyDups"* ]] || fail "$label points at stale RustyDups path: $value"
    [[ "$value" != *"/rustydups"* ]] || fail "$label points at stale rustydups path: $value"
}

check_release_config() {
    if [[ "$allow_other_root" == false ]]; then
        [[ "$repo_root" == "$expected_root" ]] || fail "repo root must be $expected_root, got $repo_root"
    fi
    reject_stale_path "$repo_root" "repo root"

    [[ -f "$repo_root/Cargo.toml" ]] || fail "missing root Cargo.toml"
    [[ -f "$repo_root/src-tauri/Cargo.toml" ]] || fail "missing src-tauri/Cargo.toml"
    [[ -f "$repo_root/src-tauri/tauri.conf.json" ]] || fail "missing src-tauri/tauri.conf.json"

    local cargo_metadata workspace_root target_directory
    if ! cargo_metadata="$(cd "$repo_root" && cargo metadata --no-deps --format-version 1)"; then
        fail "could not resolve Cargo workspace metadata from $repo_root"
    fi
    workspace_root="$(print -r -- "$cargo_metadata" | sed -nE 's/.*"workspace_root":"([^"]+)".*/\1/p')"
    target_directory="$(print -r -- "$cargo_metadata" | sed -nE 's/.*"target_directory":"([^"]+)".*/\1/p')"
    [[ "$workspace_root" == "$repo_root" ]] \
        || fail "Cargo workspace root must be $repo_root, got ${workspace_root:-<missing>}"
    [[ "$target_directory" == "$repo_root/target" ]] \
        || fail "Cargo target directory must be $repo_root/target, got ${target_directory:-<missing>}"

    grep -Eq '^[[:space:]]*name[[:space:]]*=[[:space:]]*"rusty"[[:space:]]*$' "$repo_root/src-tauri/Cargo.toml" \
        || fail "src-tauri/Cargo.toml must name the package/bin rusty"
    ! grep -Eq '^[[:space:]]*name[[:space:]]*=[[:space:]]*"rustydups"[[:space:]]*$' "$repo_root/src-tauri/Cargo.toml" \
        || fail "src-tauri/Cargo.toml still names a rustydups package or binary"
    grep -Eq '"productName"[[:space:]]*:[[:space:]]*"Rusty"' "$repo_root/src-tauri/tauri.conf.json" \
        || fail "tauri.conf.json productName must be Rusty"
    ! grep -Eq 'RustyDups|rustydups' "$repo_root/src-tauri/tauri.conf.json" \
        || fail "tauri.conf.json contains stale RustyDups/rustydups text"

    local -a python_runner
    if command -v uv >/dev/null 2>&1; then
        python_runner=(uv run --no-project --python 3.14 python)
    else
        python_runner=(python3)
    fi

    "${python_runner[@]}" - "$repo_root/src-tauri/tauri.conf.json" <<'PY' \
        || fail "tauri.conf.json does not match the locked Rusty bundle/DMG contract"
import json
import sys
from pathlib import Path

config = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
bundle = config.get("bundle", {})
macos = bundle.get("macOS", {})
dmg = macos.get("dmg", {})

expected = {
    "bundle.active": (bundle.get("active"), True),
    "bundle.targets": (bundle.get("targets"), ["app"]),
    "bundle.macOS.signingIdentity": (macos.get("signingIdentity"), "-"),
    "dmg.windowSize": (dmg.get("windowSize"), {"width": 500, "height": 420}),
    "dmg.appPosition": (dmg.get("appPosition"), {"x": 130, "y": 160}),
    "dmg.applicationFolderPosition": (
        dmg.get("applicationFolderPosition"),
        {"x": 370, "y": 160},
    ),
}

failures = [
    f"{name}: expected {wanted!r}, got {actual!r}"
    for name, (actual, wanted) in expected.items()
    if actual != wanted
]
if failures:
    print("\n".join(failures), file=sys.stderr)
    raise SystemExit(1)
PY

    [[ ! -e "$repo_root/target/release/rustydups" ]] \
        || fail "stale binary exists: $repo_root/target/release/rustydups"
    if [[ -d "$repo_root/target/release/bundle" ]]; then
        local stale_bundle
        stale_bundle="$(find "$repo_root/target/release/bundle" \( -name '*RustyDups*' -o -name 'rustydups' \) -print -quit)"
        [[ -z "$stale_bundle" ]] || fail "stale bundle artifact exists: $stale_bundle"
    fi
}

verify_build_log() {
    local log_path="$1"
    [[ -f "$log_path" ]] || fail "build log not found: $log_path"

    local matches
    matches="$(grep -En 'RustyDups|/rustydups|target/release/rustydups|Contents/MacOS/rustydups|Compiling rustydups|Built application at: .*rustydups' "$log_path" || true)"
    [[ -z "$matches" ]] || fail "stale RustyDups path or rustydups binary found in $log_path:
$matches"

    local version
    version="$(grep -E '"version"[[:space:]]*:' "$repo_root/src-tauri/tauri.conf.json" | head -n 1 | sed -E 's/.*"version"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/')"
    [[ -n "$version" ]] || fail "could not read Tauri version"

    local expected
    for expected in \
        "Repository: $repo_root" \
        "Log: $log_path" \
        "Built application at: $repo_root/target/release/rusty" \
        "$repo_root/target/release/bundle/macos/Rusty.app" \
        "$repo_root/target/release/bundle/dmg/Rusty_${version}_aarch64.dmg" \
        "$repo_root/dist/Rusty.dmg"
    do
        grep -Fq "$expected" "$log_path" \
            || fail "build log does not record expected Rusty output: $expected"
    done
}

verify_bundle_outputs() {
    local version
    version="$(grep -E '"version"[[:space:]]*:' "$repo_root/src-tauri/tauri.conf.json" | head -n 1 | sed -E 's/.*"version"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/')"
    [[ -n "$version" ]] || fail "could not read Tauri version"

    local binary="$repo_root/target/release/rusty"
    local dist_app="$repo_root/dist/Rusty.app"
    local dist_dmg="$repo_root/dist/Rusty.dmg"
    local target_app="$repo_root/target/release/bundle/macos/Rusty.app"
    local target_dmg="$repo_root/target/release/bundle/dmg/Rusty_${version}_aarch64.dmg"

    reject_stale_path "$binary" "release binary"
    reject_stale_path "$dist_dmg" "dist dmg"

    [[ -x "$binary" ]] || fail "missing executable release binary: $binary"
    [[ -f "$dist_dmg" ]] || fail "missing final DMG: $dist_dmg"
    [[ ! -e "$dist_app" ]] || fail "app bundle should not be kept in dist/: $dist_app"
    [[ ! -d "$target_app" ]] || fail "app bundle should not be kept in target/release/bundle/macos/: $target_app"
    [[ ! -f "$target_dmg" ]] || fail "intermediate DMG should not be kept in target/release/bundle/dmg/: $target_dmg"

    print "Verified final output:"
    print "  $dist_dmg"
}

if [[ "$check_config" == true ]]; then
    check_release_config
    print "Release config OK: $repo_root"
fi

if [[ -n "$verify_log" ]]; then
    verify_build_log "$verify_log"
    print "Build log OK: $verify_log"
fi

if [[ "$verify_bundles" == true ]]; then
    verify_bundle_outputs
fi

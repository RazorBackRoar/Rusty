#!/bin/zsh
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
preflight="$repo_root/scripts/release-preflight.zsh"
tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/rusty-release-preflight.XXXXXX")"
trap 'rm -rf "$tmp_dir"' EXIT

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
Compiling rusty v0.2.0 ($repo_root/src-tauri)
Built application at: $repo_root/target/release/rusty
Finished 2 bundles at:
    $repo_root/target/release/bundle/macos/Rusty.app
    $repo_root/target/release/bundle/dmg/Rusty_0.2.0_aarch64.dmg
EOF

run_expect_pass config "$preflight" --check-config-only
run_expect_fail wrong-root "$preflight" --repo-root /Users/home/Workspace/Apps/RustyDups --check-config-only
run_expect_fail stale-log "$preflight" --verify-log "$stale_log"
run_expect_pass good-log "$preflight" --verify-log "$good_log"

print "release preflight tests passed"

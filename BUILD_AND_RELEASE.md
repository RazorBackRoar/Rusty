# Build & Release — Rusty

Organization-standard build and release guide for
[RazorBackRoar/Rusty](https://github.com/RazorBackRoar/Rusty).

## Overview

Rusty is a native macOS duplicate photo/video finder built with **Rust** and
**Tauri 2**, packaged as an Apple Silicon `.app` / `.dmg`.

## Platform Requirements

| Requirement | Value |
|-------------|-------|
| OS | macOS (Apple Silicon) |
| Arch | `arm64` |
| Toolchain | Rust via Homebrew (`brew install rust`) |
| Node.js | **Not required** |

## Prerequisites

```zsh
cd /path/to/Rusty
# Install Tauri CLI into the workspace-local .cargo dir (once)
cargo install --root .cargo tauri-cli@^2 --locked
```

## Development Build

```zsh
cargo check --workspace
cargo test --workspace
```

## Packaging

```zsh
zsh scripts/release-build.zsh
```

Bundle lands at:

```text
dist/Rusty.dmg
```

The `.app` bundle and intermediate `Rusty_*_aarch64.dmg` under `target/release/bundle/`
are removed during packaging; the workspace app folder contains only one DMG.
The Mach-O binary inside the shipped app is named `rusty` (lowercase). Each release run
writes a timestamped log under `build-logs/`.

Preflight-only check:

```zsh
zsh scripts/release-preflight.zsh --allow-other-root --check-config-only
```

## Release Process

1. Ensure `main` is green (`cargo test --workspace` / CI).
2. Confirm versions in `src-tauri/Cargo.toml` and `tauri.conf.json` match.
3. Run `zsh scripts/release-build.zsh`.
4. Smoke-test by mounting `dist/Rusty.dmg` and dragging `Rusty.app` to `/Applications`.
   Use `pgrep -x rusty` in smoke scripts (process name is lowercase).
5. Publish a GitHub Release and attach `dist/Rusty.dmg`.
6. Tag `vX.Y.Z` to match the Cargo version.

## Versioning Expectations

- Semantic Versioning in `Cargo.toml` / `tauri.conf.json`.
- Keep both files in sync — they are the SSOT for Rusty.

## Troubleshooting

| Symptom | What to try |
|---------|-------------|
| `tauri` CLI missing | `cargo install --root .cargo tauri-cli@^2 --locked` |
| Preflight fails | Read `scripts/release-preflight.zsh` output; fix config before bundling |
| Gatekeeper blocks launch | Right-click → **Open** (ad-hoc signed builds) |
| Wrong process name in tests | Use `rusty`, not `Rusty` |

## Related Docs

- [README.md](README.md)
- [CONTRIBUTING.md](CONTRIBUTING.md)
- [SECURITY.md](SECURITY.md)
- [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md)

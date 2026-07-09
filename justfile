# Selene — development tasks. Run `just` (or `just --list`) to see them.
# Lines prefixed with `-` are allowed to fail (optional frontend steps that
# only exist once the relevant tooling is configured).

# Show available recipes.
default:
    @just --list

# Run the app in development (Vite dev server + Tauri, hot reload).
dev:
    pnpm tauri dev

# Build + launch a debug .app signed with a STABLE self-signed cert (macOS).
# `just dev` ad-hoc-signs the binary with a fresh identity every rebuild, so the
# keychain re-prompts each run and "Always Allow" never sticks. This recipe
# keeps one signing identity across rebuilds, so a single "Always Allow"
# persists. Tradeoff: runs a bundle, so there's NO hot reload — use `just dev`
# for fast iteration and this when the keychain prompt is in your way.
# One-time setup: create a self-signed *Code Signing* certificate named
# "Selene Dev" (Keychain Access → Certificate Assistant → Create a Certificate);
# to use a different name, edit src-tauri/tauri.dev-signed.conf.json.
dev-signed:
    pnpm tauri build --debug --bundles app --config src-tauri/tauri.dev-signed.conf.json
    @just sweep
    open target/debug/bundle/macos/Selene.app

# Reclaim stale build artifacts (old dependency versions + other toolchains)
# WITHOUT touching the current build, so `target/` stops growing without bound.
# Runs automatically at the tail of every build recipe. No-op with a hint if
# cargo-sweep isn't installed (`cargo install cargo-sweep`).
sweep:
    @command -v cargo-sweep >/dev/null 2>&1 \
        && cargo sweep --installed >/dev/null && cargo sweep --time 7 >/dev/null \
        && echo "recycled stale target/ artifacts" \
        || echo "cargo-sweep not installed; skipping target/ recycle (cargo install cargo-sweep)"

# Build the production desktop bundle.
build:
    pnpm install
    pnpm tauri build
    @just sweep

# Build a SIGNED release .app for personal install (macOS).
# Reuses the stable signing identity in tauri.dev-signed.conf.json (that file
# only sets bundle.macOS.signingIdentity, so it's equally valid for release).
# A stable identity means macOS keychain "Always Allow" persists and the app
# launches with no Gatekeeper prompt on this machine. Output:
# target/release/bundle/macos/Selene.app. Install: drag it into /Applications,
# or run `just install-signed`.
# Only the .app is built (--bundles app): the .dmg step (bundle_dmg.sh) drives
# Finder via AppleScript and isn't needed for a personal install.
build-signed:
    pnpm install
    pnpm tauri build --bundles app --config src-tauri/tauri.dev-signed.conf.json
    @just sweep
    open target/release/bundle/macos

# Build the signed release and copy it straight into /Applications.
install-signed: build-signed
    rm -rf "/Applications/Selene.app"
    cp -R target/release/bundle/macos/Selene.app "/Applications/Selene.app"
    @echo "Installed → /Applications/Selene.app"

# Type-check the whole Rust workspace without building.
check:
    cargo check --workspace

# Run all tests (Rust workspace + frontend if present).
test:
    cargo test --workspace
    -pnpm test

# Run only the core data-layer unit tests.
test-core:
    cargo test -p selene-core

# Dockerized-MSSQL integration tests (needs Docker; ephemeral ports, no 1433 clash).
# Scoped to the mssql_integration binary so it doesn't also run the lib's
# `live_keychain_round_trip` test (that one is `#[ignore]`d for manual runs —
# see its doc comment for the command).
test-integration:
    cargo test -p selene-core --features mssql --test mssql_integration -- --ignored

# Lint: clippy with warnings-as-errors (+ frontend lint once configured).
lint:
    cargo clippy --workspace --all-targets -- -D warnings
    -pnpm lint

# Format Rust (+ frontend once configured).
format:
    cargo fmt --all
    -pnpm format

# Set the version and sync it across Cargo.toml, tauri.conf.json, package.json.
# Usage: just version 0.2.0
version new:
    bash ./scripts/sync-version.sh --set {{new}}

# Verify the version is identical across all three manifests.
version-check:
    bash ./scripts/sync-version.sh --check

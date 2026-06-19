#!/usr/bin/env bash
#
# Keep the application version in sync across the three manifests.
# Source of truth: the [workspace.package] version in the root Cargo.toml.
#
#   scripts/sync-version.sh            propagate Cargo.toml version → JSON manifests
#   scripts/sync-version.sh --check    verify all three match (exit 1 on mismatch)
#   scripts/sync-version.sh --set X.Y.Z  set the version in Cargo.toml, then propagate
#
# Uses perl for in-place edits so it works with the BSD tooling on macOS.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CARGO="$ROOT/Cargo.toml"
TAURI="$ROOT/src-tauri/tauri.conf.json"
PKG="$ROOT/package.json"

cargo_version() {
  grep -E '^version[[:space:]]*=' "$CARGO" | head -n1 | sed -E 's/.*"([^"]+)".*/\1/'
}

json_version() { # $1: file
  grep -E '"version"[[:space:]]*:' "$1" | head -n1 \
    | sed -E 's/.*"version"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/'
}

set_cargo_version() { # $1: version
  perl -i -pe 'if (!$seen && s/^version\s*=\s*"[^"]*"/version = "'"$1"'"/) { $seen = 1 }' "$CARGO"
}

set_json_version() { # $1: file, $2: version
  perl -i -pe 'if (!$seen && s/"version"\s*:\s*"[^"]*"/"version": "'"$2"'"/) { $seen = 1 }' "$1"
}

cmd="${1:-}"
case "$cmd" in
  --set)
    ver="${2:-}"
    [ -n "$ver" ] || { echo "usage: sync-version.sh --set X.Y.Z" >&2; exit 2; }
    set_cargo_version "$ver"
    ;;
  --check | "") : ;;
  *) echo "usage: sync-version.sh [--check | --set X.Y.Z]" >&2; exit 2 ;;
esac

V="$(cargo_version)"
[ -n "$V" ] || { echo "could not read [workspace.package] version from $CARGO" >&2; exit 1; }

if [ "$cmd" = "--check" ]; then
  tv="$(json_version "$TAURI")"
  pv="$(json_version "$PKG")"
  if [ "$V" = "$tv" ] && [ "$V" = "$pv" ]; then
    echo "version in sync: $V"
    exit 0
  fi
  echo "version mismatch — Cargo.toml=$V  tauri.conf.json=$tv  package.json=$pv" >&2
  exit 1
fi

set_json_version "$TAURI" "$V"
set_json_version "$PKG" "$V"
echo "synced version $V -> tauri.conf.json, package.json"

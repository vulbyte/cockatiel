#!/usr/bin/env bash
#
# doctor-gate.sh — assemble an install root from the freshly built release
# binaries and run `cockatiel doctor --quick` against it in both pipeline modes.
# This is the release gate: a platform's artifacts are not published unless a
# real engine + user-db + test-runner boot and the compliance suites pass here.
#
# Usage: scripts/doctor-gate.sh [--soak]
#
# Runs on the current machine from the monorepo root; requires the release
# binaries to already be built (scripts/package-release.sh builds them).

set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SOAK=""
[[ "${1:-}" == "--soak" ]] && SOAK="--soak"

cargo build --release --manifest-path "$ROOT/modules/cockatiel_module-launcher-rs/Cargo.toml"
LAUNCHER="$ROOT/modules/cockatiel_module-launcher-rs/target/release/cockatiel"

TMP="$(mktemp -d "${TMPDIR:-/tmp}/cockatiel-doctor-gate.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

# Installed layout: <root>/engine, <root>/user-db, <root>/bin, <root>/modules.
mkdir -p "$TMP/root/engine" "$TMP/root/user-db" "$TMP/root/bin" "$TMP/root/modules"

link_bin() { # <component-dir> <binary-name> <target-subdir>
  local dir="$ROOT/$1" bin="$2" dest="$TMP/root/$3"
  local path="$dir/target/release/$bin"
  [[ -f "$path" ]] || path="$path.exe"
  [[ -f "$path" ]] || { printf 'error: missing %s (run scripts/package-release.sh first)\n' "$path" >&2; exit 1; }
  # Copy, not symlink: the native Windows launcher does not resolve a Git Bash
  # (MSYS) symlink, so a symlinked install root is invisible to `doctor` there.
  cp -f "$path" "$dest/"
}

link_bin cockatiel_engine-rs        cockatiel-engine-rs   engine
link_bin cockatiel_engine-rs/modules/cockatiel_user_database-rs cockatiel-user-database user-db
link_bin modules/cockatiel_module-test_runner-rs cockatiel-test-runner bin
[[ -f "$ROOT/rank_chart.json" ]] && cp "$ROOT/rank_chart.json" "$TMP/root/rank_chart.json"

exec "$LAUNCHER" doctor --root "$TMP/root" --quick --mode both $SOAK

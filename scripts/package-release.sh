#!/usr/bin/env bash
#
# package-release.sh — build + package the Cockatiel core components for the
# CURRENT platform into <out-dir>, computing each archive's SHA-256 and writing
# it back into that component's `cockatiel_module_info.json` (`release.assets`).
#
# This is the primitive `.github/workflows/release.yml` runs on each matrix
# runner; it is also runnable locally to produce a release directory for the
# machine you are on.
#
# Usage: scripts/package-release.sh <version> <out-dir>
#
# The launcher (`cockatiel package`) decides the archive name + format from the
# component's manifest, so the file the launcher later looks for and the file
# published here are guaranteed to match.

set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION="${1:?usage: package-release.sh <version> <out-dir>}"
OUT="${2:?usage: package-release.sh <version> <out-dir>}"

mkdir -p "$OUT"

# Build the launcher (release) — it is the packaging tool.
cargo build --release --manifest-path "$ROOT/cockatiel_launcher-rs/Cargo.toml"
LAUNCHER="$ROOT/cockatiel_launcher-rs/target/release/cockatiel"

# component-dir | binary-name (the release binary lives at <dir>/target/release/<bin>)
COMPONENTS=(
  "cockatiel_engine-rs|cockatiel-engine-rs"
  "cockatiel_user_database-rs|cockatiel-user-database"
  "cockatiel_tui_v2-rs|cockatiel-tui-v2"
  "cockatiel_test_runner-rs|cockatiel-test-runner"
)

for entry in "${COMPONENTS[@]}"; do
  dir="${entry%%|*}"
  bin="${entry##*|}"
  manifest="$ROOT/$dir/cockatiel_module_info.json"

  if [[ ! -f "$manifest" ]]; then
    printf 'error: missing manifest %s\n' "$manifest" >&2
    exit 1
  fi

  printf '==> building %s\n' "$dir"
  cargo build --release --manifest-path "$ROOT/$dir/Cargo.toml"

  binpath="$ROOT/$dir/target/release/$bin"
  if [[ ! -f "$binpath" && -f "$binpath.exe" ]]; then
    binpath="$binpath.exe"
  fi
  if [[ ! -f "$binpath" ]]; then
    printf 'error: built binary not found: %s\n' "$binpath" >&2
    exit 1
  fi

  "$LAUNCHER" package \
    --manifest "$manifest" \
    --binary "$binpath" \
    --out-dir "$OUT" \
    --version "$VERSION" \
    --write

  # Carry the patched manifest alongside the archive so the publish job can
  # merge each platform's asset entry into the component's checked-in manifest.
  cp "$manifest" "$OUT/$dir.manifest.json"
done

printf '\nrelease directory: %s\n' "$OUT"
ls -1 "$OUT"

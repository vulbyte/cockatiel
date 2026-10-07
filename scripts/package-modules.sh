#!/usr/bin/env bash
#
# package-modules.sh — build + package every Rust module in `modules/` for the
# current (or cross) platform, writing each archive's SHA-256 back into the
# module's manifest. Python-only modules (no cargo build) are skipped; pass
# SKIP_MODULES="name-a name-b" to skip specific modules (e.g. tts-rs on 32-bit,
# where sherpa-onnx has no prebuilt).
#
# Usage: [COCKATIEL_BUILD_TARGET=triple] [COCKATIEL_CARGO=cross] \
#          scripts/package-modules.sh <version> <out-dir>

set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION="${1:?usage: package-modules.sh <version> <out-dir>}"
OUT="${2:?usage: package-modules.sh <version> <out-dir>}"
mkdir -p "$OUT"

TARGET="${COCKATIEL_BUILD_TARGET:-}"
CARGO_BIN="${COCKATIEL_CARGO:-cargo}"

# Same platform derivation as package-release.sh (kept in sync deliberately).
if [[ -n "$TARGET" ]]; then
  case "$TARGET" in
    *apple-darwin*) PLATFORM_OS=macos ;;
    *windows*)      PLATFORM_OS=windows ;;
    *)              PLATFORM_OS=linux ;;
  esac
  case "$TARGET" in
    x86_64*)         PLATFORM_ARCH=x86_64 ;;
    aarch64*|arm64*) PLATFORM_ARCH=aarch64 ;;
    i686*|i586*)     PLATFORM_ARCH=x86 ;;
    arm*)            PLATFORM_ARCH=arm ;;
    *)               PLATFORM_ARCH=x86_64 ;;
  esac
else
  case "$(uname -s)" in
    Darwin) PLATFORM_OS=macos ;;
    MINGW*|MSYS*|CYGWIN*) PLATFORM_OS=windows ;;
    *) PLATFORM_OS=linux ;;
  esac
  case "$(uname -m)" in
    arm64|aarch64) PLATFORM_ARCH=aarch64 ;;
    x86_64|amd64)  PLATFORM_ARCH=x86_64 ;;
    i686|i386)     PLATFORM_ARCH=x86 ;;
    armv7l|arm*)   PLATFORM_ARCH=arm ;;
    *)             PLATFORM_ARCH=x86_64 ;;
  esac
fi
PLATFORM="$PLATFORM_OS-$PLATFORM_ARCH"
SKIP_MODULES="${SKIP_MODULES:-}"

cargo build --release --manifest-path "$ROOT/cockatiel_launcher-rs/Cargo.toml"
LAUNCHER="$ROOT/cockatiel_launcher-rs/target/release/cockatiel"

fail=0
for dir in "$ROOT"/modules/*/; do
  manifest="$dir/cockatiel_module_info.json"
  [[ -f "$manifest" ]] || continue
  name=$(python3 -c "import json;print(json.load(open('$manifest'))['name'])")
  build_cmd=$(python3 -c "import json;print(json.load(open('$manifest')).get('build_command') or '')")

  if [[ "$build_cmd" != "cargo" ]]; then
    printf 'skip %s (non-cargo build)\n' "$name"
    continue
  fi
  skipped=0
  for s in $SKIP_MODULES; do
    [[ "$name" == "$s" ]] && skipped=1 && break
  done
  if [[ "$skipped" == 1 ]]; then
    printf 'skip %s (in SKIP_MODULES)\n' "$name"
    continue
  fi

  printf '==> building %s\n' "$name"
  if ! "$CARGO_BIN" build --release ${TARGET:+--target "$TARGET"} --manifest-path "$dir/Cargo.toml"; then
    printf 'error: build failed for %s\n' "$name" >&2
    fail=1
    continue
  fi

  bin=$(python3 -c "import json;b=json.load(open('$manifest')).get('binary',{}).get('$PLATFORM_OS',{}).get('$PLATFORM_ARCH');print(b or '')")
  rel="release"
  [[ -n "$TARGET" ]] && rel="$TARGET/release"
  binpath="$dir/target/$rel/$bin"
  if [[ -z "$bin" || ! -f "$binpath" ]]; then
    printf 'error: built binary not found for %s (%s)\n' "$name" "$binpath" >&2
    fail=1
    continue
  fi

  "$LAUNCHER" package \
    --manifest "$manifest" \
    --binary "$binpath" \
    --out-dir "$OUT" \
    --version "$VERSION" \
    --os "$PLATFORM_OS" \
    --arch "$PLATFORM_ARCH" \
    --write
  cp "$manifest" "$OUT/$(basename "$dir").$PLATFORM.manifest.json"
done

printf '\nmodules for %s in %s\n' "$PLATFORM" "$OUT"
ls -1 "$OUT"
exit "$fail"

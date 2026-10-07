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

# Optional cross target, e.g. COCKATIEL_BUILD_TARGET=x86_64-apple-darwin. The
# launcher (packaging tool) is still built natively; only the components cross.
TARGET="${COCKATIEL_BUILD_TARGET:-}"

# Build driver: `cargo` by default, or `cross` for targets that need a
# containerized toolchain (e.g. linux armv7). The launcher is always built with
# the host `cargo` (it is a host tool).
CARGO_BIN="${COCKATIEL_CARGO:-cargo}"

# The release platform (matches the launcher's os/arch keys), derived from the
# cross target when set, else from the host.
if [[ -n "$TARGET" ]]; then
  case "$TARGET" in
    *apple-darwin*) PLATFORM_OS=macos ;;
    *windows*)      PLATFORM_OS=windows ;;
    *)              PLATFORM_OS=linux ;;
  esac
  case "$TARGET" in
    x86_64*)            PLATFORM_ARCH=x86_64 ;;
    aarch64*|arm64*)    PLATFORM_ARCH=aarch64 ;;
    i686*|i586*)        PLATFORM_ARCH=x86 ;;
    arm*)               PLATFORM_ARCH=arm ;;
    *)                  PLATFORM_ARCH=x86_64 ;;
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
  # Git Bash's `-f` matches `foo.exe` for `foo`, so the `.exe` fallback below
  # never fires there and the native launcher gets an extensionless path. Make
  # the suffix explicit on Windows.
  [[ "$PLATFORM_OS" == windows && "$bin" != *.exe ]] && bin="$bin.exe"
  manifest="$ROOT/$dir/cockatiel_module_info.json"

  if [[ ! -f "$manifest" ]]; then
    printf 'error: missing manifest %s\n' "$manifest" >&2
    exit 1
  fi

  printf '==> building %s\n' "$dir"
  "$CARGO_BIN" build --release ${TARGET:+--target "$TARGET"} --manifest-path "$ROOT/$dir/Cargo.toml"

  # With a cross target the binary lands under target/<triple>/release/.
  rel="release"
  [[ -n "$TARGET" ]] && rel="$TARGET/release"
  binpath="$ROOT/$dir/target/$rel/$bin"
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
    --os "$PLATFORM_OS" \
    --arch "$PLATFORM_ARCH" \
    --write

  # Carry the patched manifest alongside the archive so the publish job can
  # merge each platform's asset entry into the component's checked-in manifest.
  # The name is per-platform so artifacts from different runners never collide
  # when merged.
  cp "$manifest" "$OUT/$dir.$PLATFORM.manifest.json"
done

printf '\nrelease directory: %s\n' "$OUT"
ls -1 "$OUT"

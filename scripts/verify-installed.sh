#!/usr/bin/env bash
#
# verify-installed.sh — clean-room proof that a release actually installs and
# runs: install the core components into a FRESH root and run `cockatiel
# doctor` against it (both pipeline modes).
#
# By default the install downloads from the published release URLs recorded in
# the component manifests — the real user path. Pass a local release directory
# to test artifacts you just built instead (the tag subdirectory is arranged
# automatically).
#
# Usage: scripts/verify-installed.sh <version> [local-release-dir]

set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION="${1:?usage: verify-installed.sh <version> [local-release-dir]}"
LOCAL="${2:-}"

cargo build --release --manifest-path "$ROOT/cockatiel_launcher-rs/Cargo.toml"
LAUNCHER="$ROOT/cockatiel_launcher-rs/target/release/cockatiel"

TMP="$(mktemp -d "${TMPDIR:-/tmp}/cockatiel-verify.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT

INSTALL_ARGS=(
  install
  --lock "$ROOT/cockatiel.lock"
  --repo-root "$ROOT"
  --root "$TMP/root"
  --component engine --component tui --component user-db --component test-runner
)

if [[ -n "$LOCAL" ]]; then
  # Mirror the layout the resolver expects: <base>/<tag>/<file>.
  tagdir="$LOCAL/v$VERSION"
  mkdir -p "$tagdir"
  cp "$LOCAL"/*.tar.gz "$tagdir/" 2>/dev/null || true
  cp "$LOCAL"/*.zip "$tagdir/" 2>/dev/null || true
  INSTALL_ARGS+=(--release-base "file://$LOCAL")
  printf '==> installing from local release %s\n' "$LOCAL"
else
  printf '==> installing from published release v%s\n' "$VERSION"
fi

"$LAUNCHER" "${INSTALL_ARGS[@]}"

printf '\n==> installed layout\n'
find "$TMP/root" -type f | sed "s#$TMP/root/##"

printf '\n==> doctor (both pipeline modes)\n'
exec "$LAUNCHER" doctor --root "$TMP/root" --quick --mode both

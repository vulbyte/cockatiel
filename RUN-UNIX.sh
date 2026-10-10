#!/usr/bin/env bash
#
# RUN-UNIX.sh — one-click build + launch for Cockatiel on macOS and Linux.
#
# macOS: double-click RUN-MAC.command (Finder).
# Linux / macOS: run this file directly (./RUN-UNIX.sh) — RUN-LINUX.sh is a
# thin wrapper with the same behaviour.
#
# It builds the engine, the user database and the TUI from THIS repository,
# then launches the TUI. Everything stays inside this folder. Rust is required
# to build: if it is missing the script offers to install it, and declining
# simply exits (nothing else happens).

set -euo pipefail

# ── Locate this repo from the script's own path ───────────────────────────
SOURCE="${BASH_SOURCE[0]}"
while [ -h "$SOURCE" ]; do
  DIR="$(cd -P "$(dirname "$SOURCE")" >/dev/null 2>&1 && pwd)"
  SOURCE="$(readlink "$SOURCE")"
  case "$SOURCE" in
    /*) ;;
    *) SOURCE="$DIR/$SOURCE" ;;
  esac
done
REPO_DIR="$(cd -P "$(dirname "$SOURCE")" >/dev/null 2>&1 && pwd)"

pause() {
  # Only pause when there is a terminal to read from (a double-click gives one;
  # a piped/redirected run does not).
  if [ -t 0 ]; then
    printf '\nPress Enter to close this window...'
    read -r _ || true
  fi
}

# Pause on any failure so a double-click user can actually read the error.
on_exit() {
  local code=$?
  if [ "$code" -ne 0 ]; then
    printf '\nCockatiel did not start (exit code %s).\n' "$code" >&2
    pause
  fi
}
trap on_exit EXIT

die() { printf '\n%s\n' "$1" >&2; exit 1; }

# ── Verify we are in the repo root ────────────────────────────────────────
printf 'Cockatiel one-click launcher\n'
printf 'Repository: %s\n\n' "$REPO_DIR"
if [ ! -f "$REPO_DIR/cockatiel.lock" ] \
  || [ ! -d "$REPO_DIR/cockatiel_engine-rs" ] \
  || [ ! -d "$REPO_DIR/modules" ]; then
  die "This launcher must sit in the Cockatiel repository root (it expects cockatiel.lock, cockatiel_engine-rs/ and modules/ next to it)."
fi
cd "$REPO_DIR"

# ── Submodules (a plain clone without --recurse-submodules leaves these empty)
if [ ! -f "cockatiel_engine-rs/Cargo.toml" ] \
  || [ ! -f "modules/cockatiel_module-tui_v2-rs/Cargo.toml" ]; then
  printf 'Fetching submodules (first run)...\n'
  command -v git >/dev/null 2>&1 \
    || die "The repo's submodules are missing and git is not installed. Install git, then run this launcher again."
  git submodule update --init --recursive
fi

# ── Rust (optional; offered, never forced) ────────────────────────────────
if ! command -v cargo >/dev/null 2>&1; then
  printf 'Rust (cargo) is needed to build Cockatiel and is not installed.\n'
  printf 'It is free and takes a couple of minutes: https://rustup.rs\n\n'
  printf 'Install Rust now via rustup? [y/N] '
  read -r answer || answer=""
  case "$answer" in
    [yY] | [yY][eE][sS])
      printf '\nInstalling Rust (rustup)...\n'
      if command -v curl >/dev/null 2>&1; then
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
      elif command -v wget >/dev/null 2>&1; then
        wget -qO- https://sh.rustup.rs | sh -s -- -y
      else
        die "Neither curl nor wget is available. Install Rust from https://rustup.rs, then run this launcher again."
      fi
      # Make cargo usable in THIS shell without reopening the terminal.
      if [ -f "$HOME/.cargo/env" ]; then
        # shellcheck disable=SC1090
        . "$HOME/.cargo/env"
      fi
      ;;
    *)
      printf '\nNo problem. Install Rust from https://rustup.rs, then run this launcher again.\n'
      exit 0
      ;;
  esac
fi
if ! command -v cargo >/dev/null 2>&1; then
  die "cargo is still not on PATH after installing Rust. Open a new terminal (or run 'source \"\$HOME/.cargo/env\"'), then run this launcher again."
fi

# ── Help the engine build find the RIGHT pkg-config on macOS ──────────────
# The engine's build links libgit2, which it locates through pkg-config. On a
# Mac that also has a Homebrew of the OTHER architecture on PATH (a very common
# Apple-Silicon + Rosetta-Intel-Homebrew setup) the wrong libgit2 is picked and
# the link fails with an architecture mismatch. So choose the Homebrew prefix
# that MATCHES the Rust host architecture, not whatever `brew` resolves to.
if [ "$(uname -s)" = "Darwin" ]; then
  host_arch="$(rustc -vV 2>/dev/null | awk '/^host:/{print $2}')"
  case "$host_arch" in
    aarch64-*) brew_prefix="/opt/homebrew" ;;
    x86_64-*)  brew_prefix="/usr/local" ;;
    *)         brew_prefix="$(command -v brew >/dev/null 2>&1 && brew --prefix 2>/dev/null || true)" ;;
  esac
  if [ -n "${brew_prefix:-}" ] && [ -d "$brew_prefix/lib/pkgconfig" ]; then
    export PKG_CONFIG_PATH="$brew_prefix/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
  fi
  export PKG_CONFIG_ALLOW_CROSS=1
fi

# ── Build the core (release) ──────────────────────────────────────────────
printf '\nBuilding Cockatiel (release). The first build takes several minutes.\n'
cargo build --release --manifest-path "cockatiel_engine-rs/Cargo.toml"
cargo build --release --manifest-path "cockatiel_engine-rs/modules/cockatiel_user_database-rs/Cargo.toml"
cargo build --release --manifest-path "modules/cockatiel_module-tui_v2-rs/Cargo.toml"

# ── Launch ────────────────────────────────────────────────────────────────
TUI="modules/cockatiel_module-tui_v2-rs/target/release/cockatiel-tui-v2"
[ -x "$TUI" ] || die "The TUI binary was not produced at $TUI."
printf '\nLaunching Cockatiel...\n\n'
"$TUI" "$@"

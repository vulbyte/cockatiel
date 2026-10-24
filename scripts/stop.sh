#!/usr/bin/env bash
#
# stop.sh — stop the Cockatiel stack WITHOUT kill -9.
#
# A hard kill can tear a turso btree write and leave the timeline DB corrupt but
# still openable. Always stop the stack with this script (or the TUI's own quit)
# before a repair/restart. It sends SIGTERM first, waits up to GRACE seconds for
# every process to exit, and only then SIGKILLs whatever is left.
#
# usage: scripts/stop.sh [--dry-run] [grace_seconds]

set -uo pipefail

DRY_RUN=0
if [ "${1:-}" = "--dry-run" ]; then
  DRY_RUN=1
  shift
fi
GRACE="${1:-10}"
if ! [ "$GRACE" -ge 0 ] 2>/dev/null; then
  echo "Invalid grace period '$GRACE' — using 10 seconds." >&2
  GRACE=10
fi

# The binaries that make up a running stack. The module pattern covers every
# module binary (including event_brain) under modules/cockatiel_module-*/.
PATTERNS=(
  "cockatiel-tui-v2"
  "cockatiel-engine-rs"
  "cockatiel-user-database"
  "modules/cockatiel_module-.*/target/release"
  "event_brain"
)

SELF=$$

# A terminal emulator (or its login/shell wrapper) can have a module path in its
# command line without being the module itself. Do not kill the operator's
# terminal window just because it is hosting an events/term-chat pane.
is_wrapper() {
  case "$(basename "$(ps -p "$1" -o comm= 2>/dev/null)")" in
    sh|bash|zsh|dash|login|tmux|screen|ghostty|cool-retro-term|iTerm2|Terminal|alacritty|kitty|wezterm)
      return 0
      ;;
    *) return 1 ;;
  esac
}

collect_pids() {
  local pat pids pid
  for pat in "${PATTERNS[@]}"; do
    pids="$(pgrep -f "$pat" 2>/dev/null || true)"
    for pid in $pids; do
      [ "$pid" = "$SELF" ] && continue
      is_wrapper "$pid" && continue
      printf '%s\n' "$pid"
    done
  done | sort -u
}

describe() {
  local pid
  for pid in "$@"; do
    ps -p "$pid" -o pid=,command= 2>/dev/null || true
  done
}

PIDS="$(collect_pids)"
if [ -z "$PIDS" ]; then
  echo "No Cockatiel processes found — nothing to stop."
  exit 0
fi

echo "Sending SIGTERM to:"
# shellcheck disable=SC2086
describe $PIDS

if [ "$DRY_RUN" = 1 ]; then
  echo "Dry run: not sending any signals."
  exit 0
fi

# shellcheck disable=SC2086
kill -TERM $PIDS 2>/dev/null || true

for _ in $(seq 1 "$GRACE"); do
  REMAINING=""
  for pid in $PIDS; do
    if kill -0 "$pid" 2>/dev/null; then
      REMAINING="$REMAINING $pid"
    fi
  done
  if [ -z "$REMAINING" ]; then
    echo "All Cockatiel processes stopped gracefully."
    exit 0
  fi
  sleep 1
done

echo "Still running after ${GRACE}s — sending SIGKILL to:"
# shellcheck disable=SC2086
describe $REMAINING
# shellcheck disable=SC2086
kill -KILL $REMAINING 2>/dev/null || true

sleep 1
LEFT=""
for pid in $REMAINING; do
  kill -0 "$pid" 2>/dev/null && LEFT="$LEFT $pid"
done
if [ -n "$LEFT" ]; then
  echo "WARNING: these processes survived SIGKILL:"
  # shellcheck disable=SC2086
  describe $LEFT
  exit 1
fi

echo "All Cockatiel processes stopped."

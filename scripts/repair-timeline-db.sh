#!/usr/bin/env bash
#
# repair-timeline-db.sh — repair a corrupt Cockatiel timeline database offline.
#
# The timeline DB (`cockatiel_data.db`) can be corrupted by a turso 0.1.5 btree
# bug (an interior page referenced twice). A full table scan then aborts, so the
# hourly noise-cull DELETE fails (`Corrupt database: IdxDelete ...`) and the file
# grows without bound. turso cannot repair a corrupt btree; sqlite3's `.recover`
# can — it walks the file page-by-page, tolerating the damage, and emits a clean
# SQL dump.
#
# Run this with the engine STOPPED. It backs the original up (never deletes it)
# and only swaps the recovered file in once a full `integrity_check` passes.
#
# usage: scripts/repair-timeline-db.sh [path/to/cockatiel_data.db]

set -euo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DB="${1:-$REPO_DIR/cockatiel_engine-rs/cockatiel_data.db}"

if ! command -v sqlite3 >/dev/null 2>&1; then
  echo "sqlite3 is required (it ships with macOS)." >&2
  exit 1
fi
if [ ! -f "$DB" ]; then
  echo "No database at: $DB" >&2
  exit 1
fi

# Refuse to run while the engine is up: a running engine holds the DB open and
# a race would corrupt the swap. The empty-WAL check alone is not enough — the
# WAL can be 0 bytes at any moment.
if pgrep -f "cockatiel-engine-rs" >/dev/null 2>&1; then
  echo "The engine is running (pgrep cockatiel-engine-rs) — stop it before repairing." >&2
  exit 1
fi
if [ -s "$DB-wal" ]; then
  echo "The WAL sidecar is non-empty — stop the engine before repairing." >&2
  exit 1
fi

BEFORE="$(sqlite3 "$DB" 'PRAGMA quick_check;' 2>&1 | head -1)"
echo "integrity before: $BEFORE"
if [ "$BEFORE" = "ok" ]; then
  echo "Database is already clean — nothing to do."
  exit 0
fi

STAMP="$(date +%Y%m%d-%H%M%S)"
TMP="$DB.recovered.$STAMP"
echo "Recovering into $TMP ..."
sqlite3 "$DB" ".recover" | sqlite3 "$TMP"

AFTER="$(sqlite3 "$TMP" 'PRAGMA integrity_check;' 2>&1 | head -1)"
if [ "$AFTER" != "ok" ]; then
  echo "Recovery produced a still-corrupt file ($AFTER); leaving the original in place." >&2
  rm -f "$TMP"
  exit 1
fi

BACKUP="$DB.corrupt.$STAMP"
echo "integrity after:  ok"
echo "Backing the original up to $BACKUP"
mv "$DB" "$BACKUP"
mv "$TMP" "$DB"
rm -f "$DB-wal" "$DB-shm"

echo "Repaired. Restart Cockatiel. (The corrupt original is at $BACKUP; delete it once you're happy.)"

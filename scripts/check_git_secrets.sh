#!/usr/bin/env bash
#
# check_git_secrets.sh — fail if this git repo tracks (or is about to commit)
# anything that must never be uploaded: .env files, databases, or runtime
# config files that hold secrets (tokens/oauth/PIN).
#
# Usage (run from inside any git repo):
#   ./check_git_secrets.sh          # check everything git tracks
#   ./check_git_secrets.sh --staged # check only what's staged (pre-commit)
#
# Exit code 0 = clean, 1 = violations found, 2 = not a git repo / usage error.
#
set -Eeuo pipefail

# ── Patterns ────────────────────────────────────────────────────────────────
# These are matched against file PATHS. Add/remove patterns here; keep them
# POSIX-ERE so they work with grep -E on macOS and Linux.
PATTERNS=(
  '(^|/)\.env($|\.)'                      # .env, .env.local, .env.production …
  '[^/]*\.(db|db-wal|db-shm|db-journal|sqlite|sqlite3)$'   # databases
  '(^|/)(config|modules|login|chat_config)\.json$'          # secret-bearing configs
  '(^|/)term-chat-rs\.json$'                                 # term-chat runtime config
  '(^|/)user_data_backup\.db$'                              # user-db snapshot
  '(^|/)tls/'                                                 # engine TLS identity dir
  '[^/]*\.(pem|key)$'                                        # TLS certs / private keys
)

main() {
  local mode="all"
  if [[ "${1:-}" == "--staged" ]]; then
    mode="staged"
  elif [[ -n "${1:-}" ]]; then
    printf 'usage: %s [--staged]\n' "$0" >&2
    exit 2
  fi

  # Must be inside a git work tree.
  git rev-parse --is-inside-work-tree >/dev/null 2>&1 || {
    printf 'error: not a git work tree\n' >&2
    exit 2
  }

  local files
  if [[ "$mode" == "staged" ]]; then
    # Pre-commit: only paths that would actually enter the index.
    files="$(git diff --cached --name-only --diff-filter=ACMR)"
  else
    # Tracked inventory + anything already staged.
    files="$(git ls-files)"
    files="${files}"$'\n'"$(git diff --cached --name-only --diff-filter=ACMR)"
  fi

  # A path can appear twice (tracked + staged); report each once.
  files="$(printf '%s\n' "$files" | sort -u)"

  local violations=()
  local path
  while IFS= read -r path; do
    [[ -n "$path" ]] || continue
    for pattern in "${PATTERNS[@]}"; do
      if printf '%s\n' "$path" | grep -Eq "$pattern"; then
        violations+=("$path")
        break
      fi
    done
  done <<<"$files"

  if [[ ${#violations[@]} -gt 0 ]]; then
    printf 'BLOCKED: %d file(s) must never be committed/uploaded (secrets or databases):\n' \
      "${#violations[@]}" >&2
    printf '  %s\n' "${violations[@]}" >&2
    printf '\nRemove them from tracking with `git rm --cached <path>` and keep\n' >&2
    printf 'them out of the repo. See .gitignore — runtime config, .env and .db\n' >&2
    printf 'files are ignored for a reason.\n' >&2
    exit 1
  fi

  printf 'OK: no secrets or databases in tracked/staged files\n'
  exit 0
}

main "$@"